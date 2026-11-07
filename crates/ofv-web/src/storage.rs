//! Embedded `redb` ACID catalog (`DATA_DIR/catalog.redb`).
//!
//! Replaces full-directory `read_dir` + `stat()` scans and `events.jsonl`
//! rewrites with three ACID tables:
//! - `VIDEOS_TABLE`: `<game_id>` -> serialized [`StoredVideoRecord`] JSON
//! - `EVENTS_TABLE`: `(timestamp_secs << 20) | seq` -> serialized [`crate::stats::Event`] JSON
//! - `COMMITS_TABLE`: `<engine_commit>` -> `bool` (`true` if 0 engine diffs vs Rust build)

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::stats::Event;

const VIDEOS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("videos");
const EVENTS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("events");
const COMMITS_TABLE: TableDefinition<&str, bool> = TableDefinition::new("commits");

/// Retention window for events (30 days in seconds).
const EVENT_KEEP_SECS: u64 = 30 * 24 * 3600;

/// Metadata and storage accounting for a rendered game video.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StoredVideoRecord {
    pub id: String,
    pub map: String,
    pub mode: String,
    pub players: u64,
    pub bytes: u64,
    pub render_secs: f64,
    pub rendered: u64,
    pub last_used: u64,
    pub meta: Value,
}

/// Embedded `redb` catalog for videos, telemetry events, and engine commit compatibility.
pub struct Catalog {
    db: Database,
    seq: AtomicU64,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn mtime_secs(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

impl Catalog {
    /// Opens or creates `dir/catalog.redb` and runs a one-time bootstrap from
    /// existing `<id>.json` / `.mp4` / `.png` files and `events.jsonl` if the tables are empty.
    pub fn open(dir: &Path) -> Arc<Self> {
        let db_path = dir.join("catalog.redb");
        let db = Database::create(&db_path).expect("failed to open catalog.redb");
        let catalog = Arc::new(Self { db, seq: AtomicU64::new(0) });
        catalog.init_and_bootstrap(dir);
        catalog
    }

    fn init_and_bootstrap(&self, dir: &Path) {
        let Ok(write_txn) = self.db.begin_write() else { return };
        let mut videos_empty = false;
        let mut events_empty = false;
        {
            if let Ok(mut videos) = write_txn.open_table(VIDEOS_TABLE) {
                videos_empty = videos.iter().map_or(true, |mut it| it.next().is_none());
                if videos_empty {
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for e in entries.flatten() {
                            let name = e.file_name().to_string_lossy().to_string();
                            let Some(id) = name.strip_suffix(".json") else { continue };
                            if !crate::valid_id(id) {
                                continue;
                            }
                            let mp4_path = dir.join(format!("{id}.mp4"));
                            let png_path = dir.join(format!("{id}.png"));
                            let Ok(mp4_meta) = std::fs::metadata(&mp4_path) else { continue };
                            let png_len = std::fs::metadata(&png_path).map_or(0, |m| m.len());
                            let Ok(text) = std::fs::read(e.path()) else { continue };
                            let Ok(meta) = serde_json::from_slice::<Value>(&text) else { continue };
                            let record = StoredVideoRecord {
                                id: id.to_string(),
                                map: meta["map"].as_str().unwrap_or("?").to_string(),
                                mode: meta["mode"].as_str().unwrap_or("?").to_string(),
                                players: meta["players"].as_u64().unwrap_or(0),
                                bytes: mp4_meta.len() + png_len + text.len() as u64,
                                render_secs: meta["renderSeconds"].as_f64().unwrap_or(0.0),
                                rendered: mtime_secs(&mp4_path),
                                last_used: mtime_secs(&e.path()),
                                meta,
                            };
                            if let Ok(encoded) = serde_json::to_vec(&record) {
                                let _ = videos.insert(id, encoded.as_slice());
                            }
                        }
                    }
                }
            }
            if let Ok(mut events) = write_txn.open_table(EVENTS_TABLE) {
                events_empty = events.iter().map_or(true, |mut it| it.next().is_none());
                if events_empty {
                    let jsonl_path = dir.join("events.jsonl");
                    if let Ok(content) = std::fs::read_to_string(&jsonl_path) {
                        let cutoff = now_secs().saturating_sub(EVENT_KEEP_SECS);
                        for line in content.lines() {
                            let Ok(ev) = serde_json::from_str::<Event>(line) else { continue };
                            if ev.t() < cutoff {
                                continue;
                            }
                            let seq = self.seq.fetch_add(1, Ordering::Relaxed) & 0xFFFFF;
                            let key = (ev.t() << 20) | seq;
                            if let Ok(encoded) = serde_json::to_vec(&ev) {
                                let _ = events.insert(key, encoded.as_slice());
                            }
                        }
                    }
                } else {
                    // Prune events older than retention window on startup.
                    let cutoff_key = now_secs().saturating_sub(EVENT_KEEP_SECS) << 20;
                    let expired: Vec<u64> = events
                        .range(..cutoff_key)
                        .ok()
                        .into_iter()
                        .flatten()
                        .filter_map(|e| e.ok().map(|(k, _)| k.value()))
                        .collect();
                    for k in expired {
                        let _ = events.remove(k);
                    }
                }
            }
            let _ = write_txn.open_table(COMMITS_TABLE);
        }
        let _ = write_txn.commit();
        let _ = (videos_empty, events_empty);
    }

    /// Inserts or updates a rendered video record in `VIDEOS_TABLE`.
    pub fn upsert_video(&self, id: &str, meta: &Value, bytes: u64, rendered: u64, last_used: u64) {
        let record = StoredVideoRecord {
            id: id.to_string(),
            map: meta["map"].as_str().unwrap_or("?").to_string(),
            mode: meta["mode"].as_str().unwrap_or("?").to_string(),
            players: meta["players"].as_u64().unwrap_or(0),
            bytes,
            render_secs: meta["renderSeconds"].as_f64().unwrap_or(0.0),
            rendered,
            last_used,
            meta: meta.clone(),
        };
        let Ok(encoded) = serde_json::to_vec(&record) else { return };
        let Ok(write_txn) = self.db.begin_write() else { return };
        if let Ok(mut table) = write_txn.open_table(VIDEOS_TABLE) {
            let _ = table.insert(id, encoded.as_slice());
        }
        let _ = write_txn.commit();
    }

    /// Updates `last_used` for `id` only if at least `granularity_secs` have elapsed since the previous `last_used`.
    pub fn touch_video(&self, id: &str, now_secs: u64, granularity_secs: u64) {
        let existing = {
            let Ok(read_txn) = self.db.begin_read() else { return };
            let Ok(table) = read_txn.open_table(VIDEOS_TABLE) else { return };
            let Ok(Some(guard)) = table.get(id) else { return };
            serde_json::from_slice::<StoredVideoRecord>(guard.value()).ok()
        };
        let Some(mut record) = existing else { return };
        if now_secs.saturating_sub(record.last_used) < granularity_secs {
            return;
        }
        record.last_used = now_secs;
        let Ok(encoded) = serde_json::to_vec(&record) else { return };
        let Ok(write_txn) = self.db.begin_write() else { return };
        if let Ok(mut table) = write_txn.open_table(VIDEOS_TABLE) {
            let _ = table.insert(id, encoded.as_slice());
        }
        let _ = write_txn.commit();
    }

    /// Removes `id` from `VIDEOS_TABLE`.
    pub fn remove_video(&self, id: &str) {
        let Ok(write_txn) = self.db.begin_write() else { return };
        if let Ok(mut table) = write_txn.open_table(VIDEOS_TABLE) {
            let _ = table.remove(id);
        }
        let _ = write_txn.commit();
    }

    /// Returns `true` if `id` is recorded in `VIDEOS_TABLE`.
    pub fn has_video(&self, id: &str) -> bool {
        let Ok(read_txn) = self.db.begin_read() else { return false };
        let Ok(table) = read_txn.open_table(VIDEOS_TABLE) else { return false };
        table.get(id).ok().flatten().is_some()
    }

    /// Returns the stored JSON metadata for `id`, if present.
    pub fn get_video_meta(&self, id: &str) -> Option<Value> {
        let read_txn = self.db.begin_read().ok()?;
        let table = read_txn.open_table(VIDEOS_TABLE).ok()?;
        let guard = table.get(id).ok()??;
        let record = serde_json::from_slice::<StoredVideoRecord>(guard.value()).ok()?;
        Some(record.meta)
    }

    /// Lists all stored video records in `VIDEOS_TABLE` without filesystem syscalls.
    pub fn list_videos(&self) -> Vec<StoredVideoRecord> {
        let mut out = Vec::new();
        let Ok(read_txn) = self.db.begin_read() else { return out };
        let Ok(table) = read_txn.open_table(VIDEOS_TABLE) else { return out };
        let Ok(iter) = table.iter() else { return out };
        for entry in iter.flatten() {
            if let Ok(record) = serde_json::from_slice::<StoredVideoRecord>(entry.1.value()) {
                out.push(record);
            }
        }
        out
    }

    /// Appends a telemetry event to `EVENTS_TABLE`.
    pub fn append_event(&self, event: &Event) {
        let Ok(encoded) = serde_json::to_vec(event) else { return };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) & 0xFFFFF;
        let key = (event.t() << 20) | seq;
        let Ok(write_txn) = self.db.begin_write() else { return };
        if let Ok(mut table) = write_txn.open_table(EVENTS_TABLE) {
            let _ = table.insert(key, encoded.as_slice());
            if seq & 0xFF == 0 {
                let cutoff_key = now_secs().saturating_sub(EVENT_KEEP_SECS) << 20;
                let expired: Vec<u64> = table
                    .range(..cutoff_key)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok().map(|(k, _)| k.value()))
                    .collect();
                for k in expired {
                    let _ = table.remove(k);
                }
            }
        }
        let _ = write_txn.commit();
    }

    /// Lists all events in `EVENTS_TABLE` with timestamp `>= cutoff_secs`.
    pub fn list_events(&self, cutoff_secs: u64) -> Vec<Event> {
        let mut out = Vec::new();
        let Ok(read_txn) = self.db.begin_read() else { return out };
        let Ok(table) = read_txn.open_table(EVENTS_TABLE) else { return out };
        let Ok(iter) = table.range((cutoff_secs << 20)..) else { return out };
        for entry in iter.flatten() {
            if let Ok(ev) = serde_json::from_slice::<Event>(entry.1.value()) {
                out.push(ev);
            }
        }
        out
    }

    /// Looks up cached upstream commit compatibility in `COMMITS_TABLE`.
    pub fn get_commit_compat(&self, commit: &str) -> Option<bool> {
        let read_txn = self.db.begin_read().ok()?;
        let table = read_txn.open_table(COMMITS_TABLE).ok()?;
        let guard = table.get(commit).ok()??;
        Some(guard.value())
    }

    /// Caches upstream commit compatibility in `COMMITS_TABLE`.
    pub fn set_commit_compat(&self, commit: &str, compatible: bool) {
        let Ok(write_txn) = self.db.begin_write() else { return };
        if let Ok(mut table) = write_txn.open_table(COMMITS_TABLE) {
            let _ = table.insert(commit, compatible);
        }
        let _ = write_txn.commit();
    }
}
