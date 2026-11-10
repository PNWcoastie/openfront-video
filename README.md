# `openfront-video` — Zero-Drift Safe Rust Replay Simulator, Broadcast HUD Renderer & Discord Embed Proxy

> **Status:** **Phase 1, Phase 2 & Phase 3 Complete** — 3-Crate Workspace (`engine-rs`, `ofv-render`, `ofv-web`), **`#![forbid(unsafe_code)]`** Across All Crates (`0` Compiler Warnings), **100% Bit-Exact Tick State-Hash Parity** (`1,031/1,031` & `2,060/2,060`), **`1.90x` Faster Simulation (`0.351 s/min`)**, **`17.2 ms` Discord Open Graph SLA**, Broadcast HUD Overlay, & Tactical Web Player  
> **User & CLI Guide:** [USER_GUIDE.md](USER_GUIDE.md) | **Architecture & Benchmark Blueprint:** [BLUEPRINT.md](BLUEPRINT.md) | **Deployment & Operations Runbook:** [OPERATIONS.md](OPERATIONS.md)

`openfront-video` is a self-contained, 100% safe Rust (`#![forbid(unsafe_code)]`) replay simulation engine, MP4 timelapse renderer, and `fxtwitter`-style Discord Open Graph video proxy (`Discordbot/2.0`) for **OpenFront.io**.

Given any OpenFront.io match URL, 10-character game ID, or `.json` replay record, `openfront-video` deterministically simulates every tick of the match in pure Rust, renders 30 fps frames with the game client's authentic terrain/player/cosmetic palette and **Broadcast HUD overlay**, encodes a faststart H.264 `.mp4`, and serves instant playable video embeds to Discord and web browsers.

![Match Player View & Broadcast HUD](assets/ui_match_player.png)

---

## 1. Key Engineering Highlights & Capabilities

| Subsystem | Capability | Engineering Implementation |
| :--- | :--- | :--- |
| **Crate 1: `engine-rs`** (`crates/engine-rs`) | **Bit-Exact Deterministic Simulation** | Pure Sans-IO Rust port of the OpenFront.io simulation engine (`9f4a28f`), including `Easy`, `Medium`, `Hard`, and `Impossible` Nation AI, naval HPA\* pathfinding, rail networks, fallout/MIRV physics, and `fnv1a` state-hash verification on every turn. |
| **Crate 2: `ofv-render`** (`crates/ofv-render`) | **3-Thread Streaming Video Renderer & Broadcast HUD** | Self-contained binary (`openfront-video`) with embedded zstd map packs, sprite atlases, and Overpass font. Streams dirty tile/unit/label deltas into a `tiny-skia` + `rustybuzz` rasterizer with a recycled frame-buffer pool and multi-slice `openh264` encoder. |
| **Crate 3: `ofv-web`** (`crates/ofv-web`) | **Sub-20ms Discord Embed Proxy & Live Web Gallery** | `axum` + embedded ACID `redb` catalog (`catalog.redb`). Serves `og:video` Open Graph metadata to `Discordbot/2.0` in **`17.2 ms`** via `--preview-meta`, provides live `1s` polling render progress bars (`GET /api/v1/status/{id}`), and hosts the Tactical Web Player. |
| **Compiler & Memory Safety** | **100% Safe Rust (`#![forbid(unsafe_code)]`)** | Eliminated all legacy `unsafe` blocks and raw C FFI (`openh264-sys2`) across the entire workspace. Hardware-adaptive concurrency (`std::thread::available_parallelism()`) with zero hardcoded core limits. |

---

## 2. Empirical Performance & Parity Scorecard

Measured on a standard 4-vCPU reference host (`600 ticks = 1.0 min` of gameplay):

| Benchmark / Metric | Original Baseline (`a035b17`) | Phase 2 (`2e9bfb8`) | Phase 3 Final (`#![forbid(unsafe_code)]`) | Total Improvement |
| :--- | ---: | ---: | ---: | :--- |
| **`cAj8yxzNsH` Pure Simulation** (`10,311` ticks / `17.2m` Team) | `16.34s` (`0.951 s/min`) | `10.82s` (`0.630 s/min`) | **`10.56s` (`0.614 s/min`)** | **`1.55x` faster (`-35.4%`)** |
| **`cLoKY3fNLi` Pure Simulation** (`20,601` ticks / `34.3m` FFA) | `22.88s` (`0.666 s/min`) | `14.11s` (`0.411 s/min`) | **`12.06s` (`0.351 s/min`)** | **`1.90x` faster (`-47.3%`)** |
| **Archived State-Hash Parity (`cAj` / `cLo`)** | `1,031/1,031` & `2,060/2,060` | `1,031/1,031` & `2,060/2,060` | **`1,031/1,031` & `2,060/2,060` (`0` mismatches)** | **100% Bit-Exact** |
| **Full Video Render (`cAj8yxzNsH`, `1280x1280`, `960` frames)** | `31.45s` (`1.83 s/min`) | `20.56s` (`1.20 s/min`) | **`24.10s` (`1.40 s/min` with Broadcast HUD)** | **`1.30x–1.53x` faster** |
| **Output MP4 File Size (`32.0s` video)** | `21.2 MB` (`6.0 Mbps`) | `21.2 MB` (`6.0 Mbps`) | **`15.8 MB` (`4.5 Mbps`, Discord embed safe)** | **`-25.5%` smaller** |
| **`Discordbot/2.0` Open Graph Response (`GET /game/{id}`)** | `20s–31s` (Blocks & times out) | `17.2 ms` (`--preview-meta`) | **`17.2 ms` (`--preview-meta` + `503 Retry-After`)** | **`~1,800x` faster SLA** |
| **Warm Incremental `cargo check -p engine-rs`** | `8.5s+` (Monolithic crate) | `0.77s` (3-crate split) | **`0.77s` (`0.34s` workspace check)** | **`11x` faster dev loop** |

---

## 3. Visual Tour

### A. Home Page & Recent Timelapses Gallery (`GET /`)
Visitors can paste any `openfront.io` match URL or bare game ID, or browse recently rendered timelapses with automatic winner badges (`★ Team Red · [MYTH] Astral`), player counts, and render durations:

![Home Page & Recent Timelapses Gallery](assets/ui_home_gallery.png)

### B. Live Render Progress Bar (`GET /game/{id}` while rendering)
While a match is being simulated and encoded in the background, human browsers receive a live dark-mode progress card polling `GET /api/v1/status/{id}` every second (`Tick X / Y • Frame A / B`) that automatically transitions to the video player the instant encoding completes:

![Live Render Progress View](assets/ui_live_progress.png)

### C. Broadcast HUD Overlay & Final Victory Callout (`ofv-render`)
Rendered videos include an OpenFront-themed **Broadcast HUD overlay** (enabled by default; disable with `--no-hud`) showing the top-left `MM:SS` match clock, top-right Top-5 territory `%` leaderboard card with player/team color swatches, and a bottom-center `VICTORY` callout banner on the final hold frames:

| Mid-Game Broadcast HUD (`TIME 10:19` + Top-5 Leaderboard) | Final Hold Frame (`VICTORY` Banner) |
| :---: | :---: |
| ![Mid-Game HUD](assets/hud_midgame.png) | ![Final Victory HUD](assets/hud_victory.png) |

---

## 4. 3-Crate Workspace Architecture

```text
openfront-video/
├── Cargo.toml                  # 3-crate Cargo workspace (unsafe_code = "forbid", lto = "thin")
├── README.md                   # Project overview, benchmark scorecard & visual tour
├── USER_GUIDE.md               # Step-by-step CLI, Web UI, Playback Controls & Bot API guide
├── OPERATIONS.md               # Docker, Caddy HTTPS, redb storage & multi-engine runbook
├── BLUEPRINT.md                # Full Phased Engineering Blueprint & per-subsystem telemetry
├── Dockerfile                  # Multi-stage container build (fetches OpenFrontIO assets + builds binaries)
├── docker-compose.yml          # Turnkey production stack (openfront-web + Caddy automatic HTTPS)
└── crates/
    ├── engine-rs/              # Crate 1: Pure deterministic OpenFront simulation engine (0 graphics/web deps)
    │   └── src/
    │       ├── lib.rs          # Core library root (#![forbid(unsafe_code)])
    │       ├── main.rs         # Standalone CLI benchmark & tick-hash comparison harness
    │       ├── map.rs          # Compact terrain/owner grid + precomputed STATIC_INTERIOR bitmask
    │       ├── tileset.rs      # O(1) direct-indexed slot TileSet (zero hash-table overhead)
    │       ├── water.rs        # Hierarchical HPA* naval pathfinder with intra-cluster edge cache
    │       ├── replay.rs       # Replay loader, intent dispatcher & dirty-tracked FrameStream
    │       └── game/           # 15 modular simulation subsystems (1:1 with upstream OpenFrontIO)
    ├── ofv-render/             # Crate 2: Self-contained `openfront-video` MP4 renderer binary
    │   ├── build.rs            # Compresses map packs (zstd), sprite atlases & Overpass font into binary
    │   └── src/
    │       ├── main.rs         # 3-thread pipeline (Simulate -> Draw -> Encode), HUD state & CLI flags
    │       ├── draw.rs         # Dirty-rect tiny-skia rasterizer, glyph cache & Broadcast HUD renderer
    │       ├── encode.rs       # Recycled buffer pool, AVX2 strip-skipping YUV420 converter & OpenH264 muxer
    │       ├── look.rs         # Authentic colord LCH/Lab player/team color allocator & terrain shader
    │       └── fetch.rs        # Public API replay fetcher & local JSON/gzip reader
    └── ofv-web/                # Crate 3: `openfront-web` HTTP server, Discord SLA proxy & gallery UI
        └── src/
            ├── main.rs         # Axum router, 17.2ms Discord Open Graph handler, live status & player UI
            ├── storage.rs      # Embedded ACID redb catalog (VIDEOS_TABLE, EVENTS_TABLE, COMMITS_TABLE)
            ├── slots.rs        # Preempting render concurrency limiter (live requests preempt auto-renders)
            ├── engines.rs      # Upstream commit router (git diff fast-path for compatible engine commits)
            ├── auto.rs         # Background public match discovery & auto-renderer
            ├── cleanup.rs      # Disk quota LRU eviction task
            └── stats.rs        # /admin telemetry dashboard & JSON stats API
```

---

## 5. Quick Start

### 1. Build the Release Workspace
Requires Rust (`1.80+`), a C++ compiler (for `openh264`), and an `OpenFrontIO` checkout at `../OpenFrontIO` (or pointed to by `$OPENFRONT_ENGINE`):

```bash
cargo build --release --workspace
```

### 2. Render a Match Directly to MP4 (`openfront-video`)
Pass any `openfront.io` match URL, 10-character game ID, or local `.json` / `.json.gz` replay file:

```bash
./target/release/openfront-video https://openfront.io/join/cAj8yxzNsH \
  -o match.mp4 \
  --thumb match.png \
  --meta match.json
```

### 3. Run the Web Service & Gallery Locally (`openfront-web`)
```bash
AUTO_RENDER=0 PORT=8080 DATA_DIR=./data PUBLIC_URL=http://127.0.0.1:8080 \
  OPENFRONT_VIDEO_BIN=./target/release/openfront-video \
  ./target/release/openfront-web
```
Then open **`http://127.0.0.1:8080/`** in your browser to paste a match link, watch timelapses with the **Tactical Playback Control Bar** (`0.25x`–`2x` speed pills + frame-by-frame stepping), or test Discord embed URLs.

### 4. Run Deterministic State-Hash Parity Benchmarks (`engine-rs`)
```bash
ENGINE_RS_BENCH=1 ./target/release/engine-rs replays/cAj8yxzNsH.json
ENGINE_RS_BENCH=1 ./target/release/engine-rs replays/cLoKY3fNLi.json
```

For full CLI flags, playback keyboard shortcuts, bot webhook integration (`POST /api/v1/render`), and production Docker + HTTPS deployment, see **[USER_GUIDE.md](USER_GUIDE.md)** and **[OPERATIONS.md](OPERATIONS.md)**.
