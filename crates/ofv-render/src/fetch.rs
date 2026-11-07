//! Loading a game record: a saved file (JSON, optionally gzipped) or a game
//! from the public API by link or ID.

use std::io::Read;

use serde_json::Value;

/// The game ID in a link (`.../game/<id>`) or a bare ID.
pub fn game_id_from(input: &str) -> Option<String> {
    let valid = |s: &str| (4..=32).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-');
    if let Some(i) = input.find("/game/") {
        let id: String = input[i + 6..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    valid(input).then(|| input.to_string())
}

pub fn load_record(input: &str) -> Result<Value, String> {
    let resolved = if std::path::Path::new(input).is_file() {
        Some(std::path::PathBuf::from(input))
    } else {
        input
            .strip_prefix("replays/")
            .map(|s| std::path::Path::new("..").join(s))
            .filter(|p| p.is_file())
    };
    if let Some(path) = resolved {
        let mut buf = std::fs::read(&path).map_err(|e| format!("reading {input}: {e}"))?;
        if buf.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&buf[..]).read_to_end(&mut out).map_err(|e| format!("unpacking {input}: {e}"))?;
            buf = out;
        }
        return serde_json::from_slice(&buf).map_err(|e| format!("{input} isn't a game record: {e}"));
    }
    let id = game_id_from(input).ok_or_else(|| format!("can't find a game ID in \"{input}\""))?;
    // The documented public API (docs/API.md in OpenFrontIO). The game client's
    // own /game/:id endpoint sits behind a Cloudflare bot check.
    let url = format!("https://api.openfront.io/public/game/{id}");
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let mut res = agent.get(&url).header("Accept", "application/json").call().map_err(|e| format!("fetching {url} failed: {e}"))?;
    let status = res.status().as_u16();
    let is_json = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    if status == 404 {
        return Err(format!("game {id} not found ({url})"));
    }
    if !is_json && (status == 403 || status == 503) {
        return Err(format!(
            "{url} answered with a Cloudflare bot check (HTTP {status}).\n  Open https://api.openfront.io/game/{id} in your browser, save the page as {id}.json and pass the file instead."
        ));
    }
    if !(200..300).contains(&status) {
        return Err(format!("fetching {url} failed: HTTP {status}"));
    }
    if !is_json {
        return Err(format!("{url} didn't return JSON"));
    }
    let body = res
        .body_mut()
        .with_config()
        .limit(1 << 30)
        .read_to_vec()
        .map_err(|e| format!("reading {url}: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("{url} didn't return a game record: {e}"))
}
