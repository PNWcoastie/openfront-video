//! Keeps the stored videos under MAX_STORAGE_GB (0: no limit). Each game is
//! its <id>.mp4, .png and .json; the .json's modified time is when it was last
//! viewed (see `touch`), or rendered. Over the limit, the least recently viewed
//! games go until usage is back under 90% of it, so a new render doesn't
//! trigger a deletion every time. A deleted game is simply rendered again
//! when someone asks for it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::{valid_id, App};

/// How often usage is checked (also right after each render).
const CHECK_EVERY: Duration = Duration::from_secs(300);
/// A view newer than this doesn't move the timestamp again (saves disk writes).
const TOUCH_GRANULARITY: Duration = Duration::from_secs(3600);

pub fn limit_bytes() -> u64 {
    let gb: f64 = crate::env_or("MAX_STORAGE_GB", "35").parse().expect("MAX_STORAGE_GB must be a number");
    (gb.max(0.0) * 1e9) as u64
}

/// Marks a game as just viewed.
pub fn touch(meta: &Path) {
    let Ok(file) = std::fs::File::options().append(true).open(meta) else { return };
    let stale = file
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_none_or(|age| age >= TOUCH_GRANULARITY);
    if stale {
        let _ = file.set_modified(SystemTime::now());
    }
}

struct Stored {
    id: String,
    bytes: u64,
    last_used: SystemTime,
}

/// Finished games on disk (a game counts once its .json exists).
fn scan(dir: &Path) -> Vec<Stored> {
    let mut games: HashMap<String, (u64, Option<SystemTime>)> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some((id, ext)) = name.rsplit_once('.') else { continue };
        if !valid_id(id) || !matches!(ext, "mp4" | "png" | "json") {
            continue; // temporary files of renders in progress, and anything else
        }
        let Ok(meta) = e.metadata() else { continue };
        let g = games.entry(id.to_string()).or_default();
        g.0 += meta.len();
        if ext == "json" {
            g.1 = meta.modified().ok();
        }
    }
    games
        .into_iter()
        .filter_map(|(id, (bytes, used))| Some(Stored { id, bytes, last_used: used? }))
        .collect()
}

/// Deletes the least recently viewed games while over the limit.
pub fn run_once(dir: &Path, limit: u64) -> (u64, u64) {
    if limit == 0 {
        return (0, 0);
    }
    let mut games = scan(dir);
    let mut total: u64 = games.iter().map(|g| g.bytes).sum();
    if total <= limit {
        return (0, 0);
    }
    let target = limit / 10 * 9;
    games.sort_by_key(|g| g.last_used);
    let (mut removed, mut freed) = (0, 0u64);
    for g in games {
        if total <= target {
            break;
        }
        // The .json goes first, so the game stops counting as ready at once.
        for ext in ["json", "mp4", "png"] {
            let _ = std::fs::remove_file(dir.join(format!("{}.{ext}", g.id)));
        }
        total -= g.bytes;
        freed += g.bytes;
        removed += 1;
    }
    println!(
        "cleanup: deleted {removed} least recently viewed games ({:.2} GB), {:.2} of {:.2} GB used",
        freed as f64 / 1e9,
        total as f64 / 1e9,
        limit as f64 / 1e9
    );
    (removed, freed)
}

pub fn start(app: Arc<App>) {
    let limit = limit_bytes();
    if limit == 0 {
        println!("cleanup: no storage limit");
        return;
    }
    println!("cleanup: keeping videos under {:.2} GB", limit as f64 / 1e9);
    let dir: PathBuf = app.dir.clone();
    tokio::spawn(async move {
        loop {
            let d = dir.clone();
            if let Ok((removed, bytes)) = tokio::task::spawn_blocking(move || run_once(&d, limit)).await {
                if removed > 0 {
                    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                    app.events.record(crate::stats::Event::Cleanup { t, removed, bytes });
                }
            }
            // Woken early after each finished render.
            let _ = tokio::time::timeout(CHECK_EVERY, app.rendered.notified()).await;
        }
    });
}
