// Runs the real TypeScript engine on a game record and writes one JSON line
// per tick describing its full state, for the Rust port to be compared
// against (see compare.mjs). Run from the OpenFrontIO folder:
//
//   TSX_TSCONFIG_PATH=$PWD/tsconfig.json node --import tsx \
//     ../engine-rs/harness/dump-ts.mts <record.json> <ticks> <out.jsonl> [dumpOwnersAtTick]
//
// Line t describes the state after tick t ran ("t": -1 is the state before
// the first tick). Everything is integers or strings so both sides can
// produce byte-identical lines.
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const [recFile, ticksArg, outFile, dumpAtArg] = process.argv.slice(2);
const engineDir = process.cwd();
const maxTicks = Number(ticksArg);
const dumpAt = dumpAtArg === undefined ? null : Number(dumpAtArg);
// Synchronous writes: the tick loop never yields, so a write stream would
// only open (and flush) after the last tick.
const fd = fs.openSync(outFile, "w");
const out = { write: (s: string) => fs.writeSync(fd, s) };
console.log = () => {};
console.warn = () => {};
console.info = () => {};

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
const turns = decompressGameRecord(record).turns;
const game: any = runner.game;
const N = game.width() * game.height();

// FNV-1a over 32-bit words (each value XORed in whole, then multiplied).
const fnv = (values: Iterable<number>) => {
  let h = 2166136261;
  for (const v of values) h = Math.imul(h ^ v, 16777619) >>> 0;
  return h >>> 0;
};
const tileSetValues = (set: any): number[] => {
  const vals: number[] = [];
  set.forEach((t: number) => vals.push(t));
  return vals;
};
// Run-length encoded execution class names, e.g. "SpawnExecution*400".
const execList = (execs: any[]) => {
  const parts: string[] = [];
  let prev = "";
  let n = 0;
  for (const e of execs) {
    const name = e.constructor.name;
    if (name === prev) n++;
    else {
      if (n > 0) parts.push(n > 1 ? `${prev}*${n}` : prev);
      prev = name;
      n = 1;
    }
  }
  if (n > 0) parts.push(n > 1 ? `${prev}*${n}` : prev);
  return parts.join(",");
};

// Exact float64 bits, so both sides agree on every last bit.
const f64 = new Float64Array(1);
const u64 = new BigUint64Array(f64.buffer);
const bits = (x: number) => {
  f64[0] = x;
  return u64[0].toString(16);
};
const attacksOf = (p: any) =>
  p._outgoingAttacks
    .map((a: any) => `${a.id()}:${a.target().smallID()}:${bits(a.troops())}:${a.borderSize()}:${a.retreating() ? 1 : 0}`)
    .join(";");
const incomingOf = (p: any) => p._incomingAttacks.map((a: any) => a.id()).join(";");
const relationsOf = (p: any) => {
  const parts: string[] = [];
  p.relations.forEach((v: number, other: any) => parts.push(`${other.smallID()}:${bits(v)}`));
  return parts.join(";");
};
const unitsOf = (p: any) =>
  p._units
    .map(
      (u: any) =>
        `${u.id()}:${u.type()}:${u.tile()}:${bits(u.troops())}:${u.level()}:${u.isUnderConstruction() ? 1 : 0}:${u._deletionAt ?? -1}:${u.health()}`,
    ).join(";");
const alliancesOf = (p: any) =>
  p._alliances.map((a: any) => `${a.id()}:${a.other(p).smallID()}:${a.expiresAt()}`).join(";");
const embargoesOf = (p: any) => {
  const parts: string[] = [];
  p.embargoes.forEach((e: any) => parts.push(`${e.target.smallID()}:${e.createdAt}:${e.isTemporary ? 1 : 0}`));
  return parts.join(";");
};

const line = (t: number) => {
  const owners: number[] = new Array(N);
  for (let ref = 0; ref < N; ref++) owners[ref] = game.ownerID(ref) | (game.hasFallout(ref) ? 1 << 13 : 0);
  const players = game.allPlayers().map((p: any) => {
    const tiles = tileSetValues(p._tiles);
    const borders = tileSetValues(p._borderTiles);
    return [
      p.smallID(),
      p.id(),
      p.name(),
      p.type(),
      p.team() ?? "",
      String(p._troops),
      String(p._gold),
      tiles.length,
      fnv(tiles),
      borders.length,
      fnv(borders),
      p.spawnTile() ?? -1,
      p._isDisconnected ? 1 : 0,
      attacksOf(p),
      incomingOf(p),
      relationsOf(p),
      unitsOf(p),
      alliancesOf(p),
      p.markedTraitorTick,
      embargoesOf(p),
    ];
  });
  return JSON.stringify({
    t,
    spawnPhase: game.inSpawnPhase() ? 1 : 0,
    hash: game.hash(),
    owners: fnv(owners),
    execs: execList(game.execs),
    pending: execList(game.unInitExecs),
    requests: game.allianceRequests
      .map((r: any) => `${r.requestor().smallID()}>${r.recipient().smallID()}@${r.createdAt()}`)
      .join(";"),
    players,
  });
};

const dumpOwners = (t: number) => {
  const buf = new Uint16Array(N);
  for (let ref = 0; ref < N; ref++) buf[ref] = game.ownerID(ref) | (game.hasFallout(ref) ? 1 << 13 : 0);
  fs.writeFileSync(`${outFile}.owners-${t}.bin`, Buffer.from(buf.buffer));
};

out.write(line(-1) + "\n");
for (let i = 0; i < Math.min(maxTicks, turns.length); i++) {
  runner.addTurn(turns[i]);
  runner.executeNextTick();
  out.write(line(i) + "\n");
  if (dumpAt === i) dumpOwners(i);
}
fs.closeSync(fd);
process.stdout.write(`wrote ${Math.min(maxTicks, turns.length) + 1} lines to ${outFile}\n`);
