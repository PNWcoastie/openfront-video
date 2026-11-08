//! openfront.video: a small web service around openfront-video.
//!
//!   /                          a form for a game link
//!   /game/<id>, /<x>/game/<id> the game's page: the video, with Open Graph tags so
//!                              Discord embeds it (swap openfront.io for openfront.video
//!                              in a game link)
//!   /video/<id>.mp4            the video (byte ranges supported)
//!   /thumb/<id>.png            its last frame, the embed's preview image
//!   /api/v1/render             optional generic Bot Render API (enabled via RENDER_API_TOKEN)
//!
//! The first request for a game renders it; the video, thumbnail and details
//! are kept in DATA_DIR and served from there afterwards. One render per game
//! runs at a time, and at most MAX_RENDERS at once.
//!
//! Settings (environment): PORT (8080), DATA_DIR (./data), PUBLIC_URL
//! (https://openfront.video), OPENFRONT_VIDEO_BIN (openfront-video),
//! MAX_RENDERS (2), EMBED_WAIT_SECS (25), VIDEO_WAIT_SECS (120),
//! RENDER_API_TOKEN (optional), and the TypeScript fallback's (see engines.rs).
//!
//! Games from engine versions openfront-video doesn't follow render with
//! the Rust engine when `git diff` shows zero engine changes, or fall back to
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
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::watch;
use tower::ServiceExt;
use tower_http::services::ServeFile;

mod auto;
mod cleanup;
mod engines;
mod slots;
mod stats;
mod storage;

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
    /// Receives the preview metadata (~0.15s) and final outcome when the render ends.
    Running {
        done_rx: watch::Receiver<Option<Outcome>>,
        preview_rx: watch::Receiver<Option<Value>>,
    },
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
    catalog: Arc<storage::Catalog>,
    events: stats::EventLog,
    auto_status: Mutex<stats::AutoStatus>,
    /// When the server started (Unix seconds).
    started: u64,
    /// TypeScript renders for games from engine versions the Rust build doesn't follow.
    engines: Option<engines::Engines>,
}

enum Status {
    Ready(Value),
    /// Fast preview metadata written via `--preview-meta` before simulation begins.
    PreviewReady(Value),
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

/// The game ID in a link (`.../game/<id>` or `.../join/<id>`) or a bare ID.
fn game_id_from(input: &str) -> Option<String> {
    let input = input.trim();
    for marker in ["/game/", "/join/"] {
        if let Some(i) = input.find(marker) {
            let id: String = input[i + marker.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            return valid_id(&id).then_some(id);
        }
    }
    if let Some((_, tail)) = input.rsplit_once('/') {
        let id: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if valid_id(&id) {
            return Some(id);
        }
    }
    valid_id(input).then(|| input.to_string())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

impl App {
    fn path(&self, id: &str, ext: &str) -> PathBuf {
        let primary = self.dir.join(format!("{id}.{ext}"));
        if ext == "mp4" && !primary.exists() {
            let alt = self.dir.join("videos").join(format!("{id}.mp4"));
            if alt.exists() {
                return alt;
            }
        }
        primary
    }

    fn progress_path(&self, id: &str) -> PathBuf {
        self.dir.join("tmp").join(format!("{id}.progress.json"))
    }

    /// Reads cached fast preview metadata (`--preview-meta`) for an in-progress render if available.
    async fn preview_meta(&self, id: &str) -> Option<Value> {
        {
            let jobs = self.jobs.lock().unwrap();
            if let Some(Job::Running { preview_rx, .. }) = jobs.get(id) {
                if let Some(meta) = preview_rx.borrow().clone() {
                    return Some(meta);
                }
            }
        }
        let bytes = tokio::fs::read(self.dir.join(format!(".{id}.tmp.preview.json"))).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Reads finished game metadata from the `redb` catalog (falling back to disk if placed externally).
    async fn meta(&self, id: &str) -> Option<Value> {
        if let Some(meta) = self.catalog.get_video_meta(id) {
            return Some(meta);
        }
        if let Ok(bytes) = tokio::fs::read(self.path(id, "json")).await {
            if let Ok(meta) = serde_json::from_slice::<Value>(&bytes) {
                let mp4_len = tokio::fs::metadata(self.path(id, "mp4")).await.map_or(0, |m| m.len());
                let png_len = tokio::fs::metadata(self.path(id, "png")).await.map_or(0, |m| m.len());
                let now = now_secs();
                self.catalog.upsert_video(id, &meta, mp4_len + png_len + bytes.len() as u64, now, now);
                return Some(meta);
            }
        }
        if let Ok(mp4_meta) = tokio::fs::metadata(self.path(id, "mp4")).await {
            let meta = json!({
                "gameID": id,
                "map": "?",
                "mode": "?",
                "players": 0,
                "durationSeconds": 0.0,
                "width": 1280,
                "height": 1280,
                "bytes": mp4_meta.len(),
            });
            return Some(meta);
        }
        None
    }

    /// The game's status, starting its render if needed and waiting up to
    /// `wait` for it to finish.
    async fn status(self: &Arc<Self>, id: &str, wait: Duration) -> Status {
        self.status_for(id, wait, false).await
    }

    /// `status`, for an on-demand request or (auto) the background renderer.
    async fn status_for(self: &Arc<Self>, id: &str, wait: Duration, auto: bool) -> Status {
        self.status_with_preview(id, wait, auto, false).await
    }

    /// `status`, optionally returning `Status::PreviewReady` as soon as `--preview-meta` is written (~0.15s).
    async fn status_with_preview(self: &Arc<Self>, id: &str, wait: Duration, auto: bool, allow_preview: bool) -> Status {
        if let Some(meta) = self.meta(id).await {
            return Status::Ready(meta);
        }
        let (mut done_rx, mut preview_rx) = {
            let mut jobs = self.jobs.lock().unwrap();
            match jobs.get(id) {
                Some(Job::Running { done_rx, preview_rx }) => (done_rx.clone(), preview_rx.clone()),
                Some(Job::Failed(msg, at)) if at.elapsed() < FAILURE_TTL => return Status::Failed(msg.clone()),
                _ => {
                    let (done_tx, done_rx) = watch::channel(None);
                    let (preview_tx, preview_rx) = watch::channel(None);
                    jobs.insert(
                        id.to_string(),
                        Job::Running {
                            done_rx: done_rx.clone(),
                            preview_rx: preview_rx.clone(),
                        },
                    );
                    let app = self.clone();
                    let id = id.to_string();
                    tokio::spawn(async move {
                        let outcome = app.render(&id, auto, preview_tx).await;
                        let mut jobs = app.jobs.lock().unwrap();
                        match &outcome {
                            Outcome::Done | Outcome::NotFound | Outcome::Preempted => jobs.remove(&id),
                            Outcome::Failed(msg) => jobs.insert(id.clone(), Job::Failed(msg.clone(), Instant::now())),
                        };
                        let _ = done_tx.send(Some(outcome));
                    });
                    (done_rx, preview_rx)
                }
            }
        };
        if allow_preview {
            if let Some(meta) = preview_rx.borrow().clone() {
                return Status::PreviewReady(meta);
            }
            enum WaitStep {
                Done(Option<Outcome>),
                Preview(Option<Value>),
            }
            let step = tokio::time::timeout(wait, async {
                tokio::select! {
                    r = done_rx.wait_for(|o| o.is_some()) => r.ok().map(|o| WaitStep::Done(o.clone())),
                    r = preview_rx.wait_for(|p| p.is_some()) => r.ok().map(|p| WaitStep::Preview(p.clone())),
                }
            })
            .await
            .ok()
            .flatten();
            return match step {
                Some(WaitStep::Preview(Some(meta))) => Status::PreviewReady(meta),
                Some(WaitStep::Done(Some(outcome))) => self.outcome_to_status(id, outcome).await,
                _ => Status::Pending,
            };
        }
        // Copied out so the channel's lock isn't held across an await.
        let finished = tokio::time::timeout(wait, done_rx.wait_for(|o| o.is_some()))
            .await
            .ok()
            .and_then(|r| r.ok().map(|o| o.clone()));
        match finished {
            Some(Some(outcome)) => self.outcome_to_status(id, outcome).await,
            _ => Status::Pending,
        }
    }

    async fn outcome_to_status(&self, id: &str, outcome: Outcome) -> Status {
        match outcome {
            Outcome::Done => self.meta(id).await.map_or(Status::Failed("the render left no details".into()), Status::Ready),
            Outcome::NotFound => Status::NotFound,
            Outcome::Preempted => Status::Pending,
            Outcome::Failed(msg) => Status::Failed(msg),
        }
    }

    /// Runs openfront-video for the game into temporary files, then moves
    /// them into place (details last) and records the video in `catalog.redb`.
    async fn render(&self, id: &str, auto: bool, preview_tx: watch::Sender<Option<Value>>) -> Outcome {
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
        let preview_path = tmp("preview.json");
        let progress_path = self.progress_path(id);
        let _ = tokio::fs::create_dir_all(self.dir.join("tmp")).await;
        let _ = tokio::fs::remove_file(&preview_path).await;
        let _ = tokio::fs::remove_file(&progress_path).await;
        let preview_watcher = async {
            for _ in 0..60 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if let Ok(bytes) = tokio::fs::read(&preview_path).await {
                    if let Ok(val) = serde_json::from_slice::<Value>(&bytes) {
                        let _ = preview_tx.send(Some(val));
                        break;
                    }
                }
            }
            std::future::pending::<()>().await
        };
        let work = async {
            let mut cmd = tokio::process::Command::new(&self.bin);
            cmd.arg(id)
                .arg("--out")
                .arg(tmp("mp4"))
                .arg("--thumbnail")
                .arg(tmp("png"))
                .arg("--meta")
                .arg(tmp("json"))
                .arg("--preview-meta")
                .arg(&preview_path)
                .arg("--progress-file")
                .arg(&progress_path)
                // Games from other engine versions stop right away (exit code 4): they
                // are checked for engine-path git diffs, or go to the TypeScript engine.
                .args(if auto || self.engines.is_some() { &["--same-engine"][..] } else { &[][..] })
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            let out = tokio::time::timeout(RENDER_TIMEOUT, cmd.output()).await?;
            let (Some(engines), Ok(out)) = (&self.engines, &out) else { return Ok(out) };
            if out.status.code() != Some(4) {
                return Ok(Ok(out.clone()));
            }
            let err = String::from_utf8_lossy(&out.stderr);
            let Some(commit) = err.split("played on engine version ").nth(1).and_then(|r| r.split(',').next()).map(str::trim) else {
                return Ok(Ok(out.clone()));
            };
            let rust_commit = err
                .split("this build follows ")
                .nth(1)
                .and_then(|r| r.split(|c: char| !c.is_ascii_hexdigit()).next())
                .unwrap_or("");
            if !rust_commit.is_empty() && engines.is_engine_compatible(commit, rust_commit, &self.catalog).await {
                println!(
                    "render {id}: commit {} has 0 engine changes vs Rust build, rendering with Rust engine",
                    &commit[..commit.len().min(9)]
                );
                let mut cmd2 = tokio::process::Command::new(&self.bin);
                cmd2.arg(id)
                    .arg("--out")
                    .arg(tmp("mp4"))
                    .arg("--thumbnail")
                    .arg(tmp("png"))
                    .arg("--meta")
                    .arg(tmp("json"))
                    .arg("--preview-meta")
                    .arg(&preview_path)
                    .arg("--progress-file")
                    .arg(&progress_path)
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true);
                return tokio::time::timeout(RENDER_TIMEOUT, cmd2.output()).await;
            }
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
            _ = preview_watcher => None,
            _ = preempted => None,
        };
        let _ = tokio::fs::remove_file(&preview_path).await;
        let _ = tokio::fs::remove_file(&progress_path).await;
        let _ = tokio::fs::remove_file(format!("{}.tmp", progress_path.display())).await;
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
                        Ok(()) => {
                            if let Ok(json_bytes) = tokio::fs::read(self.path(id, "json")).await {
                                if let Ok(meta) = serde_json::from_slice::<Value>(&json_bytes) {
                                    let mp4_len = tokio::fs::metadata(self.path(id, "mp4")).await.map_or(0, |m| m.len());
                                    let png_len = tokio::fs::metadata(self.path(id, "png")).await.map_or(0, |m| m.len());
                                    let now = now_secs();
                                    self.catalog.upsert_video(id, &meta, mp4_len + png_len + json_bytes.len() as u64, now, now);
                                }
                            }
                            Outcome::Done
                        }
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
main{max-width:960px;margin:0 auto;padding:24px 16px}a{color:#7cb7ff;text-decoration:none}a:hover{text-decoration:underline}\
h1{font-size:22px;margin:0 0 4px;color:#f3f4f6}h2{font-size:17px;margin:28px 0 12px;color:#e6e6e6;font-weight:600}\
p{color:#a9adb7;margin:0 0 16px}video,img{width:100%;height:auto;border-radius:6px;background:#000;display:block}\
form{display:flex;gap:8px;flex-wrap:wrap}input{flex:1;min-width:0;padding:9px 12px;border-radius:6px;border:1px solid #374151;background:#111827;color:#f3f4f6;font:inherit}input:focus{outline:none;border-color:#4785b5}\
button,.btn{padding:9px 16px;border-radius:6px;border:1px solid #4785b5;background:#2b547e;color:#f3f4f6;font:inherit;font-weight:500;cursor:pointer;display:inline-flex;align-items:center;gap:6px;text-decoration:none}button:hover,.btn:hover{background:#366799;text-decoration:none}\
.btn-sub{padding:5px 10px;font-size:13px;border-color:#374151;background:#111827;color:#d1d5db}.btn-sub:hover{border-color:#4785b5;background:#1c2536;color:#f3f4f6}\
.btn-sub.active{border-color:#4785b5;background:#2b547e;color:#f3f4f6}\
.toolbar{display:flex;flex-wrap:wrap;justify-content:space-between;align-items:center;gap:10px;margin-top:10px;padding:10px 12px;border-radius:6px;border:1px solid #2b303c;background:#111827}\
.tool-group{display:flex;flex-wrap:wrap;align-items:center;gap:6px}\
.tool-label{font-size:12px;color:#9ca3af;margin-right:4px;font-weight:600;text-transform:uppercase;letter-spacing:.04em}\
.kbd-hint{font-size:12px;color:#8b929e;margin-top:8px}\
.status-card{margin:20px 0;padding:18px 20px;border-radius:6px;border:1px solid #374151;background:#111827}\
.progress-head{display:flex;justify-content:space-between;align-items:baseline;margin-bottom:10px;font-weight:600;color:#f3f4f6}\
.progress-pct{color:#d1d5db;font-variant-numeric:tabular-nums;font-size:16px}\
.progress-track{height:8px;border-radius:4px;background:#1f2937;overflow:hidden;margin-bottom:12px}\
.progress-fill{height:100%;width:0%;background:#4785b5;transition:width .3s ease}\
.progress-meta{display:flex;justify-content:space-between;gap:12px;color:#9ca3af;font-size:13px;font-variant-numeric:tabular-nums}\
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(260px,1fr));gap:14px}\
.card{display:flex;flex-direction:column;border-radius:6px;border:1px solid #2b303c;background:#111827;overflow:hidden;color:inherit}\
.card:hover{border-color:#4785b5;text-decoration:none}\
.card img{aspect-ratio:1/1;object-fit:cover;border-radius:0;border-bottom:1px solid #222630;background:#16181d}\
.card-body{padding:10px 12px;display:flex;flex-direction:column;gap:6px}\
.card-title{font-weight:600;color:#f3f4f6;font-size:14px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}\
.card-meta{display:flex;flex-wrap:wrap;gap:6px;font-size:12px;color:#a9adb7}\
.pill{background:#16181d;border:1px solid #2b303c;border-radius:4px;padding:1px 6px}\
.pill-win{border-color:#b48a1e;color:#f3e3a3}";

fn page(head: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"theme-color\" content=\"#4785b5\">{head}<style>{STYLE}</style></head><body><main>{body}</main></body></html>"
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

/// Collects up to `limit` recently rendered games from the `redb` catalog, scanning
/// `<DATA_DIR>` and `<DATA_DIR>/videos` for `*.mp4` files as a fallback if needed.
async fn recent_timelapses(app: &App, limit: usize) -> Vec<storage::StoredVideoRecord> {
    let mut recent = app.catalog.list_recent(limit);
    if recent.len() < limit {
        for scan_dir in [app.dir.clone(), app.dir.join("videos")] {
            let Ok(mut rd) = tokio::fs::read_dir(&scan_dir).await else { continue };
            while let Ok(Some(entry)) = rd.next_entry().await {
                if recent.len() >= limit {
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Some(id) = name.strip_suffix(".mp4") else { continue };
                if !valid_id(id) || recent.iter().any(|r| r.id == id) {
                    continue;
                }
                let Ok(mp4_meta) = entry.metadata().await else { continue };
                let rendered = mp4_meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                recent.push(storage::StoredVideoRecord {
                    id: id.to_string(),
                    map: "?".to_string(),
                    mode: "?".to_string(),
                    players: 0,
                    bytes: mp4_meta.len(),
                    render_secs: 0.0,
                    rendered,
                    last_used: rendered,
                    meta: Value::Null,
                });
            }
        }
        recent.sort_by(|a, b| b.rendered.cmp(&a.rendered).then_with(|| b.last_used.cmp(&a.last_used)));
        recent.truncate(limit);
    }
    recent
}

async fn index(State(app): State<Arc<App>>, headers: HeaderMap) -> Html<String> {
    app.events.view("", "home", &headers);
    let recent = recent_timelapses(&app, 12).await;
    let gallery_html = if recent.is_empty() {
        "<p>No timelapses rendered yet — paste a match URL or ID above to render the first one.</p>".to_string()
    } else {
        let mut cards = String::from("<div class=\"grid\">");
        for g in &recent {
            let id_esc = esc(&g.id);
            let has_meta = g.map != "?" && !g.map.is_empty();
            let title = if has_meta {
                format!("{} · {}", esc(&g.map), esc(&g.mode))
            } else {
                format!("Match {id_esc}")
            };
            let mut pills = format!("<span class=\"pill\">#{id_esc}</span>");
            if let Some(w) = g.meta["winner"].as_str().filter(|s| !s.is_empty()) {
                if let Some(pct) = g.meta["winnerPct"].as_f64() {
                    pills.push_str(&format!("<span class=\"pill pill-win\">★ {} ({pct:.1}%)</span>", esc(w)));
                } else {
                    pills.push_str(&format!("<span class=\"pill pill-win\">★ {}</span>", esc(w)));
                }
            }
            if g.players > 0 {
                pills.push_str(&format!("<span class=\"pill\">{} players</span>", g.players));
            }
            if g.bytes > 0 {
                pills.push_str(&format!("<span class=\"pill\">{}</span>", fmt_bytes(g.bytes)));
            }
            if g.render_secs > 0.0 {
                pills.push_str(&format!("<span class=\"pill\">{:.1}s render</span>", g.render_secs));
            }
            cards.push_str(&format!(
                "<a class=\"card\" href=\"/{id_esc}\">\
<img src=\"/thumb/{id_esc}.png\" alt=\"{title}\" loading=\"lazy\" onerror=\"this.style.display='none'\">\
<div class=\"card-body\"><div class=\"card-title\">{title}</div><div class=\"card-meta\">{pills}</div></div></a>"
            ));
        }
        cards.push_str("</div>");
        cards
    };
    let body = format!(
        "<h1>openfront.video</h1><p>Paste an OpenFront.io match URL or ID to watch or render a full-game timelapse. \
Or swap <b>openfront.io</b> for <b>openfront.video</b> in a game link.</p>\
<form id=\"paste-form\" action=\"/go\">\
<input id=\"game-input\" name=\"game\" placeholder=\"https://openfront.io/join/cAj8yxzNsH or cAj8yxzNsH\" required autofocus>\
<button type=\"submit\">Watch / Render Timelapse</button></form>\
<script>\
document.getElementById('paste-form').addEventListener('submit',function(e){{\
var raw=(document.getElementById('game-input').value||'').trim();\
var m=raw.match(/(?:\\/(?:game|join)\\/)([A-Za-z0-9_-]{{4,32}})/)||raw.match(/^([A-Za-z0-9_-]{{4,32}})$/);\
if(m&&m[1]){{e.preventDefault();window.location.href='/'+m[1];}}\
}});\
</script>\
<section class=\"gallery\"><h2>Recent Timelapses</h2>{gallery_html}</section>"
    );
    page_with_ads(
        "<title>openfront.video</title><meta property=\"og:title\" content=\"openfront.video\">\
<meta property=\"og:description\" content=\"Timelapse videos of OpenFront.io games\">",
        &body,
    )
}

#[derive(Deserialize)]
struct Go {
    game: String,
}

async fn go(Query(q): Query<Go>) -> Response {
    match game_id_from(&q.game) {
        Some(id) => Redirect::to(&format!("/{id}")).into_response(),
        None => (StatusCode::BAD_REQUEST, page("<title>openfront.video</title>", "<h1>That doesn't look like a game link</h1><p><a href=\"/\">Try again</a></p>")).into_response(),
    }
}

/// Link previewers (Discord and friends) receive immediate `<meta>` Open Graph tags
/// once `--preview-meta` is ready (~0.15s), and their media proxy waits on `/video/<id>.mp4`.
fn is_previewer(headers: &HeaderMap) -> bool {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    ["discordbot", "twitterbot", "slackbot", "telegrambot", "facebookexternalhit", "whatsapp", "embedly"].iter().any(|b| ua.contains(b))
}

async fn game_page(app: Arc<App>, id: String, headers: HeaderMap) -> Response {
    if !valid_id(&id) {
        return (StatusCode::NOT_FOUND, page("<title>Not found</title>", "<h1>Not a game ID</h1>")).into_response();
    }
    let previewer = is_previewer(&headers);
    // Link previewers wait up to 3s for `--preview-meta` (~0.15s); human browsers
    // wait briefly (250ms) so already-finished or near-instant renders resolve,
    // otherwise immediately receive the live progress UI.
    let wait = if previewer { Duration::from_secs(3) } else { Duration::from_millis(250) };
    let base = &app.public_url;
    let video = format!("{base}/video/{id}.mp4");
    let thumb = format!("{base}/thumb/{id}.png");
    let url = format!("{base}/game/{id}");
    let source = format!("https://openfront.io/game/{id}");
    match app.status_with_preview(&id, wait, false, previewer).await {
        Status::Ready(meta) | Status::PreviewReady(meta) => {
            cleanup::touch(&app.catalog, &id);
            app.events.view(&id, "page", &headers);
            let s = |k: &str| meta[k].as_str().unwrap_or("?").to_string();
            let (w, h) = (meta["width"].as_u64().unwrap_or(1280), meta["height"].as_u64().unwrap_or(1280));
            let minutes = (meta["durationSeconds"].as_f64().unwrap_or(0.0) / 60.0).round();
            let has_meta = s("map") != "?";
            let title = if has_meta { format!("{} · {}", s("map"), s("mode")) } else { format!("OpenFront game {id}") };
            let mut desc = if has_meta {
                format!("OpenFront.io game {id}: {} players, {minutes} min, as a timelapse", meta["players"])
            } else {
                format!("OpenFront.io game {id} timelapse")
            };
            if let Some(winner) = meta["winner"].as_str().filter(|s| !s.is_empty()) {
                if let Some(pct) = meta["winnerPct"].as_f64() {
                    desc.push_str(&format!(" — Won by {winner} ({pct:.1}%)"));
                } else {
                    desc.push_str(&format!(" — Won by {winner}"));
                }
            }
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
                "<h1>{t}</h1><p>{d}. <a href=\"{source}\">Replay on OpenFront</a> · <a href=\"/video/{id}.mp4\" download>Download MP4</a> · <a href=\"/\">All timelapses</a></p>\
<video id=\"ofv-video\" src=\"/video/{id}.mp4\" poster=\"/thumb/{id}.png\" width=\"{w}\" height=\"{h}\" controls autoplay muted loop playsinline></video>\
<div class=\"toolbar\">\
<div class=\"tool-group\">\
<span class=\"tool-label\">Speed</span>\
<button type=\"button\" class=\"btn-sub\" data-rate=\"0.25\">0.25x</button>\
<button type=\"button\" class=\"btn-sub\" data-rate=\"0.5\">0.5x</button>\
<button type=\"button\" class=\"btn-sub active\" data-rate=\"1\">1x</button>\
<button type=\"button\" class=\"btn-sub\" data-rate=\"1.5\">1.5x</button>\
<button type=\"button\" class=\"btn-sub\" data-rate=\"2\">2x</button>\
</div>\
<div class=\"tool-group\">\
<span class=\"tool-label\">Scrub</span>\
<button type=\"button\" class=\"btn-sub\" id=\"btn-start\" title=\"Jump to start\">⏮ Start</button>\
<button type=\"button\" class=\"btn-sub\" id=\"btn-prev\" title=\"Previous frame (,)\">◀ -1f</button>\
<button type=\"button\" class=\"btn-sub\" id=\"btn-next\" title=\"Next frame (.)\">+1f ▶</button>\
<button type=\"button\" class=\"btn-sub\" id=\"btn-end\" title=\"Jump to final victory board\">⏭ Victory</button>\
</div>\
<div class=\"tool-group\">\
<button type=\"button\" class=\"btn-sub\" id=\"btn-copy\">Copy Discord Link</button>\
</div>\
</div>\
<div class=\"kbd-hint\">Shortcuts: <b>Space</b> Play/Pause · <b>,</b> / <b>.</b> Step 1 Frame · <b>[</b> / <b>]</b> Change Speed</div>\
<script>\
(function(){{\
var v=document.getElementById('ofv-video');\
if(!v)return;\
var rates=[0.25,0.5,1,1.5,2];\
var btns=document.querySelectorAll('[data-rate]');\
var setRate=function(r){{\
v.playbackRate=r;\
btns.forEach(function(b){{b.classList.toggle('active',Number(b.getAttribute('data-rate'))===r);}});\
}};\
btns.forEach(function(b){{b.addEventListener('click',function(){{setRate(Number(b.getAttribute('data-rate')));}});}});\
var step=function(dir){{v.pause();v.currentTime=Math.max(0,Math.min(v.duration||32,v.currentTime+dir*(1/30)));}};\
document.getElementById('btn-start').addEventListener('click',function(){{v.currentTime=0;}});\
document.getElementById('btn-prev').addEventListener('click',function(){{step(-1);}});\
document.getElementById('btn-next').addEventListener('click',function(){{step(1);}});\
document.getElementById('btn-end').addEventListener('click',function(){{v.pause();v.currentTime=Math.max(0,(v.duration||32)-1.5);}});\
var cp=document.getElementById('btn-copy');\
cp.addEventListener('click',function(){{\
navigator.clipboard.writeText(window.location.origin+'/game/{id}').then(function(){{\
cp.textContent='Copied Discord Link!';\
setTimeout(function(){{cp.textContent='Copy Discord Link';}},2000);\
}});\
}});\
window.addEventListener('keydown',function(e){{\
if(e.target&&(e.target.tagName==='INPUT'||e.target.tagName==='TEXTAREA'))return;\
if(e.key===','){{e.preventDefault();step(-1);}}\
else if(e.key==='.'){{e.preventDefault();step(1);}}\
else if(e.key==='['||e.key===']'){{\
e.preventDefault();\
var idx=rates.indexOf(v.playbackRate);\
if(idx<0)idx=2;\
idx=Math.max(0,Math.min(rates.length-1,idx+(e.key===']'?1:-1)));\
setRate(rates[idx]);\
}}\
}});\
}})();\
</script>"
            );
            page_with_ads(&head, &body).into_response()
        }
        // Link previewers never get a 200 OK text-only pending card, so Discord
        // does not permanently cache a broken embed without og:video.
        Status::Pending if previewer => {
            app.events.view(&id, "pending", &headers);
            (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "5")], "still rendering").into_response()
        }
        Status::Pending => {
            app.events.view(&id, "pending", &headers);
            let pmeta = app.preview_meta(&id).await;
            let (heading, sub) = if let Some(ref m) = pmeta {
                let map = m["map"].as_str().unwrap_or("?");
                let mode = m["mode"].as_str().unwrap_or("?");
                let players = m["players"].as_u64().unwrap_or(0);
                if map != "?" {
                    (
                        esc(&format!("{map} · {mode}")),
                        format!("OpenFront.io game {id} ({players} players) · <a href=\"{source}\">Replay on OpenFront</a> · <a href=\"/\">Home</a>"),
                    )
                } else {
                    (
                        format!("Rendering game {id}…"),
                        format!("OpenFront.io game {id} · <a href=\"{source}\">Replay on OpenFront</a> · <a href=\"/\">Home</a>"),
                    )
                }
            } else {
                (
                    format!("Rendering game {id}…"),
                    format!("OpenFront.io game {id} · <a href=\"{source}\">Replay on OpenFront</a> · <a href=\"/\">Home</a>"),
                )
            };
            let head = format!(
                "<title>Rendering {id}</title>\
<noscript><meta http-equiv=\"refresh\" content=\"3\"></noscript>\
<meta property=\"og:site_name\" content=\"openfront.video\"><meta property=\"og:title\" content=\"OpenFront game {id}\">\
<meta property=\"og:description\" content=\"The timelapse is still rendering\"><meta property=\"og:url\" content=\"{url}\">"
            );
            let body = format!(
                "<h1 id=\"match-title\">{heading}</h1>\
<p id=\"match-sub\">{sub}</p>\
<div id=\"stage\">\
<div class=\"status-card\">\
<div class=\"progress-head\"><span id=\"status-label\">Rendering timelapse…</span><span id=\"pct-text\" class=\"progress-pct\">0%</span></div>\
<div class=\"progress-track\"><div id=\"progress-fill\" class=\"progress-fill\" style=\"width:0%\"></div></div>\
<div class=\"progress-meta\"><span id=\"tick-text\">Starting simulation…</span><span id=\"elapsed-text\">0.0s</span></div>\
</div>\
</div>\
<script>\
(function(){{\
var id={id_json};\
var fmt=function(n){{return Number(n||0).toLocaleString();}};\
var swapped=false;\
var poll=function(){{\
if(swapped)return;\
fetch('/api/v1/status/'+encodeURIComponent(id),{{cache:'no-store'}})\
.then(function(r){{return r.json();}})\
.then(function(d){{\
if(!d||swapped)return;\
if(d.map&&d.map!=='?'){{document.getElementById('match-title').textContent=d.map+' · '+(d.mode||'');}}\
if(d.status==='ready'){{\
swapped=true;\
var fill=document.getElementById('progress-fill');if(fill)fill.style.width='100%';\
var pct=document.getElementById('pct-text');if(pct)pct.textContent='100%';\
window.location.reload();\
return;\
}}\
if(d.status==='failed'){{\
swapped=true;\
var lbl=document.getElementById('status-label');if(lbl)lbl.textContent='Render failed';\
var tt=document.getElementById('tick-text');if(tt)tt.textContent=d.error||'The renderer stopped.';\
return;\
}}\
var p=Math.min(100,Math.max(0,Math.round(Number(d.percent||0))));\
var fillEl=document.getElementById('progress-fill');if(fillEl)fillEl.style.width=p+'%';\
var pctEl=document.getElementById('pct-text');if(pctEl)pctEl.textContent=p+'%';\
if(d.status==='rendering'&&d.total_ticks){{\
var tickEl=document.getElementById('tick-text');\
if(tickEl)tickEl.textContent='Tick '+fmt(d.tick)+' / '+fmt(d.total_ticks)+' • Frame '+fmt(d.frame)+' / '+fmt(d.total_frames);\
}}\
if(typeof d.elapsed_secs==='number'){{\
var elEl=document.getElementById('elapsed-text');\
if(elEl)elEl.textContent=d.elapsed_secs.toFixed(1)+'s';\
}}\
}}).catch(function(){{}});\
}};\
poll();\
setInterval(poll,1000);\
}})();\
</script>",
                id_json = serde_json::to_string(&id).unwrap_or_else(|_| format!("\"{id}\""))
            );
            page(&head, &body).into_response()
        }
        // Link previewers get no page at all, so they cache nothing and fetch
        // again the next time the link is posted.
        Status::NotFound if previewer => {
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

async fn root_id_or_media(State(app): State<Arc<App>>, Path(name): Path<String>, req: Request) -> Response {
    if name.ends_with(".mp4") {
        return media(app, name, "mp4", req).await;
    }
    if name.ends_with(".png") {
        return media(app, name, "png", req).await;
    }
    let headers = req.headers().clone();
    game_page(app, name, headers).await
}

/// /video/<id>.mp4 and /thumb/<id>.png: rendered on demand, then served
/// from disk with byte ranges and long caching.
async fn media(app: Arc<App>, name: String, ext: &str, req: Request) -> Response {
    let Some(id) = name.strip_suffix(&format!(".{ext}")).filter(|id| valid_id(id)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match app.status(id, app.video_wait).await {
        Status::Ready(_) => {
            cleanup::touch(&app.catalog, id);
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
        Status::Pending | Status::PreviewReady(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "10")], "still rendering").into_response()
        }
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

/// Live render progress polling endpoint (`GET /api/v1/status/{id}`).
async fn api_status_handler(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if !valid_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(json!({"status": "invalid_id", "percent": 0})),
        )
            .into_response();
    }
    if app.catalog.has_video(&id) || tokio::fs::metadata(app.path(&id, "mp4")).await.is_ok() {
        let mut body = json!({
            "status": "ready",
            "percent": 100,
        });
        if let Some(meta) = app.meta(&id).await {
            if let Some(obj) = body.as_object_mut() {
                for key in ["map", "mode", "players", "winner", "winnerPct", "durationSeconds", "ticks", "hashesChecked", "hashMismatches"] {
                    if !meta[key].is_null() {
                        obj.insert(key.into(), meta[key].clone());
                    }
                }
            }
        }
        return ([(header::CACHE_CONTROL, "no-store")], axum::Json(body)).into_response();
    }
    let prog_path = app.progress_path(&id);
    let prog_bytes = match tokio::fs::read(&prog_path).await {
        Ok(b) => Some(b),
        Err(_) => tokio::fs::read(app.dir.join(format!(".{id}.tmp.progress.json"))).await.ok(),
    };
    if let Some(bytes) = prog_bytes {
        if let Ok(prog) = serde_json::from_slice::<Value>(&bytes) {
            let tick = prog["tick"].as_u64().or_else(|| prog["current_tick"].as_u64()).unwrap_or(0);
            let total_ticks = prog["total_ticks"].as_u64().or_else(|| prog["ticks"].as_u64()).unwrap_or(0);
            let frame = prog["frame"]
                .as_u64()
                .or_else(|| prog["encoded_frames"].as_u64())
                .or_else(|| prog["frames_encoded"].as_u64())
                .unwrap_or(0);
            let total_frames = prog["total_frames"].as_u64().or_else(|| prog["frames"].as_u64()).unwrap_or(0);
            let elapsed_secs = prog["elapsed_secs"].as_f64().or_else(|| prog["elapsed"].as_f64()).unwrap_or(0.0);
            let percent = prog["percent"]
                .as_f64()
                .or_else(|| prog["progress"].as_f64().map(|p| if p <= 1.0 { p * 100.0 } else { p }))
                .unwrap_or_else(|| {
                    if total_ticks > 0 {
                        (tick as f64 / total_ticks as f64) * 100.0
                    } else {
                        0.0
                    }
                })
                .clamp(0.0, 99.9);
            let mut body = json!({
                "status": "rendering",
                "percent": (percent * 10.0).round() / 10.0,
                "tick": tick,
                "total_ticks": total_ticks,
                "frame": frame,
                "total_frames": total_frames,
                "elapsed_secs": (elapsed_secs * 10.0).round() / 10.0,
            });
            if let Some(pmeta) = app.preview_meta(&id).await {
                if let Some(obj) = body.as_object_mut() {
                    if let Some(map) = pmeta["map"].as_str() {
                        obj.insert("map".into(), json!(map));
                    }
                    if let Some(mode) = pmeta["mode"].as_str() {
                        obj.insert("mode".into(), json!(mode));
                    }
                    if let Some(players) = pmeta["players"].as_u64() {
                        obj.insert("players".into(), json!(players));
                    }
                }
            }
            return ([(header::CACHE_CONTROL, "no-store")], axum::Json(body)).into_response();
        }
    }
    enum JobState {
        Running(Option<Value>),
        Failed(String),
        None,
    }
    let job_state = {
        let jobs = app.jobs.lock().unwrap();
        match jobs.get(&id) {
            Some(Job::Running { preview_rx, .. }) => JobState::Running(preview_rx.borrow().clone()),
            Some(Job::Failed(msg, at)) if at.elapsed() < FAILURE_TTL => JobState::Failed(msg.clone()),
            _ => JobState::None,
        }
    };
    match job_state {
        JobState::Running(preview) => {
            let mut body = json!({
                "status": "starting",
                "percent": 0,
            });
            if let Some(pmeta) = preview {
                if let Some(obj) = body.as_object_mut() {
                    if let Some(map) = pmeta["map"].as_str() {
                        obj.insert("map".into(), json!(map));
                    }
                    if let Some(mode) = pmeta["mode"].as_str() {
                        obj.insert("mode".into(), json!(mode));
                    }
                    if let Some(players) = pmeta["players"].as_u64() {
                        obj.insert("players".into(), json!(players));
                    }
                }
            }
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(body)).into_response()
        }
        JobState::Failed(msg) => (
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(json!({
                "status": "failed",
                "percent": 0,
                "error": msg,
            })),
        )
            .into_response(),
        JobState::None => (
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(json!({
                "status": "not_started",
                "percent": 0,
            })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct BotRenderRequest {
    /// Game ID or full https://openfront.io/game/<id> URL
    game: String,
    /// Optional: if true, wait up to `video_wait` for completion; default false (queue immediately and return 202/200)
    #[serde(default)]
    wait: bool,
}

fn api_authorized(headers: &HeaderMap) -> Option<bool> {
    let token = std::env::var("RENDER_API_TOKEN").ok().filter(|t| !t.is_empty())?;
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();
    let (a, b) = (given.as_bytes(), token.as_bytes());
    let same = a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0;
    Some(same)
}

/// Optional generic Bot Render API (`POST /api/v1/render`), enabled when `RENDER_API_TOKEN` is set.
async fn api_render(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::Json(req): axum::Json<BotRenderRequest>,
) -> Response {
    match api_authorized(&headers) {
        None => return StatusCode::NOT_FOUND.into_response(),
        Some(false) => return StatusCode::UNAUTHORIZED.into_response(),
        Some(true) => {}
    }
    let Some(id) = game_id_from(&req.game) else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid game ID or URL"}))).into_response();
    };
    let wait = if req.wait { app.video_wait } else { Duration::from_millis(250) };
    let (code, status_str, ready_meta) = match app.status(&id, wait).await {
        Status::Ready(m) => (StatusCode::OK, "ready", Some(m)),
        Status::Pending | Status::PreviewReady(_) => (StatusCode::ACCEPTED, "rendering", None),
        Status::NotFound => (StatusCode::NOT_FOUND, "not_found", None),
        Status::Failed(_) => (StatusCode::INTERNAL_SERVER_ERROR, "failed", None),
    };
    let base = &app.public_url;
    let mut payload = json!({
        "id": id,
        "status": status_str,
        "page_url": format!("{base}/game/{id}"),
        "video_url": format!("{base}/video/{id}.mp4"),
        "thumb_url": format!("{base}/thumb/{id}.png"),
    });
    if let (Some(meta), Some(obj)) = (ready_meta, payload.as_object_mut()) {
        for key in ["map", "mode", "players", "winner", "winnerPct", "durationSeconds", "ticks", "hashesChecked", "hashMismatches"] {
            if !meta[key].is_null() {
                obj.insert(key.into(), meta[key].clone());
            }
        }
    }
    (code, axum::Json(payload)).into_response()
}

#[tokio::main]
async fn main() {
    let dir = PathBuf::from(env_or("DATA_DIR", "./data"));
    std::fs::create_dir_all(&dir).expect("DATA_DIR");
    std::fs::create_dir_all(dir.join("tmp")).expect("DATA_DIR/tmp");
    // Leftovers of renders that were cut off by a restart.
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        if e.file_name().to_string_lossy().contains(".tmp.") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("tmp")) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().contains(".progress.json") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let secs = |k: &str, d: &str| Duration::from_secs(env_or(k, d).parse().expect(k));
    let catalog = storage::Catalog::open(&dir);
    let events = stats::EventLog::open(&dir, catalog.clone());
    let engines = engines::Engines::from_env(&dir);
    let app = Arc::new(App {
        dir,
        bin: PathBuf::from(env_or("OPENFRONT_VIDEO_BIN", "openfront-video")),
        public_url: env_or("PUBLIC_URL", "https://openfront.video").trim_end_matches('/').to_string(),
        embed_wait: secs("EMBED_WAIT_SECS", "25"),
        video_wait: secs("VIDEO_WAIT_SECS", "120"),
        slots: slots::Slots::new(env_or("MAX_RENDERS", "2").parse().expect("MAX_RENDERS")),
        jobs: Mutex::new(HashMap::new()),
        rendered: tokio::sync::Notify::new(),
        catalog,
        events,
        auto_status: Mutex::new(stats::AutoStatus::default()),
        started: now_secs(),
        engines,
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
        .route("/api/v1/render", post(api_render))
        .route("/api/v1/status/{id}", get(api_status_handler))
        .route("/game/{id}", get(game))
        .route("/join/{id}", get(game))
        .route("/{prefix}/game/{id}", get(game_prefixed))
        .route("/video/{name}", get(video))
        .route("/thumb/{name}", get(thumb))
        .route("/{id}", get(root_id_or_media))
        .with_state(app.clone());
    let addr = format!("0.0.0.0:{}", env_or("PORT", "8080"));
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    println!("openfront-web on {addr}, videos in {}, public URL {}, embed wait {}s", app.dir.display(), app.public_url, app.embed_wait.as_secs());
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .unwrap();
}
