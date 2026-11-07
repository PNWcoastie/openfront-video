# `openfront-video` — Deployment, Storage & Operations Runbook

> **Back to Overview:** [README.md](README.md) | **User & CLI Guide:** [USER_GUIDE.md](USER_GUIDE.md) | **Architecture Blueprint:** [BLUEPRINT.md](BLUEPRINT.md)

This runbook covers building `openfront-video` from source or Docker, configuring environment variables for `openfront-web`, managing the embedded `redb` catalog (`DATA_DIR/catalog.redb`), and operating the multi-engine commit router (`ENGINES_DIR`).

---

## 1. Environment Variables (`openfront-web`)

| Variable | Default | Purpose |
| :--- | :---: | :--- |
| `PORT` | `8080` | TCP port `openfront-web` binds on (`0.0.0.0:$PORT`). |
| `DATA_DIR` | `./data` | Directory storing rendered `<id>.mp4`, `<id>.png`, `<id>.json`, temporary in-flight renders (`tmp/`), and the embedded `catalog.redb` database. |
| `PUBLIC_URL` | `https://openfront.video` | Canonical base URL written into Open Graph (`og:url`, `og:video`, `og:image`) and Twitter player tags. |
| `OPENFRONT_VIDEO_BIN` | `openfront-video` | Path to the `openfront-video` renderer executable. |
| `MAX_RENDERS` | `2` | Maximum concurrent `openfront-video` child processes. Direct user/Discord requests automatically preempt background `auto` renders. |
| `EMBED_WAIT_SECS` | `25` | Legacy fallback timeout; link previewers (`Discordbot/2.0`) now receive `og:video` HTML in **`~17 ms`** as soon as `--preview-meta` is written. |
| `VIDEO_WAIT_SECS` | `120` | How long `GET /video/<id>.mp4` holds the connection open waiting for an active render to finish before returning `503 Retry-After: 10`. |
| `MAX_DISK_GB` | `35` | Disk quota for `DATA_DIR`. When exceeded, `cleanup` evicts the least-recently-viewed videos using the `redb` catalog. |
| `AUTO_RENDER` | `1` | Set to `1` to poll OpenFront.io's public lobby feed and pre-render finished matches in the background; set to `0` for on-demand rendering only. |
| `AUTO_INTERVAL_SECS` | `60` | Polling interval for `AUTO_RENDER`. |
| `AUTO_MIN_PLAYERS` | `10` | Minimum human players required for `AUTO_RENDER` to queue a public game. |
| `RENDER_API_TOKEN` | *unset* | Optional Bearer token enabling `POST /api/v1/render` for external bots. |
| `ADMIN_USER` / `ADMIN_PASSWORD` | *unset* | Optional HTTP Basic Auth credentials enabling `/admin` and `/admin/api/stats` (`ADMIN_TOKEN` alone defaults `ADMIN_USER` to `admin`). |
| `ENGINES_DIR` | `<DATA_DIR>/engines` | Optional directory of versioned `openfront-video-<commit>` binaries and `OpenFrontIO/` git checkouts for replaying older/newer engine versions. |

---

## 2. Production Deployment with Docker Compose & Automatic HTTPS

`docker-compose.yml` runs `openfront-web` behind **Caddy**, which automatically provisions and renews Let's Encrypt TLS certificates (including HTTP/3 on `443/udp`):

```bash
cp .env.example .env
# Edit .env and set DOMAIN=openfront.video and ACME_EMAIL=you@example.com
docker compose up -d --build
```

- **Zero External Build Dependencies:** The multi-stage `Dockerfile` clones `OpenFrontIO` at the pinned engine commit (`9f4a28f`), compresses the map packs (`zstd` level 19), and compiles both `openfront-video` and `openfront-web` into a minimal Debian runtime image.
- **Persistent Volumes:** Rendered videos and `catalog.redb` persist in the `videos` volume (`/data`); TLS certificates persist in `caddy_data`.

---

## 3. Embedded `redb` Catalog & Startup Bootstrap (`crates/ofv-web/src/storage.rs`)

Instead of scanning thousands of files with `read_dir()` and `stat()` on every `/admin` page load or LRU cleanup pass, `openfront-web` maintains an embedded, zero-configuration ACID database at `<DATA_DIR>/catalog.redb` with four tables:
1. `VIDEOS_TABLE`: Indexed by game ID, storing `StoredVideoRecord` (`map`, `mode`, `players`, `bytes`, `render_secs`, `rendered`, `last_used`, and full `meta` JSON including `winner`).
2. `EVENTS_TABLE`: Append-only telemetry log for views (`page`, `video`, `home`, `pending`) and render completions.
3. `META_TABLE`: Tracks `bootstrapped` state so an existing `DATA_DIR` from an older deployment is automatically scanned and imported once on first boot.
4. `COMMITS_TABLE`: Caches `git diff` engine-compatibility checks across restarts.

---

## 4. Multi-Engine Commit Routing (`crates/ofv-web/src/engines.rs`)

OpenFront.io updates its client and server frequently, but the core simulation (`packages/engine/`, `packages/engine-lib/`, `src/core/`) changes much less often:
1. When a replay's `gitCommit` differs from the compiled-in `OPENFRONT_ENGINE_COMMIT` (`9f4a28f`), `Engines::is_engine_compatible` checks `git diff --quiet <rust_commit> <replay_commit> -- packages/engine/ packages/engine-lib/ src/core/` against `<ENGINES_DIR>/OpenFrontIO` and caches the boolean result in `catalog.redb`.
2. If the simulation directories did **not** change (e.g. a UI, CSS, or translation commit), `openfront-web` immediately routes the replay to the fast primary Rust binary!
3. If the simulation did change and a matching `openfront-video-<commit8>` binary exists in `ENGINES_DIR`, it dispatches that binary; otherwise it falls back to building or running the matching commit from `<ENGINES_DIR>/OpenFrontIO`.
