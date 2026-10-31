//! OpenFrontIO checkouts at the engine versions games were played on, for
//! games the Rust renderer doesn't follow. Each lives in
//! ENGINES_DIR/<commit> (a sparse, shallow checkout plus `npm ci
//! --omit=dev`), made the first time a game needs it, and render.ts renders
//! with it. Only the ENGINES_KEEP most recently used are kept.
//!
//! Settings (environment): RENDER_TS (path to render.ts; unset turns the
//! fallback off), ENGINES_DIR (DATA_DIR/engines), ENGINES_KEEP (3),
//! OPENFRONT_REPO (https://github.com/openfrontio/OpenFrontIO.git).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::process::Command;

/// What a checkout needs to load the engine and the client's colors, in both
/// the current packages/ layout and the older src/core one.
const SPARSE: &[&str] =
    &["/packages/", "/src/", "/resources/", "/proprietary/", "/tsconfig.json", "/tsconfig.base.json", "/package.json", "/package-lock.json"];

/// Marks a finished checkout; its modified time is when it was last used.
const READY: &str = ".ready";

pub struct Engines {
    pub render_ts: PathBuf,
    dir: PathBuf,
    keep: usize,
    repo: String,
    /// One preparation at a time per commit.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Checkouts a render is using right now, which pruning leaves alone.
    in_use: Arc<Mutex<HashMap<String, usize>>>,
}

/// Holds a checkout in use until dropped.
pub struct Lease {
    pub dir: PathBuf,
    commit: String,
    in_use: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut m = self.in_use.lock().unwrap();
        if let Some(n) = m.get_mut(&self.commit) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.commit);
            }
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

pub fn valid_commit(c: &str) -> bool {
    c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Runs a command, giving its last stderr line on failure.
async fn run(cmd: &mut Command, what: &str, timeout: Duration) -> Result<(), String> {
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| format!("{what} took too long"))?
        .map_err(|e| format!("couldn't run {what}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let line = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    Err(format!("{what} failed: {line}"))
}

impl Engines {
    /// None when RENDER_TS isn't set: games from other versions then render
    /// with the Rust engine anyway (they may desync).
    pub fn from_env(data_dir: &Path) -> Option<Engines> {
        let render_ts = PathBuf::from(std::env::var("RENDER_TS").ok().filter(|v| !v.is_empty())?);
        let dir = std::env::var("ENGINES_DIR").ok().filter(|v| !v.is_empty()).map_or_else(|| data_dir.join("engines"), PathBuf::from);
        std::fs::create_dir_all(&dir).expect("ENGINES_DIR");
        // Checkouts cut off by a restart.
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            if !e.path().join(READY).exists() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        Some(Engines {
            render_ts,
            dir,
            keep: env_or("ENGINES_KEEP", "3").parse().expect("ENGINES_KEEP"),
            repo: env_or("OPENFRONT_REPO", "https://github.com/openfrontio/OpenFrontIO.git"),
            locks: Mutex::new(HashMap::new()),
            in_use: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// The checkout for `commit`, made first if needed.
    pub async fn get(&self, commit: &str) -> Result<Lease, String> {
        if !valid_commit(commit) {
            return Err(format!("\"{commit}\" isn't an engine commit"));
        }
        let lock = self.locks.lock().unwrap().entry(commit.to_string()).or_default().clone();
        let _guard = lock.lock().await;
        let dir = self.dir.join(commit);
        *self.in_use.lock().unwrap().entry(commit.to_string()).or_insert(0) += 1;
        let lease = Lease { dir: dir.clone(), commit: commit.to_string(), in_use: self.in_use.clone() };
        let ready = dir.join(READY);
        if ready.exists() {
            let _ = std::fs::File::options().write(true).open(&ready).and_then(|f| f.set_modified(std::time::SystemTime::now()));
            return Ok(lease);
        }
        println!("engine {}: checking out", &commit[..9]);
        let t0 = std::time::Instant::now();
        // Built in place, not moved there after: npm's workspace links can
        // point at the folder they were made in. READY marks it complete.
        let _ = tokio::fs::remove_dir_all(&dir).await;
        if let Err(e) = self.prepare(commit, &dir).await {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(e);
        }
        tokio::fs::write(&ready, b"").await.map_err(|e| format!("couldn't store the engine: {e}"))?;
        println!("engine {}: ready in {:.0} s", &commit[..9], t0.elapsed().as_secs_f64());
        self.prune();
        Ok(lease)
    }

    async fn prepare(&self, commit: &str, tmp: &Path) -> Result<(), String> {
        let minutes = |m: u64| Duration::from_secs(m * 60);
        tokio::fs::create_dir_all(tmp).await.map_err(|e| e.to_string())?;
        let git = |args: &[&str]| {
            let mut c = Command::new("git");
            c.arg("-C").arg(tmp).args(args);
            c
        };
        run(&mut git(&["init", "-q"]), "git init", minutes(1)).await?;
        run(&mut git(&["remote", "add", "origin", &self.repo]), "git remote", minutes(1)).await?;
        let mut sparse = vec!["sparse-checkout", "set", "--no-cone"];
        sparse.extend_from_slice(SPARSE);
        run(&mut git(&sparse), "git sparse-checkout", minutes(1)).await?;
        run(&mut git(&["fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", commit]), "fetching the engine", minutes(10)).await?;
        run(&mut git(&["checkout", "-q", "FETCH_HEAD"]), "checking out the engine", minutes(10)).await?;
        // render.ts reads the engine's commit from git; the rest is npm's.
        let mut npm = Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" });
        npm.current_dir(tmp).args(["ci", "--omit=dev", "--ignore-scripts", "--no-audit", "--no-fund"]);
        run(&mut npm, "installing the engine's packages", minutes(15)).await
    }

    /// Deletes all but the `keep` most recently used checkouts not in use.
    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        let mut ready: Vec<(std::time::SystemTime, String)> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let used = std::fs::metadata(e.path().join(READY)).and_then(|m| m.modified()).ok()?;
                valid_commit(&name).then_some((used, name))
            })
            .collect();
        ready.sort_by(|a, b| b.0.cmp(&a.0));
        let busy = self.in_use.lock().unwrap().clone();
        for (_, name) in ready.into_iter().skip(self.keep) {
            if busy.contains_key(&name) {
                continue;
            }
            println!("engine {}: removed (least recently used)", &name[..9]);
            let _ = std::fs::remove_dir_all(self.dir.join(&name));
        }
    }
}
