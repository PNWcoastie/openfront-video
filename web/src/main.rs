//! openfront.video: a small web service around openfront-video.
//!
//!   /                          a form for a game link
//!   /game/<id>, /<x>/game/<id> the game's page: the video, with Open Graph tags so
//!                              Discord embeds it (swap openfront.io for openfront.video
//!                              in a game link)
//!   /video/<id>.mp4            the video (byte ranges supported)
//!   /thumb/<id>.png            its last frame, the embed's preview image
//!
//! The first request for a game renders it; the video, thumbnail and details
//! are kept in DATA_DIR and served from there afterwards. One render per game
//! runs at a time, and at most MAX_RENDERS at once.
//!
//! Settings (environment): PORT (8080), DATA_DIR (./data), PUBLIC_URL
//! (https://openfront.video), OPENFRONT_VIDEO_BIN (openfront-video),
//! MAX_RENDERS (1), EMBED_WAIT_SECS (25), VIDEO_WAIT_SECS (120), and the
//! TypeScript fallback's (see engines.rs).
//!
//! Games from engine versions openfront-video doesn't follow render with
//! render.ts and the TypeScript engine at the version they were played on.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;
use tower::ServiceExt;
use tower_http::services::ServeFile;

mod auto;
mod cleanup;
mod engines;
mod slots;
mod stats;

/// A failed render is remembered this long before a request may retry it.
const FAILURE_TTL: Duration = Duration::from_secs(600);
/// A render that takes longer than this is stopped.
const RENDER_TIMEOUT: Duration = Duration::from_secs(20 * 60);

#[derive(Clone)]
enum Outcome {
    Done,
    /// The API has no such game: unfinished (the API only lists finished
    /// games) or a wrong ID. Not remembered, so the next request asks again.
    NotFound,
    /// An auto render stopped so a request could have its slot.
    Preempted,
    Failed(String),
}

enum Job {
    /// Receives the outcome when the render ends.
    Running(watch::Receiver<Option<Outcome>>),
    Failed(String, Instant),
}

struct App {
    dir: PathBuf,
    bin: PathBuf,
    public_url: String,
    embed_wait: Duration,
    video_wait: Duration,
    slots: Arc<slots::Slots>,
    jobs: Mutex<HashMap<String, Job>>,
    /// Signalled after each finished render, so the cleanup checks usage.
    rendered: tokio::sync::Notify,
    events: stats::EventLog,
    auto_status: Mutex<stats::AutoStatus>,
    /// When the server started (Unix seconds).
    started: u64,
    /// TypeScript renders for games from engine versions the Rust build doesn't follow.
    engines: Option<engines::Engines>,
}

enum Status {
    Ready(Value),
    Pending,
    NotFound,
    Failed(String),
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

fn valid_id(id: &str) -> bool {
    (4..=32).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// The game ID in a link (`.../game/<id>`) or a bare ID.
fn game_id_from(input: &str) -> Option<String> {
    let input = input.trim();
    if let Some(i) = input.find("/game/") {
        let id: String = input[i + 6..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
        return valid_id(&id).then_some(id);
    }
    valid_id(input).then(|| input.to_string())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

impl App {
    fn path(&self, id: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{id}.{ext}"))
    }

    /// The details file is written last, so it marks a finished render.
    async fn meta(&self, id: &str) -> Option<Value> {
        let bytes = tokio::fs::read(self.path(id, "json")).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The game's status, starting its render if needed and waiting up to
    /// `wait` for it to finish.
    async fn status(self: &Arc<Self>, id: &str, wait: Duration) -> Status {
        self.status_for(id, wait, false).await
    }

    /// `status`, for an on-demand request or (auto) the background renderer.
    async fn status_for(self: &Arc<Self>, id: &str, wait: Duration, auto: bool) -> Status {
        if let Some(meta) = self.meta(id).await {
            return Status::Ready(meta);
        }
        let mut rx = {
            let mut jobs = self.jobs.lock().unwrap();
            match jobs.get(id) {
                Some(Job::Running(rx)) => rx.clone(),
                Some(Job::Failed(msg, at)) if at.elapsed() < FAILURE_TTL => return Status::Failed(msg.clone()),
                _ => {
                    let (tx, rx) = watch::channel(None);
                    jobs.insert(id.to_string(), Job::Running(rx.clone()));
                    let app = self.clone();
                    let id = id.to_string();
                    tokio::spawn(async move {
                        let outcome = app.render(&id, auto).await;
                        let mut jobs = app.jobs.lock().unwrap();
                        match &outcome {
                            Outcome::Done | Outcome::NotFound | Outcome::Preempted => jobs.remove(&id),
                            Outcome::Failed(msg) => jobs.insert(id.clone(), Job::Failed(msg.clone(), Instant::now())),
                        };
                        let _ = tx.send(Some(outcome));
                    });
                    rx
                }
            }
        };
        // Copied out so the channel's lock isn't held across an await.
        let finished = tokio::time::timeout(wait, rx.wait_for(|o| o.is_some())).await.ok().and_then(|r| r.ok().map(|o| o.clone()));
        match finished {
            Some(o) => match o.unwrap() {
                Outcome::Done => self.meta(id).await.map_or(Status::Failed("the render left no details".into()), Status::Ready),
                Outcome::NotFound => Status::NotFound,
                Outcome::Preempted => Status::Pending,
                Outcome::Failed(msg) => Status::Failed(msg),
            },
            None => Status::Pending,
        }
    }

    /// Runs openfront-video for the game into temporary files, then moves
    /// them into place (details last).
    async fn render(&self, id: &str, auto: bool) -> Outcome {
        let mut slot = self.slots.acquire(id, auto).await;
        let preempted = async {
            match slot.preempted.take() {
                Some(rx) => {
                    let _ = rx.await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let t0 = Instant::now();
        println!("render {id}: started{}", if auto { " (auto)" } else { "" });
        let tmp = |ext: &str| self.dir.join(format!(".{id}.tmp.{ext}"));
        let work = async {
            let mut cmd = tokio::process::Command::new(&self.bin);
            cmd.arg(id)
                .arg("--out")
                .arg(tmp("mp4"))
                .arg("--thumbnail")
                .arg(tmp("png"))
                .arg("--meta")
                .arg(tmp("json"))
                // Games from other engine versions stop right away (exit code 4): they
                // go to the TypeScript engine, or are skipped by auto renders without it.
                .args(if auto || self.engines.is_some() { &["--same-engine"][..] } else { &[][..] })
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            let out = tokio::time::timeout(RENDER_TIMEOUT, cmd.output()).await?;
            let (Some(engines), Ok(out)) = (&self.engines, &out) else { return Ok(out) };
            if out.status.code() != Some(4) {
                return Ok(Ok(out.clone()));
            }
            let err = String::from_utf8_lossy(&out.stderr);
            let Some(commit) = err.split("played on engine version ").nth(1).and_then(|r| r.split(',').next()) else {
                return Ok(Ok(out.clone()));
            };
            println!("render {id}: played on engine {}, rendering with the TypeScript engine", &commit[..commit.len().min(9)]);
            let lease = match engines.get(commit).await {
                Ok(l) => l,
                Err(e) => return Ok(Err(std::io::Error::other(e))),
            };
            let mut ts = tokio::process::Command::new("node");
            ts.current_dir(&lease.dir)
                .arg("--import")
                .arg("tsx")
                .arg(&engines.render_ts)
                .arg(id)
                .arg("--engine")
                .arg(&lease.dir)
                .arg("--out")
                .arg(tmp("mp4"))
                .arg("--thumbnail")
                .arg(tmp("png"))
                .arg("--meta")
                .arg(tmp("json"))
                .args(["--no-cache", "--strict"])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            // The TypeScript engine is about twice as slow.
            tokio::time::timeout(RENDER_TIMEOUT * 2, ts.output()).await
        };
        // Dropping the work on preemption kills the renderer (kill_on_drop).
        let result = tokio::select! {
            r = work => Some(r),
            _ = preempted => None,
        };
        let outcome = match result {
            None => Outcome::Preempted,
            Some(result) => match result {
            Err(_) => Outcome::Failed("the render took too long".into()),
            Ok(Err(e)) => Outcome::Failed(format!("couldn't render: {e}")),
            Ok(Ok(out)) if out.status.code() == Some(3) => Outcome::NotFound,
            Ok(Ok(out)) if !out.status.success() => {
                // The renderer's last line says why ("error: ...", "... is not ported yet").
                let err = String::from_utf8_lossy(&out.stderr);
                let line = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("the renderer failed");
                let line = line.trim_start_matches("error: ").trim_start_matches("Error: ").trim_start_matches("openfront-video stopped: ");
                Outcome::Failed(line.to_string())
            }
            Ok(Ok(_)) => {
                let mut moved = Ok(());
                for ext in ["mp4", "png", "json"] {
                    if let Err(e) = tokio::fs::rename(tmp(ext), self.path(id, ext)).await {
                        moved = Err(e);
                        break;
                    }
                }
                match moved {
                    Ok(()) => Outcome::Done,
                    Err(e) => Outcome::Failed(format!("couldn't store the video: {e}")),
                }
            }
            },
        };
        drop(slot);
        if !matches!(outcome, Outcome::Done) {
            for ext in ["mp4", "png", "json"] {
                let _ = tokio::fs::remove_file(tmp(ext)).await;
            }
        }
        let (kind, msg) = match &outcome {
            Outcome::Done => ("done", ""),
            Outcome::NotFound => ("not_found", ""),
            Outcome::Preempted => ("preempted", ""),
            Outcome::Failed(m) => ("failed", m.as_str()),
        };
        self.events.render(id, auto, kind, t0.elapsed().as_secs_f64(), msg);
        match &outcome {
            Outcome::Done => {
                println!("render {id}: done in {:.1} s", t0.elapsed().as_secs_f64());
                self.rendered.notify_one();
            }
            Outcome::NotFound => println!("render {id}: not finished yet (or no such game)"),
            Outcome::Preempted => println!("render {id}: stopped after {:.1} s to make room for a request", t0.elapsed().as_secs_f64()),
            Outcome::Failed(msg) => println!("render {id}: failed after {:.1} s: {msg}", t0.elapsed().as_secs_f64()),
        }
        outcome
    }
}

const STYLE: &str = "body{margin:0;background:#16181d;color:#e6e6e6;font:16px/1.5 system-ui,sans-serif}\
main{max-width:960px;margin:0 auto;padding:24px 16px}a{color:#7cb7ff}h1{font-size:22px;margin:0 0 4px}\
p{color:#a9adb7;margin:0 0 16px}video,img{width:100%;height:auto;border-radius:8px;background:#000}\
form{display:flex;gap:8px;flex-wrap:wrap}input{flex:1;min-width:0;padding:10px 12px;border-radius:6px;border:1px solid #3a3f4b;background:#0f1115;color:inherit;font:inherit}\
button{padding:10px 16px;border-radius:6px;border:0;background:#3b82f6;color:#fff;font:inherit;cursor:pointer}";

fn page(head: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"theme-color\" content=\"#3b82f6\">{head}<style>{STYLE}</style></head><body><main>{body}</main></body></html>"
    ))
}

/// Google AdSense: ADSENSE_CLIENT (ca-pub-...) loads the script (Auto ads), and
/// ADSENSE_SLOT adds a fixed responsive unit. Both are checked to be plain IDs
/// before they go into the page.
struct Ads {
    client: String,
    slot: Option<String>,
}

static ADS: std::sync::OnceLock<Option<Ads>> = std::sync::OnceLock::new();

fn ads() -> Option<&'static Ads> {
    ADS.get_or_init(|| {
        let client = std::env::var("ADSENSE_CLIENT").ok().filter(|c| !c.is_empty())?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !client.strip_prefix("ca-pub-").is_some_and(digits) {
            eprintln!("ADSENSE_CLIENT should look like ca-pub-1234567890123456; ads are off");
            return None;
        }
        let slot = std::env::var("ADSENSE_SLOT").ok().filter(|s| digits(s));
        Some(Ads { client, slot })
    })
    .as_ref()
}

/// `page` with ads. Only for pages that stay put: AdSense doesn't allow ads on
/// a page that reloads itself, like the one shown while a game renders.
fn page_with_ads(head: &str, body: &str) -> Html<String> {
    let Some(ads) = ads() else { return page(head, body) };
    let script = format!(
        r#"<script async src="https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js?client={}" crossorigin="anonymous"></script>"#,
        ads.client
    );
    let unit = match &ads.slot {
        Some(slot) => format!(
            r#"<ins class="adsbygoogle" style="display:block;margin-top:24px" data-ad-client="{}" data-ad-slot="{slot}" data-ad-format="auto" data-full-width-responsive="true"></ins><script>(adsbygoogle=window.adsbygoogle||[]).push({{}});</script>"#,
            ads.client
        ),
        None => String::new(),
    };
    page(&format!("{head}{script}"), &format!("{body}{unit}"))
}

/// /ads.txt, which AdSense checks to confirm who may sell ads on the site.
async fn ads_txt() -> Response {
    match ads() {
        Some(ads) => format!("google.com, {}, DIRECT, f08c47fec0942fa0\n", ads.client.trim_start_matches("ca-")).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn index(State(app): State<Arc<App>>, headers: HeaderMap) -> Html<String> {
    app.events.view("", "home", &headers);
    page_with_ads(
        "<title>openfront.video</title><meta property=\"og:title\" content=\"openfront.video\">\
<meta property=\"og:description\" content=\"Timelapse videos of OpenFront.io games\">",
        "<h1>openfront.video</h1><p>Paste an OpenFront.io game link or ID to get a timelapse of the whole game. \
Or swap <b>openfront.io</b> for <b>openfront.video</b> in a game link.</p>\
<form action=\"/go\"><input name=\"game\" placeholder=\"https://openfront.io/game/...\" required autofocus><button>Make video</button></form>",
    )
}

#[derive(Deserialize)]
struct Go {
    game: String,
}

async fn go(Query(q): Query<Go>) -> Response {
    match game_id_from(&q.game) {
        Some(id) => Redirect::to(&format!("/game/{id}")).into_response(),
        None => (StatusCode::BAD_REQUEST, page("<title>openfront.video</title>", "<h1>That doesn't look like a game link</h1><p><a href=\"/\">Try again</a></p>")).into_response(),
    }
}

/// Link previewers (Discord and friends) wait for the render; people get a
/// page that refreshes until it's done.
fn is_previewer(headers: &HeaderMap) -> bool {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    ["discordbot", "twitterbot", "slackbot", "telegrambot", "facebookexternalhit", "whatsapp", "embedly"].iter().any(|b| ua.contains(b))
}

async fn game_page(app: Arc<App>, id: String, headers: HeaderMap) -> Response {
    if !valid_id(&id) {
        return (StatusCode::NOT_FOUND, page("<title>Not found</title>", "<h1>Not a game ID</h1>")).into_response();
    }
    // People wait a moment too: long enough to learn a game is not finished
    // (a quick API lookup), not for a whole render.
    let wait = if is_previewer(&headers) { app.embed_wait } else { Duration::from_secs(3) };
    let base = &app.public_url;
    let video = format!("{base}/video/{id}.mp4");
    let thumb = format!("{base}/thumb/{id}.png");
    let url = format!("{base}/game/{id}");
    let source = format!("https://openfront.io/game/{id}");
    match app.status(&id, wait).await {
        Status::Ready(meta) => {
            cleanup::touch(&app.path(&id, "json"));
            app.events.view(&id, "page", &headers);
            let s = |k: &str| meta[k].as_str().unwrap_or("?").to_string();
            let (w, h) = (meta["width"].as_u64().unwrap_or(1280), meta["height"].as_u64().unwrap_or(1280));
            let minutes = (meta["durationSeconds"].as_f64().unwrap_or(0.0) / 60.0).round();
            let title = format!("{} · {}", s("map"), s("mode"));
            let mut desc = format!("OpenFront.io game {id}: {} players, {minutes} min, as a timelapse", meta["players"]);
            if meta["hashMismatches"].as_u64().unwrap_or(0) > 0 {
                desc.push_str(" (the replay drifted from the real game, so it may differ)");
            }
            let (t, d) = (esc(&title), esc(&desc));
            let head = format!(
                "<title>{t}</title>\
<meta name=\"description\" content=\"{d}\">\
<meta property=\"og:site_name\" content=\"openfront.video\">\
<meta property=\"og:type\" content=\"video.other\">\
<meta property=\"og:title\" content=\"{t}\">\
<meta property=\"og:description\" content=\"{d}\">\
<meta property=\"og:url\" content=\"{url}\">\
<meta property=\"og:image\" content=\"{thumb}\">\
<meta property=\"og:image:width\" content=\"{w}\"><meta property=\"og:image:height\" content=\"{h}\">\
<meta property=\"og:video\" content=\"{video}\">\
<meta property=\"og:video:url\" content=\"{video}\">\
<meta property=\"og:video:secure_url\" content=\"{video}\">\
<meta property=\"og:video:type\" content=\"video/mp4\">\
<meta property=\"og:video:width\" content=\"{w}\"><meta property=\"og:video:height\" content=\"{h}\">\
<meta name=\"twitter:card\" content=\"player\">\
<meta name=\"twitter:title\" content=\"{t}\">\
<meta name=\"twitter:image\" content=\"{thumb}\">\
<meta name=\"twitter:player\" content=\"{video}\">\
<meta name=\"twitter:player:stream\" content=\"{video}\">\
<meta name=\"twitter:player:stream:content_type\" content=\"video/mp4\">\
<meta name=\"twitter:player:width\" content=\"{w}\"><meta name=\"twitter:player:height\" content=\"{h}\">"
            );
            let body = format!(
                "<h1>{t}</h1><p>{d}. <a href=\"{source}\">Replay on OpenFront</a> · <a href=\"/video/{id}.mp4\" download>Download</a></p>\
<video src=\"/video/{id}.mp4\" poster=\"/thumb/{id}.png\" width=\"{w}\" height=\"{h}\" controls autoplay muted loop playsinline></video>"
            );
            page_with_ads(&head, &body).into_response()
        }
        Status::Pending => {
            app.events.view(&id, "pending", &headers);
            let head = format!(
                "<title>Rendering {id}</title><meta http-equiv=\"refresh\" content=\"5\">\
<meta property=\"og:site_name\" content=\"openfront.video\"><meta property=\"og:title\" content=\"OpenFront game {id}\">\
<meta property=\"og:description\" content=\"The timelapse is still rendering\"><meta property=\"og:url\" content=\"{url}\">"
            );
            let body = format!(
                "<h1>Rendering game {id}…</h1><p>This takes about half a minute the first time; the page reloads by itself. \
After that the video is ready right away.</p><p><a href=\"{source}\">Replay on OpenFront</a></p>"
            );
            page(&head, &body).into_response()
        }
        // Link previewers get no page at all, so they cache nothing and fetch
        // again the next time the link is posted.
        Status::NotFound if is_previewer(&headers) => {
            app.events.view(&id, "not_found", &headers);
            (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "60")], "game not finished yet").into_response()
        }
        Status::NotFound => {
            app.events.view(&id, "not_found", &headers);
            let head = format!("<title>Game {id} isn't finished</title><meta http-equiv=\"refresh\" content=\"30\">");
            let body = format!(
                "<h1>Game {id} isn't finished yet</h1><p>Its video is made once the game ends. This page checks again every 30 seconds. If the game is over, the ID may be wrong.</p><p><a href=\"{source}\">Watch it on OpenFront</a></p>"
            );
            page(&head, &body).into_response()
        }
        Status::Failed(msg) => {
            app.events.view(&id, "failed", &headers);
            let head = format!("<title>Couldn't render {id}</title><meta property=\"og:title\" content=\"Couldn't render game {id}\">");
            let body = format!("<h1>Couldn't render game {id}</h1><p>{}</p><p><a href=\"/\">Try another game</a></p>", esc(&msg));
            page(&head, &body).into_response()
        }
    }
}

async fn game(State(app): State<Arc<App>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    game_page(app, id, headers).await
}

async fn game_prefixed(State(app): State<Arc<App>>, Path((_, id)): Path<(String, String)>, headers: HeaderMap) -> Response {
    game_page(app, id, headers).await
}

/// /video/<id>.mp4 and /thumb/<id>.png: rendered on demand, then served
/// from disk with byte ranges and long caching.
async fn media(app: Arc<App>, name: String, ext: &str, req: Request) -> Response {
    let Some(id) = name.strip_suffix(&format!(".{ext}")).filter(|id| valid_id(id)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match app.status(id, app.video_wait).await {
        Status::Ready(_) => {
            cleanup::touch(&app.path(id, "json"));
            // A video play: the first request (later ranges are the same play).
            let range = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).unwrap_or("bytes=0-");
            if ext == "mp4" && range.starts_with("bytes=0-") {
                app.events.view(id, "video", req.headers());
            }
            let mut res = match ServeFile::new(app.path(id, ext)).oneshot(req).await {
                Ok(r) => r.map(Body::new),
                Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
            res
        }
        Status::Pending => (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "10")], "still rendering").into_response(),
        Status::NotFound => (StatusCode::NOT_FOUND, "game not finished yet").into_response(),
        Status::Failed(msg) => (StatusCode::NOT_FOUND, msg).into_response(),
    }
}

async fn video(State(app): State<Arc<App>>, Path(name): Path<String>, req: Request) -> Response {
    media(app, name, "mp4", req).await
}

async fn thumb(State(app): State<Arc<App>>, Path(name): Path<String>, req: Request) -> Response {
    media(app, name, "png", req).await
}

#[tokio::main]
async fn main() {
    let dir = PathBuf::from(env_or("DATA_DIR", "./data"));
    std::fs::create_dir_all(&dir).expect("DATA_DIR");
    // Leftovers of renders that were cut off by a restart.
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        if e.file_name().to_string_lossy().contains(".tmp.") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let secs = |k: &str, d: &str| Duration::from_secs(env_or(k, d).parse().expect(k));
    let dir_for_events = dir.clone();
    let app = Arc::new(App {
        dir,
        bin: PathBuf::from(env_or("OPENFRONT_VIDEO_BIN", "openfront-video")),
        public_url: env_or("PUBLIC_URL", "https://openfront.video").trim_end_matches('/').to_string(),
        embed_wait: secs("EMBED_WAIT_SECS", "25"),
        video_wait: secs("VIDEO_WAIT_SECS", "120"),
        slots: slots::Slots::new(env_or("MAX_RENDERS", "2").parse().expect("MAX_RENDERS")),
        jobs: Mutex::new(HashMap::new()),
        rendered: tokio::sync::Notify::new(),
        events: stats::EventLog::open(&dir_for_events),
        auto_status: Mutex::new(stats::AutoStatus::default()),
        started: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        engines: engines::Engines::from_env(&dir_for_events),
    });
    cleanup::start(app.clone());
    if env_or("AUTO_RENDER", "1") != "0" {
        auto::start(app.clone(), auto::Settings::from_env());
    }
    let router = Router::new()
        .route("/", get(index))
        .route("/go", get(go))
        .route("/health", get(|| async { "ok" }))
        .route("/ads.txt", get(ads_txt))
        .route("/admin", get(stats::page))
        .route("/admin/api/stats", get(stats::api))
        .route("/game/{id}", get(game))
        .route("/{prefix}/game/{id}", get(game_prefixed))
        .route("/video/{name}", get(video))
        .route("/thumb/{name}", get(thumb))
        .with_state(app.clone());
    let addr = format!("0.0.0.0:{}", env_or("PORT", "8080"));
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    println!("openfront-web on {addr}, videos in {}, public URL {}", app.dir.display(), app.public_url);
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .unwrap();
}
