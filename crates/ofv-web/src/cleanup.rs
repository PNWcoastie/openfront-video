//! Keeps the stored videos under MAX_STORAGE_GB (0: no limit). Each game is
//! its <id>.mp4, .png and .json, indexed in `catalog.redb`. Over the limit,
//! the least recently viewed games go until usage is back under 90% of it,
//! so a new render doesn't trigger a deletion every time. A deleted game is
//! simply rendered again when someone asks for it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::storage::Catalog;
use crate::App;

/// How often usage is checked (also right after each render).
const CHECK_EVERY: Duration = Duration::from_secs(300);
/// A view newer than this doesn't move the timestamp again (saves disk writes).
const TOUCH_GRANULARITY: Duration = Duration::from_secs(3600);

pub fn limit_bytes() -> u64 {
    let gb: f64 = crate::env_or("MAX_STORAGE_GB", "35").parse().expect("MAX_STORAGE_GB must be a number");
    (gb.max(0.0) * 1e9) as u64
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Marks a game as just viewed in the `redb` catalog (with 1-hour granularity).
pub fn touch(catalog: &Catalog, id: &str) {
    catalog.touch_video(id, now_secs(), TOUCH_GRANULARITY.as_secs());
}

/// Deletes the least recently viewed games while over the limit using the `redb` catalog.
pub fn run_once(dir: &Path, catalog: &Catalog, limit: u64) -> (u64, u64) {
    if limit == 0 {
        return (0, 0);
    }
    let mut games = catalog.list_videos();
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
        // Remove from catalog and delete files (.json first so it stops counting as ready at once).
        catalog.remove_video(&g.id);
        for ext in ["json", "mp4", "png"] {
            let _ = std::fs::remove_file(dir.join(format!("{}.{ext}", g.id)));
        }
        total = total.saturating_sub(g.bytes);
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
            let cat = app.catalog.clone();
            if let Ok((removed, bytes)) = tokio::task::spawn_blocking(move || run_once(&d, &cat, limit)).await {
                if removed > 0 {
                    let t = now_secs();
                    app.events.record(crate::stats::Event::Cleanup { t, removed, bytes });
                }
            }
            // Woken early after each finished render.
            let _ = tokio::time::timeout(CHECK_EVERY, app.rendered.notified()).await;
        }
    });
}
