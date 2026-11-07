//! Renders public games as soon as they end, before anyone asks.
//!
//! Every AUTO_POLL_SECS the poller lists public games that started within the
//! last AUTO_LOOKBACK_MINS (the API filters by start time, and a game can run
//! for about three hours) and queues the ones that have ended and haven't been
//! seen. About ten public games end every minute, more than one machine can
//! render, so the queue keeps only games worth rendering (unranked, not Humans
//! vs Nations, at least AUTO_MIN_PLAYERS players), takes the newest first and
//! drops games that ended more than AUTO_MAX_AGE_MINS ago. The worker renders
//! up to one game per render slot, gives its slot to any request that needs one,
//! and uses the same render path, so a game is never rendered twice.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{env_or, valid_id, App, Status};

const LIST_URL: &str = "https://api.openfront.io/public/games";
/// Most games the listing returns per request.
const PAGE: usize = 1000;

pub struct Settings {
    poll: Duration,
    lookback: chrono::Duration,
    max_age: chrono::Duration,
    min_players: u64,
}

impl Settings {
    pub fn from_env() -> Settings {
        let num = |k: &str, d: &str| env_or(k, d).parse::<i64>().unwrap_or_else(|_| panic!("{k} must be a number"));
        Settings {
            poll: Duration::from_secs(num("AUTO_POLL_SECS", "60").max(10) as u64),
            lookback: chrono::Duration::minutes(num("AUTO_LOOKBACK_MINS", "180")),
            max_age: chrono::Duration::minutes(num("AUTO_MAX_AGE_MINS", "30")),
            min_players: num("AUTO_MIN_PLAYERS", "10").max(0) as u64,
        }
    }
}

/// A finished game waiting to be rendered.
struct Pending {
    id: String,
    ended: DateTime<Utc>,
}

#[derive(Default)]
struct Queue {
    pending: Vec<Pending>,
    /// Games already queued or skipped, with when they started (to forget them later).
    seen: HashMap<String, DateTime<Utc>>,
}

pub fn start(app: Arc<App>, settings: Settings) {
    println!(
        "auto render: every {} s, games with {}+ players that ended in the last {} min",
        settings.poll.as_secs(),
        settings.min_players,
        settings.max_age.num_minutes()
    );
    {
        let mut st = app.auto_status.lock().unwrap();
        st.enabled = true;
        st.min_players = settings.min_players;
        st.max_age_mins = settings.max_age.num_minutes();
    }
    let queue = Arc::new(Mutex::new(Queue::default()));
    let settings = Arc::new(settings);
    tokio::spawn(poll_loop(app.clone(), queue.clone(), settings.clone()));
    tokio::spawn(work_loop(app, queue, settings));
}

fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v.as_str()?).ok().map(|t| t.with_timezone(&Utc))
}

/// Whether the renderer can do this game and it's worth doing.
fn wanted(g: &Value, min_players: u64) -> bool {
    g["rankedType"].as_str().unwrap_or("unranked") == "unranked"
        && g["numPlayers"].as_u64().unwrap_or(0) >= min_players
}

/// Every public game that started in [from, to], over as many pages as it takes.
fn list_games(from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Value>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).build().into();
    let fmt = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut all = Vec::new();
    for page in 0.. {
        let mut res = agent
            .get(LIST_URL)
            .query("start", fmt(from))
            .query("end", fmt(to))
            .query("type", "Public")
            .query("limit", PAGE.to_string())
            .query("offset", (page * PAGE).to_string())
            .header("Accept", "application/json")
            .call()
            .map_err(|e| e.to_string())?;
        let body = res.body_mut().with_config().limit(64 << 20).read_to_vec().map_err(|e| e.to_string())?;
        let games: Vec<Value> = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        let n = games.len();
        all.extend(games);
        if n < PAGE {
            break;
        }
    }
    Ok(all)
}

async fn poll_loop(app: Arc<App>, queue: Arc<Mutex<Queue>>, s: Arc<Settings>) {
    let mut interval = tokio::time::interval(s.poll);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let now = Utc::now();
        let from = now - s.lookback;
        let games = match tokio::task::spawn_blocking(move || list_games(from, now)).await {
            Ok(Ok(g)) => g,
            Ok(Err(e)) => {
                println!("auto render: listing games failed: {e}");
                continue;
            }
            Err(_) => continue,
        };
        let mut added = 0;
        {
            let mut q = queue.lock().unwrap();
            // Games that started before the window can't show up again.
            q.seen.retain(|_, started| *started >= from);
            for g in &games {
                let Some(id) = g["game"].as_str().filter(|id| valid_id(id)) else { continue };
                // Listed games have ended; one without an end time isn't done yet.
                let Some(ended) = parse_time(&g["end"]) else { continue };
                if q.seen.contains_key(id) {
                    continue;
                }
                q.seen.insert(id.to_string(), parse_time(&g["start"]).unwrap_or(now));
                if !wanted(g, s.min_players) || now - ended > s.max_age || app.catalog.has_video(id) {
                    continue;
                }
                q.pending.push(Pending { id: id.to_string(), ended });
                added += 1;
            }
            // Newest first; anything too old to be worth it goes.
            q.pending.retain(|p| now - p.ended <= s.max_age);
            q.pending.sort_by(|a, b| b.ended.cmp(&a.ended));
            {
                let mut st = app.auto_status.lock().unwrap();
                st.queue_len = q.pending.len();
                st.queue = q.pending.iter().take(400).map(|p| (p.id.clone(), p.ended.timestamp() as u64)).collect();
                st.last_poll = Some(now.timestamp() as u64);
                st.last_new = added;
            }
            if added > 0 {
                println!("auto render: {added} new finished games queued, {} waiting", q.pending.len());
            }
        }
    }
}

async fn work_loop(app: Arc<App>, queue: Arc<Mutex<Queue>>, s: Arc<Settings>) {
    // Auto renders in flight: up to one per slot, all yielding to requests.
    let in_flight = Arc::new(AtomicUsize::new(0));
    loop {
        if in_flight.load(Ordering::SeqCst) >= app.slots.capacity() || !app.slots.auto_can_start() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        let next = {
            let mut q = queue.lock().unwrap();
            let now = Utc::now();
            q.pending.retain(|p| now - p.ended <= s.max_age);
            let job = if q.pending.is_empty() { None } else { Some(q.pending.remove(0)) };
            {
                let mut st = app.auto_status.lock().unwrap();
                st.queue_len = q.pending.len();
                st.queue = q.pending.iter().take(400).map(|p| (p.id.clone(), p.ended.timestamp() as u64)).collect();
            }
            job
        };
        let Some(job) = next else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        in_flight.fetch_add(1, Ordering::SeqCst);
        let (app, queue, in_flight) = (app.clone(), queue.clone(), in_flight.clone());
        tokio::spawn(async move {
            // Waits for the render (or joins one a request already started).
            // Still pending afterwards means a request took its slot: queue it again.
            if let Status::Pending = app.status_for(&job.id, Duration::from_secs(30 * 60), true).await {
                if !app.catalog.has_video(&job.id) {
                    let mut q = queue.lock().unwrap();
                    q.pending.push(job);
                    q.pending.sort_by(|a, b| b.ended.cmp(&a.ended));
                }
            }
            in_flight.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
