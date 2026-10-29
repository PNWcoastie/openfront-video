// Generates random water-path queries for a game's map and answers them with
// the TypeScript WaterPathFinder, for comparing with `engine-rs paths`.
// Run from the OpenFrontIO folder:
//
//   TSX_TSCONFIG_PATH=$PWD/tsconfig.json node --import tsx \
//     ../engine-rs/harness/paths-ts.mts <record.json> <count> <queries.json> <ts-paths.json>
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const [recFile, countArg, queriesFile, outFile] = process.argv.slice(2);
const engineDir = process.cwd();
console.log = () => {};
console.warn = () => {};
const eng = (p: string) => import(pathToFileURL(path.join(engineDir, p)).href);
const { createGameRunner } = await eng("packages/engine/src/GameRunner.ts");
const { GameMapType } = await eng("packages/engine-api/src/game/Maps.gen.ts");
const { WaterPathFinder } = await eng("packages/engine/src/pathfinding/PathFinder.ts");
const { PseudoRandom } = await eng("packages/engine-lib/src/PseudoRandom.ts");
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
const game: any = runner.game;
const N = game.width() * game.height();

// Shore land tiles (where boats launch and land) and some open water.
const shores: number[] = [];
for (let t = 0; t < N; t++) if (game.isShore(t)) shores.push(t);
const random = new PseudoRandom(Number(process.env.SEED ?? 42));
const pick = () => shores[random.nextInt(0, shores.length)];
const queries: (number | number[])[][] = [];
for (let i = 0; i < Number(countArg); i++) {
  const kind = i % 4;
  if (kind === 3) {
    // Several sources, like a player with shore on many fronts.
    const n = random.nextInt(2, 6);
    queries.push([Array.from({ length: n }, pick), pick()]);
  } else if (kind === 2) {
    // A nearby target: within 150 tiles.
    const a = pick();
    let b = pick();
    for (let k = 0; k < 200 && game.manhattanDist(a, b) > 150; k++) b = pick();
    queries.push([a, b]);
  } else {
    queries.push([pick(), pick()]);
  }
}

const pf = new WaterPathFinder(game);
const t0 = performance.now();
const out = queries.map(([from, to]) => pf.findPath(from, to as number));
const ms = performance.now() - t0;
fs.writeFileSync(queriesFile, JSON.stringify(queries));
fs.writeFileSync(outFile, JSON.stringify(out.map((p: any) => (p ? Array.from(p) : null))));
process.stdout.write(`${queries.length} queries in ${ms.toFixed(0)} ms, ${out.filter((p: any) => p).length} found\n`);
