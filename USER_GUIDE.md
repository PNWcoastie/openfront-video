# `openfront-video` — User, Player & CLI Guide

> **Back to Overview:** [README.md](README.md) | **Architecture Blueprint:** [BLUEPRINT.md](BLUEPRINT.md) | **Operations & Deployment:** [OPERATIONS.md](OPERATIONS.md)

This guide covers how to watch and share OpenFront.io timelapses using the **Web UI**, how to use the **Tactical Playback Control Bar** to slow down or step frame-by-frame through battles, how to render custom videos from the **Command Line (`openfront-video`)**, and how to trigger renders programmatically from a **Discord Bot (`POST /api/v1/render`)**.

---

## 1. Sharing & Watching Replays in Discord and the Browser

### Method A: The `fxtwitter`-Style URL Swap (Fastest for Discord)
Whenever a match finishes on `openfront.io`, take the game URL and replace `openfront.io` with your `openfront.video` domain:
- **Original Match URL:** `https://openfront.io/join/cAj8yxzNsH` *(or `https://openfront.io/w0/game/cAj8yxzNsH`)*
- **Video Embed URL:** `https://openfront.video/join/cAj8yxzNsH` *(or `https://openfront.video/cAj8yxzNsH`)*

When pasted into a Discord channel:
1. Discord's `Discordbot/2.0` scraper hits `openfront-web` and receives the match's Open Graph `<meta>` tags (`og:title`, `og:description` with player count and winner, `og:video`) in **`~17 ms`** via `--preview-meta`.
2. Discord's media proxy holds a connection to `/video/cAj8yxzNsH.mp4` while the 3-thread Rust renderer finishes encoding the `.mp4` (~15–24 seconds), then displays the inline video player directly in chat.

### Method B: Pasting a Match Link or ID on the Web Home Page (`GET /`)
1. Open the web service in your browser (e.g. `http://127.0.0.1:8080/`).
2. Paste either a full URL (`https://openfront.io/join/cAj8yxzNsH`) or a bare game ID (`cAj8yxzNsH`) into the top input box and click **Watch / Render Timelapse**.
3. If the match hasn't been rendered yet, you'll see a live progress card updating every second with the current simulated tick (`Tick 5,150 / 10,311`), encoded frame (`Frame 436 / 960`), and elapsed time.
4. The instant encoding reaches `100%`, the page automatically transitions into the video player.

---

## 2. Using the Tactical Playback Control Bar

Because OpenFront.io matches (15–45 minutes) are compressed into a **32-second timelapse** (`900` action frames + `60` hold frames at `30 fps`), battles and nuclear exchanges happen quickly at `1x` speed.

Below every video on the match page (`/game/<id>` or `/<id>`), the **Tactical Playback Control Bar** gives you full control over playback speed and frame-by-frame inspection:

| Control / Button | Keyboard Shortcut | Action |
| :--- | :---: | :--- |
| **`0.25x` / `0.5x` / `1x` / `1.5x` / `2x`** | `[` *(slower)* / `]` *(faster)* | Changes video playback speed immediately (`0.25x` plays at 7.5 fps—ideal for inspecting naval landings, SAM interceptions, and border wars). |
| **`⏯ Play / Pause`** | `Space` | Toggles video playback. |
| **`◀ -1f`** | `,` *(comma)* | Pauses playback and steps **backward 1 exact frame** (`1/30s`). |
| **`+1f ▶`** | `.` *(period)* | Pauses playback and steps **forward 1 exact frame** (`1/30s`). |
| **`⏮ Start`** | — | Jumps back to `00:00` (`tick 0`). |
| **`⏭ Victory`** | — | Pauses and jumps directly to the final 2-second hold frame showing the final map state and the **`VICTORY`** banner. |
| **`Copy Discord Link`** | — | Copies `{origin}/game/{id}` to your clipboard so you can paste the playable video embed directly into Discord. |

---

## 3. Reading the Broadcast HUD Overlay

By default, `openfront-video` renders three non-intrusive HUD elements styled with OpenFront's native dark-slate palette (`#111827` / `#374151`):

1. **Top-Left Match Clock (`TIME MM:SS`):**
   - Shows the elapsed in-game clock (`10 ticks = 1 second` of OpenFront gameplay).
   - Notice how the clock advances more slowly during the early expansion phase and accelerates during late-game cleanup (following the $t^{1.3}$ timelapse curve).
2. **Top-Right Top-5 Territory Leaderboard:**
   - Displays the top 5 living players sorted by land tile count descending.
   - Each row shows the player's exact territory/team color swatch, display name (with clan tag), and percentage share of total land tiles on the map.
3. **Bottom-Center Final `VICTORY` Banner:**
   - Appears during the final 2-second hold at the end of the video.
   - In **Free For All (FFA)** matches, displays the winning player and their final land share (e.g. `VICTORY — [MYTH] Astral (55.8%)`).
   - In **Team** matches, displays the winning Team alongside its top MVP player and combined team land share (e.g. `VICTORY — Team Red · [MYTH] Astral (66.1%)`).

> **Want a clean map without the HUD?** Pass `--no-hud` on the CLI when running `openfront-video`.

---

## 4. Standalone CLI Reference (`openfront-video`)

You can run the `openfront-video` binary directly on any machine without running the web server:

```bash
./target/release/openfront-video <game-url-or-id-or-json-path> [OPTIONS]
```

### All CLI Options

| Flag | Alias | Default | Description |
| :--- | :---: | :---: | :--- |
| `--out <file>` | `-o` | `<gameID>.mp4` | Output `.mp4` file path. |
| `--thumbnail <file>` | `--thumb` | *off* | Saves the final frame as a `.png` image. |
| `--meta <file>` | — | *off* | Writes full post-render metadata JSON (`map`, `mode`, `players`, `winner`, `winnerPct`, `bytes`, `renderSeconds`, `hashMismatches`). |
| `--preview-meta <file>` | — | *off* | Writes initial match metadata JSON immediately after loading the replay (~`0.12s`) before simulation begins. |
| `--progress-file <file>` | — | *off* | Atomically writes live render progress JSON (`tick`, `total_ticks`, `frame`, `total_frames`, `percent`, `elapsed_secs`) every 15 frames. |
| `--seconds <n>` | — | `30` | Duration of the active timelapse portion in seconds (before `--hold`). Set to `60` or `90` for a slower, more detailed video. |
| `--hold <n>` | — | `2` | Seconds to hold the final victory frame at the end of the video. |
| `--fps <n>` | — | `30` | Output video frame rate. |
| `--width <n>` | — | `1280` | Output video width in pixels (even integer; height scales automatically to the map's aspect ratio). |
| `--bitrate <Mbit/s>` | — | `4.5` | Target H.264 bitrate in Mbit/s (`4.5` produces ~`15.8 MB` for a 32s video, ideal for Discord inline embeds). |
| `--threads <n>` | — | *auto* | Number of OpenH264 slice encoder threads (scales automatically to `available_parallelism()`, capped at 4). |
| `--no-hud` | — | *off* | Disables the Broadcast HUD overlay (clock, Top-5 leaderboard, and Victory banner). |
| `--same-engine` | — | *off* | Refuses to render if the replay's `gitCommit` differs from the embedded `OPENFRONT_ENGINE_COMMIT`. |
| `--strict` | — | *off* | Aborts with a non-zero exit code if any archived tick state hash mismatches during simulation. |

### Examples

**Render a slower 60-second timelapse at 6 Mbps:**
```bash
./target/release/openfront-video cAj8yxzNsH -o slow_match.mp4 --seconds 60 --bitrate 6.0
```

**Render a clean map timelapse without the Broadcast HUD:**
```bash
./target/release/openfront-video replays/cLoKY3fNLi.json -o clean_map.mp4 --no-hud
```

---

## 5. Bot & Webhook API Reference (`openfront-web`)

### A. Poll Live Render Status (`GET /api/v1/status/{id}`)
No authentication required. Used by the waiting page UI or external tools to track render progress:

```bash
curl -s http://127.0.0.1:8080/api/v1/status/cAj8yxzNsH | jq .
```

**Response while rendering:**
```json
{
  "status": "rendering",
  "percent": 52,
  "tick": 5380,
  "total_ticks": 10311,
  "frame": 454,
  "total_frames": 960,
  "elapsed_secs": 13.2,
  "map": "Bab el-Mandeb Strait",
  "mode": "Team",
  "players": 21
}
```

**Response when complete:**
```json
{
  "status": "ready",
  "percent": 100,
  "map": "Bab el-Mandeb Strait",
  "mode": "Team",
  "players": 21
}
```

### B. Trigger or Pre-Warm a Render from a Bot (`POST /api/v1/render`)
When `RENDER_API_TOKEN` is set in the server environment, any Discord bot or tournament service can pre-warm or await a timelapse render:

```bash
curl -X POST http://127.0.0.1:8080/api/v1/render \
  -H "Authorization: Bearer $RENDER_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"game": "https://openfront.io/join/cAj8yxzNsH", "wait": false}'
```

**Response (`202 Accepted` when queued, `200 OK` when already ready or `wait: true` finishes):**
```json
{
  "id": "cAj8yxzNsH",
  "status": "rendering",
  "page_url": "https://openfront.video/game/cAj8yxzNsH",
  "video_url": "https://openfront.video/video/cAj8yxzNsH.mp4",
  "thumb_url": "https://openfront.video/thumb/cAj8yxzNsH.png"
}
```
