//! engine-rs: a Rust port of the OpenFront.io simulation (engine commit
//! 9f4a28f), checked tick by tick against the TypeScript engine with
//! harness/dump-ts.mts and harness/compare.mjs.
//!
//!   engine-rs <record.json> <OpenFrontIO dir> <ticks> <out.jsonl>
//!   engine-rs stream <record.json> <OpenFrontIO dir> <frames>   (for render.ts --rust)

#![forbid(unsafe_code)]

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use engine_rs::game::{Exec, Game};
use engine_rs::map::GameMap;
use engine_rs::replay::{self, create_exec, find_map, setup, DirResources};
use engine_rs::{prof, water};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("paths") {
        return paths_mode(&args[2..]);
    }
    if args.get(1).map(|s| s.as_str()) == Some("stream") {
        return stream_mode(&args[2..]);
    }
    if args.len() < 5 {
        eprintln!("usage: engine-rs <record.json> <OpenFrontIO dir> <ticks> <out.jsonl>");
        std::process::exit(2);
    }
    let record: Value = serde_json::from_str(&fs::read_to_string(&args[1]).expect("read record")).expect("parse record");
    let engine_dir = Path::new(&args[2]);
    let max_ticks: usize = args[3].parse().expect("ticks");
    let out_path = &args[4];

    let t0 = std::time::Instant::now();
    prof::ON.store(std::env::var("ENGINE_RS_PROFILE").is_ok(), std::sync::atomic::Ordering::Relaxed);
    let (mut game, turns) = setup(&record, &DirResources(engine_dir.to_path_buf()));
    let setup_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // ENGINE_RS_BENCH=1: only the last state line, so the timing is the
    // simulation alone.
    let bench = std::env::var("ENGINE_RS_BENCH").is_ok();
    let mut out = BufWriter::new(fs::File::create(out_path).expect("create output"));
    if !bench {
        writeln!(out, "{}", digest(&game, -1)).unwrap();
    }
    let t1 = std::time::Instant::now();
    let mut sim = std::time::Duration::ZERO;
    let n = max_ticks.min(turns.len());
    for (i, intents) in turns.iter().take(n).enumerate() {
        let s0 = std::time::Instant::now();
        for intent in intents {
            let e = create_exec(&game, intent);
            game.add_execution(e);
        }
        game.execute_next_tick();
        sim += s0.elapsed();
        if !bench || i + 1 == n {
            writeln!(out, "{}", digest(&game, i as i64)).unwrap();
        }
        // ENGINE_RS_DUMP_AT=<tick>: the owner and fallout map after that tick,
        // in the format of dump-ts.mts (for finding the exact tiles).
        if std::env::var("ENGINE_RS_DUMP_AT").ok().and_then(|v| v.parse::<usize>().ok()) == Some(i) {
            let mut buf = Vec::with_capacity(game.map.num_tiles() as usize * 2);
            for r in 0..game.map.num_tiles() {
                let v = game.map.owner_id(r) | if game.map.has_fallout(r) { 1 << 13 } else { 0 };
                buf.extend_from_slice(&v.to_le_bytes());
            }
            fs::write(format!("{out_path}.owners-{i}.bin"), buf).unwrap();
        }
    }
    out.flush().unwrap();
    if game.profile.is_some() {
        prof::report();
    }
    if let Some(p) = &game.profile {
        let mut v: Vec<_> = p.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        for (k, d) in v {
            eprintln!("  {:<32} {:>8.1} ms", k, d.as_secs_f64() * 1000.0);
        }
    }
    eprintln!(
        "setup {:.0} ms, {} ticks: simulation {:.1} ms, total with state lines {:.0} ms",
        setup_ms,
        n,
        sim.as_secs_f64() * 1000.0,
        t1.elapsed().as_secs_f64() * 1000.0
    );
}


// ── State lines for the comparison harness (same fields as dump-ts.mts) ──

fn fnv(values: impl IntoIterator<Item = u32>) -> u32 {
    let mut h: u32 = 2166136261;
    for v in values {
        h = (h ^ v).wrapping_mul(16777619);
    }
    h
}

fn exec_list(execs: &[Exec]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut prev = "";
    let mut n = 0;
    for e in execs {
        let name = e.name();
        if name == prev {
            n += 1;
        } else {
            if n > 0 {
                parts.push(if n > 1 { format!("{prev}*{n}") } else { prev.to_string() });
            }
            prev = name;
            n = 1;
        }
    }
    if n > 0 {
        parts.push(if n > 1 { format!("{prev}*{n}") } else { prev.to_string() });
    }
    parts.join(",")
}

fn digest(game: &Game, t: i64) -> String {
    let n = game.map.num_tiles();
    let owners = fnv((0..n).map(|r| game.map.owner_id(r) as u32 | if game.map.has_fallout(r) { 1 << 13 } else { 0 }));
    let players: Vec<Value> = game
        .players
        .iter()
        .enumerate()
        .map(|(i, p)| {
            json!([
                p.small_id,
                p.info.id,
                p.info.name,
                p.info.player_type.as_str(),
                p.team.clone().unwrap_or_default(),
                p.troops.to_string(),
                p.gold.to_string(),
                p.tiles.len(),
                fnv(p.tiles.values()),
                p.border_tiles.len(),
                fnv(p.border_tiles.values()),
                p.spawn_tile.map(|s| s as i64).unwrap_or(-1),
                if p.is_disconnected { 1 } else { 0 },
                p.outgoing
                    .iter()
                    .map(|&a| {
                        let at = &game.attacks[a];
                        let target = at.target.map(|t| game.players[t].small_id).unwrap_or(0);
                        format!("{}:{}:{:x}:{}:{}", at.id, target, at.troops.to_bits(), at.border_size, if at.retreating { 1 } else { 0 })
                    })
                    .collect::<Vec<_>>()
                    .join(";"),
                p.incoming.iter().map(|&a| game.attacks[a].id.clone()).collect::<Vec<_>>().join(";"),
                p.relations
                    .iter()
                    .map(|(o, v)| format!("{}:{:x}", game.players[*o].small_id, v.to_bits()))
                    .collect::<Vec<_>>()
                    .join(";"),
                p.units
                    .iter()
                    .map(|&u| {
                        let u = &game.units[u];
                        format!(
                            "{}:{}:{}:{:x}:{}:{}:{}:{}",
                            u.id,
                            u.type_name,
                            u.tile,
                            u.troops.to_bits(),
                            u.level,
                            if u.under_construction { 1 } else { 0 },
                            u.deletion_at.map(|d| d as i64).unwrap_or(-1),
                            u.health
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(";"),
                p.alliances
                    .iter()
                    .map(|&a| {
                        let al = &game.alliances[a];
                        format!("{}:{}:{}", al.id, game.players[al.other(i)].small_id, al.expires_at)
                    })
                    .collect::<Vec<_>>()
                    .join(";"),
                p.marked_traitor_tick,
                p.embargoes
                    .iter()
                    .map(|e| format!("{}:{}:{}", game.players[e.target].small_id, e.created_at, if e.is_temporary { 1 } else { 0 }))
                    .collect::<Vec<_>>()
                    .join(";"),
            ])
        })
        .collect();
    json!({
        "t": t,
        "spawnPhase": if game.in_spawn_phase() { 1 } else { 0 },
        "hash": game.hash(),
        "owners": owners,
        "execs": exec_list(&game.execs),
        "pending": exec_list(&game.uninit_execs),
        "requests": game.alliance_requests.iter().map(|&r| {
            let q = &game.requests[r];
            format!("{}>{}@{}", game.players[q.requestor].small_id, game.players[q.recipient].small_id, q.created_at)
        }).collect::<Vec<_>>().join(";"),
        "players": players,
    })
    .to_string()
}

/// engine-rs paths <OpenFrontIO dir> <map name> <queries.json> <out.json>:
/// answers water-path queries ([from, to] or [[from...], to] on the full
/// map) with the ported chain, for harness/paths-ts.mts to compare.
fn paths_mode(args: &[String]) {
    let engine_dir = Path::new(&args[0]);
    let (map_dir, manifest) = find_map(engine_dir, &args[1]);
    let load = |key: &str, file: &str| {
        let m = &manifest[key];
        let (w, h) = (m["width"].as_u64().unwrap() as u32, m["height"].as_u64().unwrap() as u32);
        GameMap::new(w, h, fs::read(map_dir.join(file)).unwrap(), m["num_land_tiles"].as_u64().unwrap() as u32)
    };
    let map = load("map", "map.bin");
    let t0 = std::time::Instant::now();
    let mut pf = water::WaterPathfinder::new(load("map4x", "map4x.bin"));
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let queries: Value = serde_json::from_str(&fs::read_to_string(&args[2]).unwrap()).unwrap();
    let t1 = std::time::Instant::now();
    let mut out = Vec::new();
    for q in queries.as_array().unwrap() {
        let from: Vec<u32> = match &q[0] {
            Value::Array(a) => a.iter().map(|v| v.as_u64().unwrap() as u32).collect(),
            v => vec![v.as_u64().unwrap() as u32],
        };
        let to = q[1].as_u64().unwrap() as u32;
        out.push(match pf.find_path(&map, &from, to) {
            Some(p) => json!(p),
            None => Value::Null,
        });
    }
    eprintln!("graph built in {:.0} ms, {} queries in {:.0} ms", build_ms, out.len(), t1.elapsed().as_secs_f64() * 1000.0);
    fs::write(&args[3], serde_json::to_string(&out).unwrap()).unwrap();
}

// ── stream: the state render.ts draws, at each video frame's tick ──
//
//   engine-rs stream <record.json> <OpenFrontIO dir> <frames>
//
// Writes to stdout, little-endian, every section 4-byte aligned:
//   header: "OFRS", width, height, ticks (u32 each), then one terrain byte per tile
//   frame:  tick, repeat (frames showing it), new players as JSON (u32 length +
//           bytes), changed tiles (u32 count + [ref, owner | fallout << 16] u32
//           pairs), units (u32 count + [code, tile, owner, underConstruction]
//           i32 quads), names (u32 count + [smallID u32, x, y, size, troops] with
//           the four numbers as f64)
//   end:    0xFFFFFFFF, then hashes checked, mismatches, first mismatch tick (-1: none)
// The frames are those of replay::play.
fn stream_mode(args: &[String]) {
    if args.len() < 3 {
        eprintln!("usage: engine-rs stream <record.json> <OpenFrontIO dir> <frames>");
        std::process::exit(2);
    }
    let record: Value = serde_json::from_str(&fs::read_to_string(&args[0]).expect("read record")).expect("parse record");
    let frames: u64 = args[2].parse().expect("frames");
    prof::ON.store(std::env::var("ENGINE_RS_PROFILE").is_ok(), std::sync::atomic::Ordering::Relaxed);
    let (mut game, turns) = setup(&record, &DirResources(PathBuf::from(&args[1])));
    let archived = replay::archived_hashes(&record);
    let (w, h) = (game.map.width, game.map.height);

    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(1 << 22, stdout.lock());
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"OFRS");
    for v in [w, h, turns.len() as u32] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for r in 0..w * h {
        buf.push(game.map.terrain_byte(r));
    }
    pad4(&mut buf);
    out.write_all(&buf).unwrap();

    let check = replay::play(&mut game, &turns, &archived, frames, |_, f| {
        let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);
        let put = |buf: &mut Vec<u8>, v: u32| buf.extend_from_slice(&v.to_le_bytes());
        put(&mut buf, f.tick);
        put(&mut buf, f.repeat);
        let js = replay::new_players_json(&f.new_players);
        put(&mut buf, js.len() as u32);
        buf.extend_from_slice(&js);
        pad4(&mut buf);
        put(&mut buf, (f.tiles.len() / 2) as u32);
        for &v in &f.tiles {
            put(&mut buf, v);
        }
        put(&mut buf, (f.units.len() / 4) as u32);
        for u in &f.units {
            buf.extend_from_slice(&u.to_le_bytes());
        }
        put(&mut buf, f.names.len() as u32);
        for n in &f.names {
            put(&mut buf, n.small_id);
            for v in [n.x, n.y, n.size, n.troops] {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        out.write_all(&buf).unwrap();
    });
    let mut end = Vec::new();
    let first = check.first.map(|f| f as i32).unwrap_or(-1) as u32;
    for v in [u32::MAX, check.checked, check.mismatches, first] {
        end.extend_from_slice(&v.to_le_bytes());
    }
    out.write_all(&end).unwrap();
    out.flush().unwrap();
    if prof::ON.load(std::sync::atomic::Ordering::Relaxed) {
        prof::report();
    }
}

fn pad4(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

