//! The admin dashboard: an event log of renders, views and cleanups
//! (`DATA_DIR/catalog.redb`, 30 days kept), and `/admin` with its JSON stats.
//! `/admin` needs `ADMIN_PASSWORD` (HTTP basic auth, any user name); without it
//! the dashboard is off.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::storage::{Catalog, StoredVideoRecord};
use crate::App;

const KEEP: u64 = 30 * 24 * 3600;

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// outcome: done, failed, not_found, preempted
    Render { t: u64, id: String, auto: bool, outcome: String, secs: f64, #[serde(default)] msg: String },
    /// what: home, page (a finished video), pending (still rendering),
    /// not_found, failed, video (a play). client: see `client_of`. v: the
    /// visitor, a salted hash of the IP (never the IP itself).
    View {
        t: u64,
        #[serde(default)]
        id: String,
        what: String,
        #[serde(default)]
        client: String,
        #[serde(default)]
        v: String,
    },
    Cleanup { t: u64, removed: u64, bytes: u64 },
}

impl Event {
    pub fn t(&self) -> u64 {
        match self {
            Event::Render { t, .. } | Event::View { t, .. } | Event::Cleanup { t, .. } => *t,
        }
    }
}

pub struct EventLog {
    catalog: Arc<Catalog>,
    /// Secret mixed into visitor hashes, kept in DATA_DIR/.visitor-salt.
    salt: u64,
}

/// Who sent a request, from its User-Agent.
pub fn client_of(headers: &HeaderMap) -> &'static str {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let has = |s: &str| ua.contains(s);
    if has("discordbot") {
        "discord"
    } else if has("telegrambot") {
        // Before X: Telegram's agent is "TelegramBot (like TwitterBot)".
        "telegram"
    } else if has("twitterbot") {
        "twitter"
    } else if has("slackbot") || has("slack-imgproxy") {
        "slack"
    } else if has("whatsapp") {
        "whatsapp"
    } else if has("facebookexternalhit") || has("facebot") || has("meta-externalagent") {
        "facebook"
    } else if ua.is_empty()
        || ["bot", "crawl", "spider", "curl", "wget", "python", "go-http", "headless"].iter().any(|s| has(s))
    {
        "other_bot"
    } else {
        "browser"
    }
}

/// The client address, from the headers Caddy and Cloudflare set.
fn client_ip(headers: &HeaderMap) -> String {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string());
    h("cf-connecting-ip")
        .or_else(|| h("x-forwarded-for").and_then(|v| v.split(',').next().map(|s| s.trim().to_string())))
        .or_else(|| h("x-real-ip"))
        .unwrap_or_default()
}

fn fnv(salt: u64, s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64 ^ salt;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

fn load_salt(dir: &Path) -> u64 {
    let path = dir.join(".visitor-salt");
    if let Some(v) = std::fs::read_to_string(&path).ok().and_then(|s| u64::from_str_radix(s.trim(), 16).ok()) {
        return v;
    }
    // RandomState is seeded from the OS's random source.
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos());
    let v = h.finish();
    let _ = std::fs::write(&path, format!("{v:016x}"));
    v
}

impl EventLog {
    /// Connects the event log to the `redb` catalog and loads the visitor salt.
    pub fn open(dir: &Path, catalog: Arc<Catalog>) -> EventLog {
        EventLog { catalog, salt: load_salt(dir) }
    }

    pub fn record(&self, e: Event) {
        self.catalog.append_event(&e);
    }

    pub fn render(&self, id: &str, auto: bool, outcome: &str, secs: f64, msg: &str) {
        self.record(Event::Render { t: now_secs(), id: id.into(), auto, outcome: outcome.into(), secs, msg: msg.into() });
    }

    pub fn view(&self, id: &str, what: &str, headers: &HeaderMap) {
        let ip = client_ip(headers);
        let v = if ip.is_empty() { String::new() } else { format!("{:012x}", fnv(self.salt, &ip) >> 16) };
        self.record(Event::View { t: now_secs(), id: id.into(), what: what.into(), client: client_of(headers).into(), v });
    }
}

/// What the auto renderer reports for the dashboard.
#[derive(Default)]
pub struct AutoStatus {
    pub enabled: bool,
    pub queue_len: usize,
    /// The queue, next first: game ID and when it ended (Unix seconds).
    pub queue: Vec<(String, u64)>,
    pub last_poll: Option<u64>,
    pub last_new: usize,
    pub min_players: u64,
    pub max_age_mins: i64,
}

fn authorized(headers: &HeaderMap) -> Option<bool> {
    let password = std::env::var("ADMIN_PASSWORD").ok().filter(|p| !p.is_empty())?;
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64_decode(b.trim()))
        .and_then(|creds| String::from_utf8(creds).ok())
        .and_then(|creds| creds.split_once(':').map(|(_, p)| p.to_string()))
        .unwrap_or_default();
    // Constant-time compare.
    let (a, b) = (given.as_bytes(), password.as_bytes());
    let same = a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0;
    Some(same)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((acc >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

fn guard(headers: &HeaderMap) -> Option<Response> {
    match authorized(headers) {
        None => Some(StatusCode::NOT_FOUND.into_response()),
        Some(false) => Some(
            (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Basic realm=\"openfront.video admin\"")], "admin login required")
                .into_response(),
        ),
        Some(true) => None,
    }
}

pub async fn page(headers: HeaderMap) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }
    ([(header::CACHE_CONTROL, "no-store")], Html(include_str!("admin.html"))).into_response()
}

pub async fn api(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }
    let app2 = app.clone();
    let stats = tokio::task::spawn_blocking(move || build(&app2)).await.unwrap_or(Value::Null);
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(stats)).into_response()
}

fn build(app: &App) -> Value {
    let now = now_secs();
    let games = app.catalog.list_videos();
    let events = app.catalog.list_events(now.saturating_sub(KEEP));
    let used: u64 = games.iter().map(|g| g.bytes).sum();

    // Hourly buckets, oldest first, over the last 48 hours.
    const HOURS: u64 = 48;
    let start = (now / 3600 - (HOURS - 1)) * 3600;
    let mut hourly: Vec<HashMap<&str, u64>> = vec![HashMap::new(); HOURS as usize];
    let mut views_by_game: HashMap<String, u64> = HashMap::new();
    let (mut done24, mut failed24, mut nf24, mut pre24, mut views24, mut embeds24) = (0, 0, 0, 0, 0, 0);
    // Visitors over 24 h: requests and distinct visitors per client, requests per page kind.
    let mut by_client: HashMap<String, (u64, std::collections::HashSet<String>)> = HashMap::new();
    let mut by_what: HashMap<String, (u64, u64)> = HashMap::new();
    let mut uniques: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut render_secs24: Vec<f64> = Vec::new();
    let mut failures: HashMap<String, (u64, u64, String)> = HashMap::new();
    let mut recent: Vec<&Event> = Vec::new();
    let (mut cleaned_games, mut cleaned_bytes) = (0u64, 0u64);
    for e in &events {
        let t = e.t();
        let day = now.saturating_sub(t) < 86400;
        let bucket = (t >= start).then(|| ((t - start) / 3600) as usize).filter(|&b| b < HOURS as usize);
        match e {
            Event::Render { auto, outcome, secs, msg, .. } => {
                let key = match (outcome.as_str(), auto) {
                    ("done", true) => "auto_done",
                    ("done", false) => "request_done",
                    ("failed", _) => "failed",
                    ("not_found", _) => "not_found",
                    _ => "preempted",
                };
                if let Some(b) = bucket {
                    *hourly[b].entry(key).or_default() += 1;
                }
                if day {
                    match outcome.as_str() {
                        "done" => {
                            done24 += 1;
                            render_secs24.push(*secs);
                        }
                        "failed" => failed24 += 1,
                        "not_found" => nf24 += 1,
                        _ => pre24 += 1,
                    }
                }
                if outcome == "failed" && now.saturating_sub(t) < 7 * 86400 {
                    let f = failures.entry(msg.clone()).or_insert((0, 0, String::new()));
                    f.0 += 1;
                    if t >= f.1 {
                        f.1 = t;
                        if let Event::Render { id, .. } = e {
                            f.2 = id.clone();
                        }
                    }
                }
                recent.push(e);
            }
            Event::View { id, what, client, v, .. } => {
                // Logs from before visitor sources: "embed" was a link previewer.
                let client = if what == "embed" { "other_bot" } else if client.is_empty() { "browser" } else { client.as_str() };
                let what = if what == "embed" { "page" } else { what.as_str() };
                let person = client == "browser";
                if let Some(b) = bucket {
                    let key = if !person {
                        "embeds"
                    } else if what == "video" {
                        "video_views"
                    } else {
                        "page_views"
                    };
                    *hourly[b].entry(key).or_default() += 1;
                    let src = match client {
                        "browser" => "src_browser",
                        "discord" => "src_discord",
                        _ => "src_bots",
                    };
                    *hourly[b].entry(src).or_default() += 1;
                }
                if person && matches!(what, "page" | "video") && !id.is_empty() {
                    *views_by_game.entry(id.clone()).or_default() += 1;
                }
                if day {
                    let c = by_client.entry(client.to_string()).or_default();
                    c.0 += 1;
                    if !v.is_empty() {
                        c.1.insert(v.clone());
                    }
                    let w = by_what.entry(what.to_string()).or_default();
                    if person {
                        w.0 += 1;
                        views24 += 1;
                        if !v.is_empty() {
                            uniques.insert(v.clone());
                        }
                    } else {
                        w.1 += 1;
                        embeds24 += 1;
                    }
                }
            }
            Event::Cleanup { removed, bytes, t } => {
                if now.saturating_sub(*t) < 86400 {
                    cleaned_games += removed;
                    cleaned_bytes += bytes;
                }
            }
        }
    }
    let hourly: Vec<Value> = hourly
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let g = |k: &str| h.get(k).copied().unwrap_or(0);
            json!({
                "t": start + i as u64 * 3600,
                "auto_done": g("auto_done"), "request_done": g("request_done"), "failed": g("failed"),
                "not_found": g("not_found"), "preempted": g("preempted"),
                "page_views": g("page_views"), "video_views": g("video_views"), "embeds": g("embeds"),
                "src_browser": g("src_browser"), "src_discord": g("src_discord"), "src_bots": g("src_bots"),
            })
        })
        .collect();

    // Render time of stored videos, in 5-second bins up to 60 s.
    let mut hist = vec![0u64; 13];
    for g in &games {
        if g.render_secs > 0.0 {
            hist[((g.render_secs / 5.0) as usize).min(12)] += 1;
        }
    }

    // Storage by how long ago each game was last viewed.
    let ages = [("< 1 hour", 3600), ("1–24 hours", 86400), ("1–7 days", 7 * 86400), ("> 7 days", u64::MAX)];
    let mut by_age: Vec<(u64, u64)> = vec![(0, 0); ages.len()];
    for g in &games {
        let age = now.saturating_sub(g.last_used);
        let i = ages.iter().position(|a| age < a.1).unwrap_or(ages.len() - 1);
        by_age[i].0 += 1;
        by_age[i].1 += g.bytes;
    }

    let mut maps: HashMap<&str, (u64, u64)> = HashMap::new();
    let mut modes: HashMap<&str, u64> = HashMap::new();
    for g in &games {
        let m = maps.entry(g.map.as_str()).or_default();
        m.0 += 1;
        m.1 += views_by_game.get(&g.id).copied().unwrap_or(0);
        *modes.entry(g.mode.as_str()).or_default() += 1;
    }
    let mut maps: Vec<_> = maps.into_iter().collect();
    maps.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(b.0)));
    let mut modes: Vec<_> = modes.into_iter().collect();
    modes.sort_by(|a, b| b.1.cmp(&a.1));

    let mut by_views: Vec<&StoredVideoRecord> = games.iter().collect();
    by_views.sort_by(|a, b| {
        let v = |g: &StoredVideoRecord| views_by_game.get(&g.id).copied().unwrap_or(0);
        v(b).cmp(&v(a)).then(b.last_used.cmp(&a.last_used))
    });
    let game_json = |g: &StoredVideoRecord| {
        json!({
            "id": g.id, "map": g.map, "mode": g.mode, "players": g.players, "bytes": g.bytes,
            "render_secs": g.render_secs, "rendered": g.rendered, "last_used": g.last_used,
            "views": views_by_game.get(&g.id).copied().unwrap_or(0),
        })
    };
    let mut failures: Vec<_> = failures.into_iter().collect();
    failures.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    recent.reverse();

    let auto = app.auto_status.lock().unwrap();
    let queue_value = {
        let recent: Vec<Value> = events
            .iter()
            .rev()
            .filter_map(|e| match e {
                Event::Render { t, id, auto, outcome, .. } if outcome != "preempted" => {
                    Some(json!({"id": id, "t": t, "auto": auto, "outcome": outcome}))
                }
                _ => None,
            })
            .take(12)
            .collect();
        json!({
            "waiting": auto.queue.iter().map(|(id, ended)| json!({"id": id, "ended": ended})).collect::<Vec<_>>(),
            "recent": recent,
        })
    };
    let avg = if render_secs24.is_empty() { 0.0 } else { render_secs24.iter().sum::<f64>() / render_secs24.len() as f64 };
    json!({
        "now": now,
        "started": app.started,
        "config": {
            "slots": app.slots.capacity(),
            "storage_limit": crate::cleanup::limit_bytes(),
            "auto": auto.enabled,
            "min_players": auto.min_players,
            "max_age_mins": auto.max_age_mins,
        },
        "clients": (["browser", "discord", "twitter", "slack", "telegram", "whatsapp", "facebook", "other_bot"]
            .iter()
            .map(|c| {
                let e = by_client.get(*c);
                json!({"client": c, "requests": e.map_or(0, |e| e.0), "visitors": e.map_or(0, |e| e.1.len())})
            })
            .collect::<Vec<_>>()),
        "pages": (["home", "page", "pending", "not_found", "failed", "video"]
            .iter()
            .map(|w| {
                let e = by_what.get(*w).copied().unwrap_or((0, 0));
                json!({"what": w, "people": e.0, "bots": e.1})
            })
            .collect::<Vec<_>>()),
        "queue": queue_value,
        "live": {
            "running": app.slots.running().into_iter().map(|(id, a, s)| json!({"id": id, "auto": a, "secs": s})).collect::<Vec<_>>(),
            "queue": auto.queue_len,
            "last_poll": auto.last_poll,
            "last_new": auto.last_new,
        },
        "kpi": {
            "games": games.len(), "bytes": used,
            "done24": done24, "failed24": failed24, "not_found24": nf24, "preempted24": pre24,
            "avg_render24": avg, "views24": views24, "embeds24": embeds24, "visitors24": uniques.len(),
            "cleaned_games24": cleaned_games, "cleaned_bytes24": cleaned_bytes,
        },
        "hourly": hourly,
        "render_hist": hist,
        "by_age": ages.iter().zip(&by_age).map(|(a, b)| json!({"label": a.0, "games": b.0, "bytes": b.1})).collect::<Vec<_>>(),
        "maps": maps.iter().take(12).map(|(m, (n, v))| json!({"map": m, "games": n, "views": v})).collect::<Vec<_>>(),
        "modes": modes.iter().map(|(m, n)| json!({"mode": m, "games": n})).collect::<Vec<_>>(),
        "top": by_views.iter().take(15).map(|g| game_json(g)).collect::<Vec<_>>(),
        "recent": recent.iter().take(40).map(|e| serde_json::to_value(e).unwrap()).collect::<Vec<_>>(),
        "failures": failures.iter().take(15).map(|(m, (n, t, id))| json!({"msg": m, "count": n, "last": t, "id": id})).collect::<Vec<_>>(),
    })
}
