// Times the TypeScript engine on the first <ticks> ticks of a game, for
// comparing with engine-rs (ENGINE_RS_BENCH=1). Run from the OpenFrontIO
// folder like dump-ts.mts. Prints the simulation time, ticks [from, to).
//
//   TSX_TSCONFIG_PATH=$PWD/tsconfig.json node --import tsx \
//     ../engine-rs/harness/bench-ts.mts <record.json> <ticks> [fromTick]
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const [recFile, ticksArg, fromArg] = process.argv.slice(2);
const engineDir = process.cwd();
console.log = () => {};
console.warn = () => {};
const eng = (p: string) => import(pathToFileURL(path.join(engineDir, p)).href);
const { createGameRunner } = await eng("packages/engine/src/GameRunner.ts");
const { decompressGameRecord } = await eng("packages/shared/src/SharedUtil.ts");
const { GameMapType } = await eng("packages/engine-api/src/game/Maps.gen.ts");
const record = JSON.parse(fs.readFileSync(recFile, "utf8"));
const info = record.info;
const key = Object.keys(GameMapType).find((k) => GameMapType[k] === info.config.gameMap)!;
const dir = path.join(engineDir, "resources/maps", key.toLowerCase());
const bin = (f: string) => new Uint8Array(fs.readFileSync(path.join(dir, f)));
const runner = await createGameRunner(
  info,
  undefined,
  {
    map: info.config.gameMap,
    mapSize: info.config.gameMapSize,
    manifest: JSON.parse(fs.readFileSync(path.join(dir, "manifest.json"), "utf8")),
    mapBin: bin("map.bin"),
    map4xBin: bin("map4x.bin"),
    map16xBin: bin("map16x.bin"),
  },
  () => {},
);
// Same as render.ts: the per-player UI snapshots are skipped.
const game: any = runner.game;
Object.getPrototypeOf(game.allPlayers()[0]).toUpdate = () => null;
const turns = decompressGameRecord(record).turns;
const n = Math.min(Number(ticksArg), turns.length);
const from = Number(fromArg ?? 0);
let ms = 0;
for (let i = 0; i < n; i++) {
  runner.addTurn(turns[i]);
  const t0 = performance.now();
  runner.executeNextTick();
  if (i >= from) ms += performance.now() - t0;
}
process.stdout.write(`TypeScript: ticks ${from}-${n - 1}: ${ms.toFixed(1)} ms\n`);
