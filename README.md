# openfront-render

Turns an OpenFront.io replay into a sped-up MP4 and reports how long each step took.

The video uses the game client's own look, taken from the checkout: its theme for
player and team colors (including cosmetic colors), its terrain colors, its structure
and unit sprites, and the engine's name placement with troop counts. Patterns, skins,
flags and effects like the fallout glow aren't drawn.

It runs OpenFront's own game engine from a local checkout, replays every tick of the
game, draws the territory map every few ticks and streams the frames into ffmpeg
while the simulation keeps going. At the end it checks the replay against the state
hashes stored in the game record, so you know the video matches what players saw.

## Requirements

- Node.js and npm in the versions OpenFrontIO's `package.json` asks for (currently
  Node 24.15+ and npm 12; with an older npm, use `npx -y npm@12 ci` instead of `npm ci`)
- git
- ffmpeg on your PATH (or pass `--ffmpeg C:\path\to\ffmpeg.exe`)
- An OpenFrontIO checkout with dependencies installed

## Setup

Install this tool's one dependency (`@napi-rs/canvas`, for names, icons and sprites):

```
npm install
```

Then get the engine:

```
git clone https://github.com/openfrontio/OpenFrontIO.git
cd OpenFrontIO
npm ci
```

Every game has to be replayed with the exact engine version it was played on. The
tool tells you if the versions differ and prints the command to fix it, which is:

```
git fetch origin <gitCommit from the warning>
git checkout <gitCommit from the warning>
npm ci
```

Fetching the commit by name also works in a shallow clone. Both engine layouts work:
the older one with the engine in `src/core` and the newer one split into `packages/`.

## Usage

Run it from inside the OpenFrontIO folder (that's where `tsx` is installed):

```
node --import tsx ..\render.ts https://openfront.io/w0/game/cAj8yxzNsH --engine .
```

`npx tsx ..\render.ts ...` works too, but npx adds about 1.3 s to every run before the
script starts.

You can pass a full link, just the game ID, or a saved record file (`.json` or `.json.gz`).

Games are downloaded from OpenFront's documented public API,
`https://api.openfront.io/public/game/<gameID>` (see `docs/API.md` in OpenFrontIO). The
game client's own endpoint, `/game/<gameID>`, returns the same record but sits behind a
Cloudflare bot check that blocks scripts. If the public one is ever blocked too, the
tool says so: open the game's link in your browser, save the page as `<gameID>.json`
and pass that file instead.

| Option | Default | |
|---|---|---|
| `--engine <dir>` | `./OpenFrontIO` | OpenFrontIO checkout |
| `--out <file>` | `<gameID>.mp4` | Output video |
| `--seconds <n>` | 30 | Timelapse length |
| `--fps <n>` | 30 | Frames per second |
| `--width <n>` | 1280 | Video width; height follows the map |
| `--hold <n>` | 2 | Seconds to hold the final frame |
| `--crf <n>` | 23 | x264 quality (lower = better, bigger) |
| `--preset <name>` | veryfast | x264 preset |
| `--strict` | off | Stop as soon as the replay desyncs |
| `--cache-dir <dir>` | `.cache` next to `render.ts` | Where simulated games are kept |
| `--no-cache` | off | Don't read or write the cache |
| `--resim` | off | Simulate again even if the game is cached |
| `--threads <n>` | cached: a third of the CPU cores, at most 6; `--rust`: 2 | Drawing threads for cached games and `--rust` |
| `--rust <exe>` | off | Simulate with engine-rs (the Rust port) instead of the TypeScript engine; no cache |

## Workspace layout

The Rust code lives in a three-crate Cargo workspace (`#![forbid(unsafe_code)]` throughout):

- `crates/engine-rs/` — Pure deterministic simulation engine (`engine-rs` library and CLI harness), with zero graphics or network dependencies (`cargo check -p engine-rs` runs in under a second).
- `crates/ofv-render/` — The standalone `openfront-video` binary (`draw.rs`, `encode.rs`, `look.rs`, `fetch.rs`, and `build.rs` for the embedded map pack, sprite atlases, and font).
- `crates/ofv-web/` — The `openfront-web` HTTP service (`axum` + embedded `redb` catalog in `DATA_DIR/catalog.redb`).

## Rendering with the Rust engine

`crates/engine-rs/` is a Rust port of the simulation (see its README). Build it once with
`cargo build --release -p engine-rs`, then pass the binary:

```bash
node --import tsx ../render.ts ../cAj8yxzNsH.json --engine . --rust ../target/release/engine-rs
```

render.ts starts `engine-rs stream`, which simulates the game and writes, at each
video frame's tick, the tiles that changed, the units and the name labels. render.ts
turns that into frames for the same video thread as always, reading engine-rs's output
as fast as it arrives so the simulation keeps going while frames draw. The cache isn't
read or written. Frames are drawn on 2 threads by default (`--threads`): each keeps its
own copy of the map and draws every other frame. The replay check runs in engine-rs and
is reported the same way.

The simulation is the limit here: engine-rs needs about 11 s for the test game on its
own, but about 18 s inside a render (the pipe adds about 2.5 s; sharing the machine
with drawing and encoding the rest). The thread count barely matters any more.
`OPENFRONT_DRAW_PROFILE=1` prints where each drawing thread spends its time. Most of
it goes to reading the finished frame back from the canvas, which is where the canvas
library actually draws the queued sprites and text.

For the test game the video is identical frame for frame to the TypeScript render (all
960 frames of the 32-second video compared). Speed for that game:

| Video | TypeScript engine | engine-rs |
|---|---|---|
| 32 s, 1280x1280 | 45–70 s | about 19 s (1.1 s/min) |
| 5 s, 1280x1280 | 36.7 s (2.13 s/min) | 20.4 s (1.18 s/min) |

engine-rs is verified tick-for-tick against the TypeScript engine (engine commit `9f4a28f`), including `Easy`, `Medium`, `Hard`, and `Impossible` Nation AI, private team troop donations, and team shuffling.

## openfront-video: one self-contained binary

`openfront-video` (`crates/ofv-render/`) does the whole job in Rust: it fetches the game, simulates it with
`engine-rs`, draws the frames like `render.ts` and encodes the MP4 itself. It needs no
Node, no ffmpeg and no OpenFrontIO checkout at run time. Maps, tribe names, the
client's theme and render settings, the sprite atlases and the name font are built
into the executable.

```bash
cargo build --release --bin openfront-video
```

```bash
./target/release/openfront-video https://openfront.io/w12/game/cLoKY3fNLi
```

Building needs the OpenFrontIO checkout at `../OpenFrontIO` (or `$OPENFRONT_ENGINE`),
at the engine commit the port follows, and a C++ compiler for OpenH264. The first
build takes a few minutes because every map is compressed (cached across rebuilds in `OUT_DIR`). It takes a game link, a
game ID or a record file (`.json` or `.json.gz`). Options: `--out` (`-o`), `--seconds`,
`--fps`, `--width` and `--hold` as in render.ts, plus `--bitrate <Mbit/s>` (default 4.5),
`--threads <n>` for the encoder (hardware-adaptive default), `--thumbnail <file>` (`--thumb`), `--meta <file>`, `--preview-meta <file>`, `--progress-file <file>`, `--no-hud`, `--same-engine` and `--strict`.

It runs on three threads with a recycled frame buffer pool. The simulation passes each frame's changes to a drawing
thread through a queue of 10 frames and waits whenever the queue is full, so it never
gets more than 10 frames ahead of drawing. The drawing thread passes finished frames to
an encoding thread. Drawing uses tiny-skia, and text is shaped with rustybuzz, so
kerning matches the canvas. Text lines, outlines and unit sprites are cached per quarter-pixel
offset and blitted directly onto dirty canvas regions. Frames look the same as render.ts's, up to anti-aliasing
and encoder differences, including team colors, player cosmetics and fallout.
On top of what render.ts drew, it shows the game client's status icons above names
(crown for the largest player, traitor, disconnected, nuke in flight, doomsday skull,
from `status-atlas.png`), the red blast-radius circles at nuke targets, and an OpenFront-themed **Broadcast HUD overlay** (top-left `MM:SS` match clock, top-right Top-5 territory `%` leaderboard card, and a final-frame `VICTORY` callout banner on hold frames; pass `--no-hud` to disable).
`OPENFRONT_DRAW_PROFILE=1` times the drawing steps.

The video is H.264 from OpenH264 (BSD-licensed), compiled in, with BT.709 colors. At the default 4.5 Mbit/s the 32-second test video is about
15.8 MB (safely below Discord's inline embed limit). Without NASM installed at build time OpenH264 has no assembly, so it
encodes frames as slices across encoder threads to keep pace with the simulation. Characters
the Overpass font lacks (CJK or emoji in names, say) are left out, since only that
one font is built in.

`--thumbnail <file>` saves the last frame as a PNG, `--meta <file>` writes the
game's and video's details (including the match winner and land share) as JSON once rendering finishes, `--preview-meta <file>` writes the initial game metadata JSON immediately after loading the replay (~0.15 s) so the web service can answer link previewers before the video finishes encoding, and `--progress-file <file>` atomically updates live simulation/frame progress JSON during rendering. The MP4 is assembled in memory and written with its index at the front (`+faststart`), so playback starts before the download finishes.

## openfront.video (web service)

`crates/ofv-web/` is a small server (axum + embedded `redb` catalog) around `openfront-video`, and the `Dockerfile` builds
both:

```bash
docker build -t openfront-video .
```

```bash
docker run -d -p 8080:8080 -v openfront-videos:/data --name openfront-video openfront-video
```

The image build fetches OpenFrontIO at the pinned engine commit (only the maps,
atlases, fonts and theme files) and needs nothing from this folder besides the workspace
crates. Put a TLS-terminating proxy (Caddy, nginx, Cloudflare) in front for
`https://openfront.video`.

| Path | |
|---|---|
| `/` | Paste a game link or ID, plus the **Recent Timelapses** gallery |
| `/game/<id>`, `/join/<id>`, `/<id>`, `/<anything>/game/<id>` | The game's page with the video, **Tactical Playback Control Bar** (`0.25x`–`2x` speed pills, frame-by-frame stepping, jump to Victory frame), and **Copy Discord Link** button. Swapping `openfront.io` for `openfront.video` in a game link works |
| `/video/<id>.mp4` | The video, with byte ranges |
| `/thumb/<id>.png` | The last frame |
| `GET /api/v1/status/<id>` | Live JSON render progress (`status`, `percent`, `tick`, `total_ticks`, `frame`, `total_frames`, `elapsed_secs`) |
| `POST /api/v1/render` | Optional bearer-authenticated endpoint (`RENDER_API_TOKEN`) for bots to queue or pre-warm a game render |
| `/health` | `ok` |

The first request for a game starts its render. Afterwards the video, thumbnail and
details (`<id>.mp4`, `.png`, `.json` in `/data`, indexed in `/data/catalog.redb`) are served from disk, with year-long
cache headers since a game's video never changes. Renders write to temporary files
and are moved into place only when complete. A game renders once even when many
requests arrive together, and at most `MAX_RENDERS` games render at a time. A failed
render (an unported feature, an unknown game) is remembered for 10 minutes, and its
page shows the renderer's message.

The page carries Open Graph and Twitter player tags (`og:type video.other`,
`og:video` with type and size, `og:image` = the thumbnail), which is what Discord
needs to show a playable video in the embed. People get a live progress bar page immediately (`GET /api/v1/status/<id>` polled every 1 s with tick/frame telemetry) that automatically reloads into the video player the instant rendering finishes. Link previewers (Discordbot and others,
recognized by user agent) receive the full `og:video` Open Graph HTML page as soon as
`--preview-meta` writes the game's metadata (~0.15 s after fetching the replay), so
Discord's link scraper gets an immediate `<0.3 s` response and then opens a separate
request to `/video/<id>.mp4` (which waits up to `VIDEO_WAIT_SECS` for the finished MP4).
If the preview metadata isn't ready yet, link previewers get `503 Service Unavailable`
with `Retry-After: 5` rather than a `200 OK` placeholder page, so Discord never caches
a text-only card.

### Deploying with HTTPS

`docker-compose.yml` runs the app behind Caddy, which gets and renews Let's Encrypt
certificates by itself, redirects HTTP to HTTPS and `www.` to the bare domain. On a
Linux server with Docker:

1. Point DNS A (and AAAA) records for the domain and `www.` at the server. Open ports
   80 and 443 (443/udp too, for HTTP/3).
2. Copy the repo there. The image build fetches the engine assets itself, so the
   OpenFrontIO checkout isn't needed.
3. `cp .env.example .env`, then set `DOMAIN` and `ACME_EMAIL`.
4. Start it:

```bash
docker compose up -d --build
```

The first build takes several minutes: compiling, and compressing every map. Caddy
asks Let's Encrypt for certificates once DNS resolves to the server. If it can't
yet, `docker compose logs caddy` shows why, and it keeps retrying. Videos live in the
`videos` volume and certificates in `caddy_data`; both survive rebuilds. Update with
`git pull` and the same command. Renders take several CPU cores each; raise
`MAX_RENDERS` only on a bigger machine.

### Rendering games as they end

With `AUTO_RENDER=1` (the default) the server also renders public games without
waiting to be asked. Every `AUTO_POLL_SECS` (60) it lists public games from
`api.openfront.io/public/games`. That listing filters by start time, so it looks back
`AUTO_LOOKBACK_MINS` (180), since a game can run for about three hours. It queues
games that have ended and that it can render: unranked, with at
least `AUTO_MIN_PLAYERS` (10) players.

About ten public games end every minute, more than one machine renders. So the queue
takes the newest game first and drops games that ended more than
`AUTO_MAX_AGE_MINS` (30) ago. Renders share `MAX_RENDERS` slots (2 by default) between requests and the queue.
A request never waits behind an automatic render: when every slot is busy, an
automatic render is stopped, its slot goes to the request and its game goes back on
the queue. No automatic render starts while a request is waiting. It goes through the same
render path, so a game is never rendered twice. Auto renders pass `--same-engine`, so
a game played on a different engine version is skipped in under a second instead of
rendering a video that drifts. When OpenFront deploys a new engine version, auto
rendering produces nothing until the port catches up.

### Ads

Set `ADSENSE_CLIENT` to your AdSense publisher ID (`ca-pub-…`) to load the AdSense
script, which serves Auto ads. Optionally set `ADSENSE_SLOT` to an ad unit ID, which
adds a responsive unit under the video and under the home page form. The server then
also serves `/ads.txt` with your publisher ID, which AdSense checks. Ads appear only on
the home page and finished video pages. They're left off the page shown while a game
renders, because it reloads itself every 5 seconds and AdSense forbids ads on
auto-refreshing pages. Neither ID is required; without them there are no ads.

Settings (environment): `PORT` (8080), `DATA_DIR` (`/data` in the image),
`PUBLIC_URL` (`https://openfront.video`, used for the absolute URLs in the tags),
`MAX_RENDERS` (2), `EMBED_WAIT_SECS` (25), `VIDEO_WAIT_SECS` (120: how long a video
request waits for a render in progress), `MAX_STORAGE_GB` (35), `OPENFRONT_VIDEO_BIN`,
`RENDER_TS` and `ENGINES_KEEP` (see below).

### Games from other engine versions

The Rust renderer follows the engine versions in `replay::ENGINE_COMMITS`. When a game
was played on another commit, `openfront-video --same-engine` stops right away with
exit code 4 and names the commit. The server first checks whether `packages/engine/`,
`packages/engine-lib/` or `src/core/` actually changed between the Rust build's engine
commit and the game's commit (`git diff --quiet`, cached in `/data/catalog.redb`). If
only client/UI/server files changed, the server renders the game directly with the
fast Rust `openfront-video` binary. Otherwise it makes a checkout of OpenFrontIO at
that commit in `/data/engines/<commit>`: a shallow, sparse clone plus `npm ci --omit=dev`,
about 2 minutes and 1 GB the first time, and renders with `render.ts --engine <checkout> --strict`.
The `ENGINES_KEEP` (3) most recently used checkouts are kept, outside `MAX_STORAGE_GB`.
`RENDER_TS` is `/app/render.ts` in the image; without it, the fallback is off and such
games render with the Rust engine anyway, where they may desync. Auto renders skip them in that case.

### Admin dashboard

Set `ADMIN_PASSWORD` and open `/admin`. The browser asks for a login: any user name
plus that password. Without the setting, `/admin` returns 404. The dashboard refreshes
every 15 seconds and shows:
- **Totals:** stored videos and storage against the limit; renders in the last 24 hours
  by outcome; average render time; views and Discord embeds; busy render slots and the
  queue; what cleanup deleted.
- **Hourly charts, last 48 hours:** renders by outcome (automatic, requested, not
  finished, stopped for a request, failed) and views (page views, video plays, embeds).
- **Breakdowns:** storage by when each game was last viewed (cleanup removes the oldest
  first), render time distribution, top maps and modes among stored videos.
- **Render queue:** a bar with one piece per game. From left to right: the last 12
  finished or failed renders, the games rendering now (automatic or requested), and
  the waiting queue, next first. Games close to being dropped for age are amber.
  Hover a piece for the game and its timing.
- **Visitors:** distinct visitors in the last 24 hours. Requests per hour by source
  (people, Discord, other bots and apps). Requests and distinct visitors per source
  (browsers, Discord, X, Slack, Telegram, WhatsApp, Facebook, other bots). What each
  group opens (home, video page, rendering page, not-finished page, video plays). The
  source comes from the User-Agent. A visitor is a hash of the IP address, taken from
  Caddy's or Cloudflare's forwarding headers, with a secret salt in
  `/data/.visitor-salt`. The IP itself is never stored.
- **Tables:** what's rendering now, the most viewed games, recent renders, and why
  renders failed over the last 7 days, with an example game for each reason.

The data comes from the stored games' details files and an event log,
`/data/events.jsonl` (renders; every page view and video play with its source and hashed visitor; cleanups), which keeps
30 days.

### Storage cleanup

Videos are kept under `MAX_STORAGE_GB` (35 by default, for a 40 GB volume; 0 turns
the limit off). A game takes about 23 MB, the video plus its thumbnail, so 35 GB holds
roughly 1,500. Each view of a game's page, video or thumbnail bumps its details file's
modified time (at most once an hour). That's the "last viewed" time, and it survives
restarts. Every 5 minutes, and after each finished render, the server adds up usage.
Over the limit, it deletes the least recently viewed games until usage is under 90%
of the limit, so new renders don't trigger a deletion every time. The log says what it
removed. A deleted game is rendered again the next time someone asks for it. With
automatic rendering on, the folder reaches the limit within hours and then cycles:
videos people watch stay, and unwatched automatic renders make room.

## Re-rendering from the cache

The first render of a game saves what the simulation produced (every tick's tile
changes, units and readable names, plus player colors and the replay check) to
`.cache/<gameID>-<engine commit>/`, about 30 MB for a 17-minute game. Rendering the
same game again with the same engine commit skips the engine: several threads draw the
frames from the cache and one ffmpeg encodes them. Any length, frame rate, width or
quality works. The result is byte-for-byte the same video a simulating render makes.

For the test game, a 32-second 1280x1280 video takes about 13 s from the cache instead
of 45–70 s, and a 12-second 720x720 one about 4.5 s. Most of that is x264 itself,
which uses all cores while it encodes, so more drawing threads don't help much past a
third of the cores.

The cache is written to a temporary folder and only moved into place when complete,
so an interrupted render never leaves a broken one. Changing the drawing code doesn't
need `--resim`, but changing how colors or names are worked out on the main thread
does.

## Performance metric

Every run ends with one number to compare runs by: **seconds of rendering per minute
of gameplay** (lower is better), using the game's length from its record.

```
Speed: 2.39 s of rendering per minute of gameplay (41.2 s for 17.3 min)
Speed: 0.58 s of rendering per minute of gameplay (10.0 s for 17.3 min, from the cache)
```

Compare runs at the same video settings. The simulation grows with the game's length
and how much fighting there is, while drawing and encoding grow with the number of
video frames, so a 30-second video of a long game scores better per minute than the
same video of a short one. Machine load also moves the number a lot, so compare runs
made back to back.

## Example output

A 5-second video of the test game with the TypeScript engine (`--no-cache --seconds 5 --hold 0`):

```
Fetching replay ../cAj8yxzNsH.json ...
  cAj8yxzNsH: Bab el-Mandeb Strait, Team, 21 players, 10,311 ticks (17 min of play)
  Map 1500x1500, engine ready in 0.69 s
Rendering 10,311 ticks into 150 frames (5s at 30 fps, 1280x1280, ~69 ticks per frame)
  tick 10,311/10,311  100.0%  | 276 ticks/s  | frame 150/150  | 37.9s elapsed

Done: cAj8yxzNsH.mp4
  1280x1280, 5.0s of video, 2.6 MB

                                          0 s                                      38.81 s
  Fetch record                    0.04 s  █                                               |
  Load engine + map               0.69 s  █                                               |
  Simulate (main thread)         37.95 s  ████████████████████████████████████████████████|
  Draw + encode (video thread)   38.04 s  ████████████████████████████████████████████████|

  Simulation: 37.33 s of work (276 ticks/s); collecting units 0.38 s, preparing frames 0.17 s
  Drawing: 3.89 s for 150 frames (25.9 ms each)
  Main thread waited 0.01 s for the video thread; ffmpeg held up writes for 0.07 s

Speed: 2.25 s of rendering per minute of gameplay (38.8 s for 17.3 min)
Replay check: all 1,031 archived state hashes matched
```

The simulation runs on the main thread. After each frame's ticks it sends what changed
(tile owners, units, names) to a video thread, which draws the frame and feeds ffmpeg,
so drawing and encoding overlap with the simulation instead of adding to it.

The timeline at the end puts every stage on one time axis, so stages that overlap show
up as overlapping bars and the longest bar that ends last is the bottleneck. Below it:

- **Work** totals: how busy the simulation and the drawing were, which can be less than
  their bars (they wait for each other) or more (drawing on several threads).
- **Main thread waited ... for the video thread**: time the simulation sat idle because
  the video thread had frames queued. Near zero means the simulation is the bottleneck,
  which is the normal case.
- **ffmpeg held up writes**: if this grows, the encoder needs more CPU (try a faster
  `--preset`).
