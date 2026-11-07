// Prints every PseudoRandom.nextInt call one nation makes during one tick, for
// comparing with `ENGINE_RS_TRACE_NATION=<smallID>:<tick> engine-rs ...`.
// Run from the OpenFrontIO folder like dump-ts.mts:
//
//   TSX_TSCONFIG_PATH=$PWD/tsconfig.json node --import tsx \
//     ../engine-rs/harness/trace-ts.mts <record.json> <nation smallID> <tick | all> [last tick]
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const [recFile, smallArg, tickArg] = process.argv.slice(2);
const engineDir = process.cwd();
const small = Number(smallArg);
// "all": every tick (each tick's draws follow a "tick N" line).
const all = tickArg === "all";
const tick = all ? Infinity : Number(tickArg);
const realLog = console.log.bind(console);
console.log = () => {};
console.warn = () => {};
console.info = () => {};

const eng = (p: string) => import(pathToFileURL(path.join(engineDir, p)).href);
const { createGameRunner } = await eng("packages/engine/src/GameRunner.ts");
const { decompressGameRecord } = await eng("packages/shared/src/SharedUtil.ts");
const { GameMapType } = await eng("packages/engine-api/src/game/Maps.gen.ts");
const { PseudoRandom } = await eng("packages/engine-lib/src/PseudoRandom.ts");
const { NationExecution } = await eng("packages/engine/src/execution/NationExecution.ts");
const { SAMMissileExecution } = await eng("packages/engine/src/execution/SAMMissileExecution.ts");
// SAM_TRACE=1: also print every SAM missile launched (tick, SAM unit, target unit, aim tile).
if (process.env.SAM_TRACE) {
  const init = SAMMissileExecution.prototype.init;
  SAMMissileExecution.prototype.init = function (mg: any, ticks: number) {
    realLog(`sam tick ${ticks} sam ${(this as any).ownerUnit.id()} target ${(this as any).target.id()} tile ${(this as any).targetTile}`);
    return init.call(this, mg, ticks);
  };
}

// DELETE_TRACE=<unit id>: print the stack that deletes that unit.
if (process.env.DELETE_TRACE) {
  const { UnitImpl } = await eng("packages/engine/src/game/UnitImpl.ts");
  const del = UnitImpl.prototype.delete;
  UnitImpl.prototype.delete = function (...args: any[]) {
    if (this.id() === Number(process.env.DELETE_TRACE)) realLog(`delete ${this.id()} at ${this.mg.ticks()}
${new Error().stack}`);
    return del.apply(this, args);
  };
}

let tracing = false;
const nextInt = PseudoRandom.prototype.nextInt;
PseudoRandom.prototype.nextInt = function (min: number, max: number) {
  const v = nextInt.call(this, min, max);
  if (tracing) realLog(`nextInt(${min}, ${max}) = ${v}`);
  return v;
};
const tickNation = NationExecution.prototype.tick;
NationExecution.prototype.tick = function (ticks: number) {
  tracing = (all || ticks === tick) && this.player?.smallID() === small;
  if (tracing && all) realLog(`tick ${ticks}`);
  try {
    return tickNation.call(this, ticks);
  } finally {
    tracing = false;
  }
};

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
const turns = decompressGameRecord(record).turns;
const last = all ? Number(process.argv[5] ?? turns.length - 1) : tick;
for (let i = 0; i <= last; i++) {
  runner.addTurn(turns[i]);
  runner.executeNextTick();
}
