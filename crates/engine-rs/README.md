# engine-rs

A Rust port of the OpenFront.io simulation, built to replay games bit-identically to
the TypeScript engine and faster. It follows engine commit `9f4a28f` and v35's `c144d13` and `7ee9686`, which only add and change the onboarding tutorial (tutorial games stop the run); `replay::ENGINE_COMMITS` lists the versions it accepts. It is being
built up one game at a time, starting with `cAj8yxzNsH` (Bab el-Mandeb Strait, 7
teams, 400 bots, nations off).

## Status

| Milestone | Scope | State |
|---|---|---|
| 1 | Map, random numbers, players and teams, bot names and IDs, intents, spawn phase | **Done: ticks 0–200 identical**, all 21 archived checkpoints match |
| 2 | End of spawn: win check, disconnects, player income, bot AI, attacks, conquering, territory checks | **Ported; ticks 0–242 identical**, 25 checkpoints match. Stops at tick 243, the game's first boat |
| 3 | Boats and water pathfinding, ports, cities, defense posts, warships, silos, nukes, SAMs, MIRVs, trains, factories, alliances, embargoes, donations | **Done for this game: all 10,311 ticks identical**, all 1,031 archived checkpoints match. Everything the game uses is ported: water pathfinding, boats, alliances, betrayals and extensions, all structures and upgrades, trade ships, rails and trains, warships, atom and hydrogen bombs, the MIRV, fallout and land it cuts off, SAM interception, donations. Embargoes are ported only as far as trading checks them (no embargo intents in this game) |
| 4 | Nations, other modes and map sizes, plugging into render.ts | **In progress:** `engine-rs stream` feeds render.ts (`--rust`); the first game's video is identical frame for frame to the TypeScript render. Nations are ported for **Easy, Medium and Hard difficulty**, in Free For All and team games (Impossible stops the run): game `cLoKY3fNLi` (Taiwan Strait, 25 nations, 400 bots, overtime) matches all 2,060 archived checkpoints over its 20,601 ticks. Ported for it: the whole nation AI (spawning, attacks and boat attacks, alliances, betrayals, emojis, embargoes, structures and upgrades, warships, atom and hydrogen bombs, MIRVs), embargo, emoji, target, boat-cancel and delete-unit intents (game `c8jQSXkPhQ`, Central America, 41 minutes: all 2,428 checkpoints match), and nukes being cancelled when their launcher and target ally. Easy, custom nation counts (extra nations from `additionalNations`, then generated names) and singleplayer: game `1jJ6r93n` (Crimea, singleplayer against 400 Easy nations) matches the TypeScript engine on all 2,803 tick lines and its 29 checkpoints. Compact maps (the 4x map for play, the 16x one for pathfinding) and Duos/Trios/Quads team modes: games `cHVgecE2db` (Baja California, compact, 4 teams), `cPU6z9i6R8` (Finger Lakes, compact, Trios), `czvSkYZnWs` (Caribbean, Duos) and `cZupEn4WHa` (Benelux, Quads, whose unknown tribe name theme falls back to the default) match the TypeScript engine tick by tick. Also matching tick by tick: random spawn (`d42whoUP1c`), the Doomsday Clock (`dnpdFLq4eX`: rising territory bar, troop and warship drain, territory rot), boats captured with a disconnected teammate's land (`dW3g9McMKm`), and nations answering donations with emojis. Hard nations and Humans vs Nations: game `dkoQUFcL6X` (Europe, compact, 14 humans against Hard nations) matches the TypeScript engine on all 6,723 tick lines and its 672 checkpoints (beachhead boats, bot annexation, retaliation, troop caps, strikes, SAM-safe nuke trajectories, warships clearing sea lanes and countering warship hoards, team nuke targets). Team spawn areas from the map manifest (halved on compact maps), for random spawns and nations: game `dNVDLbQDBj` (Baikal Nuke Wars, Humans vs Nations, Hard) matches the TypeScript engine on all 8,334 tick lines and its 833 checkpoints. Maps whose water graph is small use the plain `AStarWater` search (game `cGQVy6WEZ7`, Alps, with an embargo-all intent: all 1,099 checkpoints match) |

Covered and matching over the whole game (17 minutes of play, won by team Red): boats,
attacks, eliminations, alliances, betrayals and extensions, bots attacking traitors,
all six structures and their upgrades, trade ships and their capture, the rail network
and trains, warships, 40 atom bombs, 13 hydrogen bombs and a MIRV with its warheads,
SAMs shooting nukes down, fallout and the land it cuts off, troop and gold donations.
Not exercised by this game: embargo intents, nations, other modes (see `cLoKY3fNLi` above for nations).

One engine quirk worth knowing: `UnitGrid` picks the 100×100 cells to search with a
formula that skips the next cell when a unit sits exactly at the query range on an
axis, so the engine can miss a unit that is in range. The Rust queries apply the same
cell bounds (`in_grid_cells`); without that, a defense post exactly 30 tiles away
changed an attack at tick 4696.

The water pathfinding was also checked on its own: `harness/paths-ts.mts` asks the
TypeScript `WaterPathFinder` for random shore-to-shore routes, `engine-rs paths`
answers the same queries, and 5,800 routes (up to 2,728 tiles long) came out
identical tile for tile.

## Speed so far

Unoptimized, single game, simulation only (`ENGINE_RS_BENCH=1` and
`harness/bench-ts.mts`, which skips the UI snapshots like render.ts):

| Ticks | TypeScript | Rust | |
|---|---|---|---|
| 0–200 (spawn phase) | ~130 ms | ~11 ms | ~10× |
| 201–242 (expansion) | 158–207 ms | 39–56 ms | ~3.5–4× |
| 0–822 (expansion and fighting) | 3.56–3.66 s | 1.15–1.27 s | ~3× |
| 0–2999 (first 5 minutes) | 16.7 s | 3.9 s | ~4.3× |
| 0–10310 (whole game, 17 minutes) | 26.0 s | 11.4–12.6 s | ~2.2× |

The nations game `cLoKY3fNLi` (20,601 ticks, 34 minutes) simulates in ~14.7 s. A unit
cell index for `nearby_units` (like the engine's `UnitGrid`) and dropping deleted units
from the per-type lists took it from ~20.4 s; warships and trade ships asking for
nearby ships were scanning every ship ever built.

`ENGINE_RS_PROFILE=1` also prints time in a few named scopes (`prof::scope`; water
pathfinding, cluster checks, `nearby_units`, stream frames). Add a scope to any
function to time it. `TileSet` uses direct flat slot-indexed position arrays (zero hashing on insert, delete or compaction), `Game` precomputes a `territory_sigmoid` lookup table across land tile counts, `GameMap` precomputes static shore/edge/interior flags, and `WaterPathfinder` caches intra-cluster HPA* edge paths and reuses `BoundedAStar` scratch buffers.

The simulation state and executions in `src/game.rs` are organized into focused domain submodules under `src/game/` (`units.rs`, `alliances.rs`, `structures.rs`, `ports_trade.rs`, `rail.rs`, `warships.rs`, `severed.rs`, `nukes.rs`, `namebox.rs`, `doomsday.rs`, `nation.rs`, `nation_attack.rs`, `nation_structures.rs`, `nation_warships.rs`, `nation_nukes.rs`), and the entire crate compiles under `#![forbid(unsafe_code)]`.

Anything not ported stops the run with "... is not ported yet".

## Making videos

`openfront-video` (`crates/ofv-render/`, see the top-level README) is the
standalone video maker: it uses this crate as a library (`src/lib.rs`), with
`replay::play` producing each frame's changes, and `crates/ofv-render/build.rs` embeds the maps and
client assets from the OpenFrontIO checkout. The harness binary `engine-rs` still
reads them from the checkout given on its command line.


`engine-rs stream <record.json> <OpenFrontIO dir> <frames>` writes what render.ts
draws (changed tiles, units, name labels, new players) at each video frame's tick to
stdout; the format is described above `stream_mode` in `src/main.rs`. Name labels come
from a port of the engine's `NameBoxCalculator`, placed again on the same ticks as
`GameRunner` does. Use it through `render.ts --rust` (see the top-level README).

Streaming costs more than the bare simulation (about 16 s instead of 11.4 s for the
test game): every 30 ticks every player's name is placed again, and each frame
compares the whole owner map with the previous one.

## Checking it against the engine

The harness compares the full state after every tick: the game hash, a fingerprint of
every tile's owner and fallout, the queued and running executions, and every player's
troops, gold, units (tile, troops, level, construction, deletion timer, health), tiles, and tile and border sets in insertion order (that order decides
later random draws, so it has to match too).

From the `OpenFrontIO` folder, at the game's engine commit:

```
TSX_TSCONFIG_PATH=$PWD/tsconfig.json node --import tsx ../engine-rs/harness/dump-ts.mts ../cAj8yxzNsH.json 202 ../engine-rs/harness/ts.jsonl
```

Then from `engine-rs`:

```
cargo build --release
./target/release/engine-rs ../cAj8yxzNsH.json ../OpenFrontIO 201 harness/rust.jsonl
node harness/compare.mjs harness/ts.jsonl harness/rust.jsonl
```

`compare.mjs` reports the first tick where anything differs and which fields. Add a
fourth argument to `dump-ts.mts` (a tick number) to also dump the full owner map at
that tick, for finding the exact tile; `ENGINE_RS_DUMP_AT=<tick>` makes `engine-rs` write
the same map. `compare.mjs` streams both files, so whole-game dumps (about 1 GB each)
work.

When a nation's choices differ, compare its random draws. `harness/trace-ts.mts
<record> <smallID> <tick | all> [last tick]` prints every `nextInt` a nation makes in
the TypeScript engine, and `ENGINE_RS_TRACE_NATION=<smallID>:<tick | all>` makes
`engine-rs` print the same to stderr. The first differing draw points at the
behavior. With `SAM_TRACE=1` it also lists SAM missile launches, and with
`DELETE_TRACE=<unit id>` it prints the stack that deletes that unit.

## Rules for porting

- Follow the TypeScript line by line, including iteration order, when random numbers
  are drawn, and float64 arithmetic order (the hash is a float sum in player order).
- `GameMapImpl.bfs` pops from the end of its queue (depth first); keep that.
- Executions start (`init`) and are removed exactly as `GameImpl.executeNextTick`
  does; during the spawn phase only spawn-phase executions start.
- Static data (tribe name themes, map manifests, map bins) is read from the engine
  checkout's `resources`, never copied.
