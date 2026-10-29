//! openfront-video: turns an OpenFront.io game into a sped-up MP4, on its own.
//!
//! It fetches the game record, replays it with the Rust engine port, draws
//! the frames like render.ts and encodes them with OpenH264. Maps, sprites,
//! the font and the theme are built in (see build.rs).
//!
//! Three threads: the simulation (this one), drawing and encoding. The
//! simulation hands frames to the drawing thread through a queue of
//! QUEUE_FRAMES and waits whenever it is full, so it never runs further ahead
//! than that.

mod assets;
mod draw;
mod encode;
mod fetch;
mod look;

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_rs::replay::{self, setup};
use serde_json::Value;

use draw::{Label, Renderer};
use look::{Settings, Style, Theme};

/// Frames the simulation may queue up for drawing before it waits.
const QUEUE_FRAMES: usize = 10;
/// Drawn frames waiting for the encoder.
const ENCODE_QUEUE: usize = 4;

const USAGE: &str = "\
openfront-video: turn an OpenFront.io game into a sped-up MP4

Usage:
  openfront-video <game link | game ID | record.json[.gz]> [options]

Options:
  --out <file>       output video (default: <gameID>.mp4)
  --seconds <n>      length of the timelapse, before the end hold (default: 30)
  --fps <n>          frames per second (default: 30)
  --width <n>        video width in pixels, height follows the map (default: 1280)
  --hold <n>         seconds to hold the final frame (default: 2)
  --bitrate <n>      target bitrate in Mbit/s (default: 6)
  --threads <n>      encoder threads (default: 4)
  --strict           stop if the replay desyncs from the archived hashes
  --same-engine      stop right away if the game was played on another engine version
  --thumbnail <file> also save the last frame as a PNG
  --meta <file>      also write the game's and video's details as JSON
";

struct Options {
    input: String,
    out: Option<String>,
    seconds: f64,
    fps: u32,
    width: u32,
    hold: f64,
    bitrate: f64,
    threads: u16,
    strict: bool,
    same_engine: bool,
    thumbnail: Option<String>,
    meta: Option<String>,
}

fn parse_args() -> Result<Options, String> {
    let mut o = Options {
        input: String::new(),
        out: None,
        seconds: 30.0,
        fps: 30,
        width: 1280,
        hold: 2.0,
        bitrate: 6.0,
        threads: 4,
        strict: false,
        same_engine: false,
        thumbnail: None,
        meta: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("missing value for {arg}"));
        let num = |v: String| -> Result<f64, String> {
            v.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0).ok_or_else(|| format!("bad number for {arg}"))
        };
        match arg.as_str() {
            "--out" => o.out = Some(value()?),
            "--seconds" => o.seconds = num(value()?)?,
            "--fps" => o.fps = num(value()?)?.round().max(1.0) as u32,
            "--width" => o.width = num(value()?)? as u32,
            "--hold" => o.hold = num(value()?)?,
            "--bitrate" => o.bitrate = num(value()?)?,
            "--threads" => o.threads = num(value()?)?.round().max(1.0) as u16,
            "--strict" => o.strict = true,
            "--same-engine" => o.same_engine = true,
            "--thumbnail" => o.thumbnail = Some(value()?),
            "--meta" => o.meta = Some(value()?),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            _ if arg.starts_with("--") => return Err(format!("unknown option {arg}")),
            _ if !o.input.is_empty() => return Err(format!("unexpected argument {arg}")),
            _ => o.input = arg,
        }
    }
    if o.input.is_empty() {
        return Err("give a game link, game ID or record file (see --help)".into());
    }
    Ok(o)
}

/// What the drawing thread gets per simulated frame.
struct DrawMsg {
    progress: f64,
    styles: Vec<(u16, Style)>,
    tiles: Vec<u32>,
    units: Arc<Vec<i32>>,
    labels: Arc<Vec<Label>>,
    /// How many video frames show it.
    repeat: u32,
}

/// When a thread did its work, for the timeline.
#[derive(Default, Clone, Copy)]
struct Span {
    start: Option<Instant>,
    end: Option<Instant>,
    busy: Duration,
    waited: Duration,
}

fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn secs(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64())
}

fn main() {
    // Engine "not ported yet" stops and other failures: one clean line.
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown error".into());
        // One line, so whoever reads the last line (the web service) gets all of it:
        // "a custom nation count is not ported yet (left: Number(400), right: "default")".
        let msg = msg.trim_start_matches("assertion `left == right` failed: ");
        let mut lines = msg.lines().map(str::trim).filter(|l| !l.is_empty());
        let first = lines.next().unwrap_or("unknown error").to_string();
        let rest: Vec<&str> = lines.collect();
        let msg = if rest.is_empty() { first } else { format!("{first} ({})", rest.join(", ")) };
        eprintln!("\nopenfront-video stopped: {msg}");
        std::process::exit(1);
    }));
    if let Err(e) = run() {
        eprintln!("error: {e}");
        // Exit code 3: the API has no such game (yet). The web service treats
        // that as "not finished" rather than as a failure.
        std::process::exit(if e.starts_with("game ") && e.contains(" not found (") { 3 } else { 1 });
    }
}

fn run() -> Result<(), String> {
    let opts = parse_args()?;
    let t0 = Instant::now();

    // 1. The record
    println!("Fetching replay {} ...", opts.input);
    let record = fetch::load_record(&opts.input)?;
    let t_fetch = t0.elapsed();
    let info = &record["info"];
    let game_id = info["gameID"].as_str().unwrap_or("game").to_string();
    let num_turns = info["num_turns"].as_u64().unwrap_or(0);
    let duration = info["duration"].as_f64().unwrap_or(0.0);
    println!(
        "  {}: {}, {}, {} players, {} ticks ({} min of play)",
        game_id,
        info["config"]["gameMap"].as_str().unwrap_or("?"),
        info["config"]["gameMode"].as_str().unwrap_or("?"),
        info["players"].as_array().map_or(0, |a| a.len()),
        fmt_int(num_turns),
        (duration / 60.0).round()
    );
    let commit = assets::ENGINE_COMMIT;
    if let Some(played) = record["gitCommit"].as_str() {
        let known = replay::ENGINE_COMMITS.contains(&played) || played == commit;
        if played != "DEV" && !known {
            if opts.same_engine {
                return Err(format!(
                    "played on engine version {}, this build follows {}",
                    &played[..played.len().min(9)],
                    &commit[..commit.len().min(9)]
                ));
            }
            eprintln!(
                "! this game was played on commit {}, but this build follows {}. The replay will probably desync.",
                &played[..played.len().min(12)],
                &commit[..commit.len().min(12)]
            );
        }
    }

    // 2. The game and the look
    let t_init0 = Instant::now();
    let (mut game, turns) = setup(&record, &assets::Embedded);
    let archived = replay::archived_hashes(&record);
    let (w, h) = (game.map.width as usize, game.map.height as usize);
    let terrain: Vec<u8> = (0..(w * h) as u32).map(|r| game.map.terrain_byte(r)).collect();
    let render_settings: Value = serde_json::from_str(assets::RENDER_SETTINGS).unwrap();
    let mut theme = Theme::new(&serde_json::from_str(assets::THEME).unwrap());
    let settings = Settings::new(&render_settings);
    let cosmetics: HashMap<String, Value> = info["players"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["clientID"].as_str()?.to_string(), p["cosmetics"].clone())))
        .collect();

    let frames = ((opts.seconds * opts.fps as f64).round() as u64).max(1);
    let hold_frames = (opts.hold * opts.fps as f64).round() as u64;
    // The requested width, scaled down if needed so the video fits OpenH264's
    // limits (3840x2160 landscape, 2160x3840 portrait): tall maps like
    // Mississippi River would otherwise come out several thousand pixels high.
    let (max_w, max_h) = if h > w { (2160.0, 3840.0) } else { (3840.0, 2160.0) };
    let want_w = opts.width as f64;
    let fit = (max_w / want_w).min(max_h / (want_w * h as f64 / w as f64)).min(1.0);
    let even_floor = |v: f64| ((v / 2.0).floor() as usize * 2).max(2);
    let out_w = even_floor(want_w * fit);
    let out_h = even_floor(out_w as f64 * h as f64 / w as f64).min(max_h as usize);
    let out = opts.out.clone().unwrap_or_else(|| format!("{game_id}.mp4"));
    let out_abs = std::path::absolute(&out).map(|p| p.display().to_string()).unwrap_or(out.clone());
    let t_init = t_init0.elapsed();
    println!("  Map {w}x{h}, ready in {}", secs(t_init));
    println!(
        "Rendering {} ticks into {} frames ({}s at {} fps, {}x{})",
        fmt_int(turns.len() as u64),
        frames,
        opts.seconds,
        opts.fps,
        out_w,
        out_h
    );

    // 3. The drawing and encoding threads
    let (to_draw, draw_rx) = sync_channel::<DrawMsg>(QUEUE_FRAMES);
    let (to_encode, encode_rx) = sync_channel::<(Arc<Vec<u8>>, u32)>(ENCODE_QUEUE);
    let encoded = Arc::new(AtomicU64::new(0));
    let draw_settings = Settings::new(&render_settings);
    let drawer = std::thread::Builder::new()
        .name("draw".into())
        .spawn({
            let thumbnail = opts.thumbnail.clone();
            move || draw_thread(Renderer::new(w, h, out_w, out_h, &terrain, draw_settings), draw_rx, to_encode, thumbnail)
        })
        .unwrap();
    let writer = encode::VideoWriter::create(&out, out_w, out_h, opts.fps, (opts.bitrate * 1e6) as u32, opts.threads)?;
    let encoded2 = encoded.clone();
    let encoder = std::thread::Builder::new()
        .name("encode".into())
        .spawn(move || encode_thread(writer, encode_rx, encoded2))
        .unwrap();

    // 4. Simulate, handing each frame to the drawing thread
    let tty = std::io::stdout().is_terminal();
    let total_ticks = turns.len() as u64;
    let sim_start = Instant::now();
    let mut sim = Span { start: Some(sim_start), ..Default::default() };
    let mut last_print = sim_start;
    let mut sent = 0u64;
    let mut last: (Arc<Vec<i32>>, Arc<Vec<Label>>) = (Arc::new(Vec::new()), Arc::new(Vec::new()));
    let mut stopped = false;
    let send = |msg: DrawMsg, sim: &mut Span| -> bool {
        let w0 = Instant::now();
        let ok = to_draw.send(msg).is_ok();
        sim.waited += w0.elapsed();
        ok
    };
    let check = replay::play(&mut game, &turns, &archived, frames, |game, f| {
        if stopped {
            return;
        }
        let mut styles = Vec::new();
        for p in &f.new_players {
            let cos = p.client_id.as_ref().and_then(|c| cosmetics.get(c)).cloned().unwrap_or(Value::Null);
            let (fill, border) = theme.player_colors(&p.id, p.player_type, p.team.as_deref(), &cos);
            styles.push((p.small_id, Style::new(fill, border, p.player_type, &settings)));
        }
        let labels: Vec<Label> = f
            .names
            .iter()
            .filter(|n| n.size > 0.0 && draw::name_is_readable(n.size, &settings, w as u32))
            .map(|n| {
                let p = game.players.iter().position(|p| p.small_id as u32 == n.small_id).unwrap();
                Label {
                    owner: n.small_id as u16,
                    name: game.display_name(p),
                    troops: look::troop_text(n.troops),
                    x: n.x,
                    y: n.y,
                    size: n.size,
                }
            })
            .collect();
        last = (Arc::new(f.units), Arc::new(labels));
        let msg = DrawMsg {
            progress: f.tick as f64 / total_ticks as f64,
            styles,
            tiles: f.tiles,
            units: last.0.clone(),
            labels: last.1.clone(),
            repeat: f.repeat,
        };
        sent += f.repeat as u64;
        if !send(msg, &mut sim) {
            stopped = true;
            return;
        }
        let now = Instant::now();
        if now - last_print >= Duration::from_millis(if tty { 250 } else { 2000 }) {
            last_print = now;
            let line = format!(
                "  tick {:>w$}/{}  {:>5.1}%  | frame {}/{}  | {:.1}s elapsed",
                fmt_int(f.tick as u64),
                fmt_int(total_ticks),
                100.0 * f.tick as f64 / total_ticks as f64,
                encoded.load(Ordering::Relaxed),
                frames + hold_frames,
                (now - sim_start).as_secs_f64(),
                w = fmt_int(total_ticks).len()
            );
            if tty {
                print!("\r{line}\x1b[K");
                let _ = std::io::stdout().flush();
            } else {
                println!("{line}");
            }
        }
    });
    sim.end = Some(Instant::now());
    if tty && !stopped {
        println!();
    }
    if opts.strict && check.mismatches > 0 {
        return Err("replay desynced (--strict)".into());
    }
    // Frames past the last tick, then the hold, repeat the final state.
    let rest = (frames + hold_frames).saturating_sub(sent);
    if rest > 0 && !stopped {
        let msg = DrawMsg { progress: 1.0, styles: Vec::new(), tiles: Vec::new(), units: last.0, labels: last.1, repeat: rest as u32 };
        send(msg, &mut sim);
    }
    drop(to_draw);

    // The encoder's own error explains a drawing thread that stopped for it.
    let draw = drawer.join().map_err(|_| "the drawing thread stopped")?;
    let (enc, written) = encoder.join().map_err(|_| "the encoder thread stopped")??;
    let draw = draw?;
    let total = t0.elapsed();

    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if let Some(path) = &opts.meta {
        let meta = serde_json::json!({
            "gameID": game_id,
            "map": info["config"]["gameMap"],
            "mode": info["config"]["gameMode"],
            "players": info["players"].as_array().map_or(0, |a| a.len()),
            "durationSeconds": duration,
            "ticks": total_ticks,
            "width": out_w,
            "height": out_h,
            "videoSeconds": written as f64 / opts.fps as f64,
            "bytes": size,
            "renderSeconds": total.as_secs_f64(),
            "hashesChecked": check.checked,
            "hashMismatches": check.mismatches,
        });
        std::fs::write(path, serde_json::to_vec_pretty(&meta).unwrap()).map_err(|e| format!("writing {path}: {e}"))?;
    }
    println!();
    println!("Done: {out_abs}");
    println!(
        "  {}x{}, {:.1}s of video, {:.1} MB",
        out_w,
        out_h,
        written as f64 / opts.fps as f64,
        size as f64 / 1024.0 / 1024.0
    );
    println!();
    print_timeline(
        t0,
        t0 + total,
        &[
            ("Fetch record", t0, t0 + t_fetch),
            ("Load map, set up game", t_init0, t_init0 + t_init),
            ("Simulate", sim_start, sim.end.unwrap()),
            ("Draw frames", draw.start.unwrap_or(sim_start), draw.end.unwrap_or(sim_start)),
            ("Encode (OpenH264)", enc.start.unwrap_or(sim_start), enc.end.unwrap_or(sim_start)),
        ],
        &[
            String::new(),
            format!(
                "Drawing: {} of work for {} frames ({:.1} ms each); encoding: {} ({:.1} ms per frame)",
                secs(draw.busy),
                fmt_int(written),
                draw.busy.as_secs_f64() * 1000.0 / written.max(1) as f64,
                secs(enc.busy),
                enc.busy.as_secs_f64() * 1000.0 / written.max(1) as f64,
            ),
            format!(
                "The simulation waited {} for drawing (queue of {QUEUE_FRAMES} frames full); drawing waited {} for the encoder",
                secs(sim.waited),
                secs(draw.waited)
            ),
        ],
    );
    println!();
    let minutes = duration / 60.0;
    println!(
        "Speed: {:.2} s of rendering per minute of gameplay ({:.1} s for {:.1} min)",
        total.as_secs_f64() / minutes,
        total.as_secs_f64(),
        minutes
    );
    if check.checked == 0 {
        println!("Replay check: no archived hashes to compare against");
    } else if check.mismatches == 0 {
        println!("Replay check: all {} archived state hashes matched", fmt_int(check.checked as u64));
    } else {
        eprintln!(
            "! Replay check: {}/{} hashes differed, first at tick {}. The video may not match what players saw.",
            fmt_int(check.mismatches as u64),
            fmt_int(check.checked as u64),
            check.first.unwrap_or(0)
        );
    }
    Ok(())
}

fn draw_thread(
    mut r: Renderer,
    rx: Receiver<DrawMsg>,
    tx: SyncSender<(Arc<Vec<u8>>, u32)>,
    thumbnail: Option<String>,
) -> Result<Span, String> {
    let mut span = Span::default();
    for msg in rx {
        span.start.get_or_insert_with(Instant::now);
        let d0 = Instant::now();
        for (id, s) in msg.styles {
            r.set_style(id, s);
        }
        r.apply_tiles(&msg.tiles);
        let frame = Arc::new(r.draw(&msg.units, &msg.labels, msg.progress).to_vec());
        span.busy += d0.elapsed();
        span.end = Some(Instant::now());
        let w0 = Instant::now();
        if tx.send((frame, msg.repeat)).is_err() {
            return Err("the encoder stopped".into());
        }
        span.waited += w0.elapsed();
    }
    if let Some(path) = &thumbnail {
        r.save_png(path)?;
    }
    if let Some(p) = &r.profile {
        let rows: Vec<String> = p.iter().map(|(k, d)| format!("{k} {:.0} ms", d.as_secs_f64() * 1000.0)).collect();
        eprintln!("draw profile: {}", rows.join(", "));
    }
    Ok(span)
}

fn encode_thread(
    mut writer: encode::VideoWriter,
    rx: Receiver<(Arc<Vec<u8>>, u32)>,
    encoded: Arc<AtomicU64>,
) -> Result<(Span, u64), String> {
    let mut span = Span::default();
    for (frame, repeat) in rx {
        span.start.get_or_insert_with(Instant::now);
        let e0 = Instant::now();
        for _ in 0..repeat {
            writer.write(&frame)?;
            encoded.fetch_add(1, Ordering::Relaxed);
        }
        span.busy += e0.elapsed();
    }
    let f0 = Instant::now();
    let written = writer.finish()?;
    span.busy += f0.elapsed();
    span.end = Some(Instant::now());
    Ok((span, written))
}

/// The run as a waterfall: one bar per stage on a shared time axis, so
/// stages that overlap show up as overlapping bars.
fn print_timeline(t0: Instant, t1: Instant, stages: &[(&str, Instant, Instant)], notes: &[String]) {
    const COLS: f64 = 48.0;
    let total = (t1 - t0).as_secs_f64().max(1e-3);
    let label = stages.iter().map(|s| s.0.len()).max().unwrap_or(0);
    let col = |t: Instant| (t.saturating_duration_since(t0).as_secs_f64() / total * COLS).clamp(0.0, COLS);
    let axis_end = secs(t1 - t0);
    println!("  {:label$}  {:>8}  0 s{:>w$}", "", "", axis_end, w = COLS as usize - 3);
    for &(name, start, end) in stages {
        let a = col(start).floor() as usize;
        let b = (col(end).ceil() as usize).max(a + 1).min(COLS as usize);
        let bar = format!("{}{}", " ".repeat(a), "█".repeat(b.saturating_sub(a)));
        println!("  {:label$}  {:>8}  {:<w$}|", name, secs(end.saturating_duration_since(start)), bar, w = COLS as usize);
    }
    for n in notes {
        println!("  {n}");
    }
}
