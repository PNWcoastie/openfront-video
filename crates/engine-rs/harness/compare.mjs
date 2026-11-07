// Compares the TypeScript engine's per-tick state (dump-ts.mts) with the Rust
// port's (engine-rs) and reports the first tick where they differ, and how.
//
//   node compare.mjs <ts.jsonl> <rust.jsonl>
import fs from "node:fs";
import readline from "node:readline";

const [tsFile, rsFile] = process.argv.slice(2);
// Streamed line by line: long games dump more than a string can hold.
const lines = (f) => readline.createInterface({ input: fs.createReadStream(f) })[Symbol.asyncIterator]();
const tsLines = lines(tsFile);
const rsLines = lines(rsFile);
// Long values (exec lists) are cut to their first difference.
const show = (v) => { const t = JSON.stringify(v); return t && t.length > 160 ? t.slice(0, 160) + "…" : t; };
const cut = (a, b) => { const x = JSON.stringify(a) ?? "", y = JSON.stringify(b) ?? ""; let i = 0; while (i < x.length && x[i] === y[i]) i++; const from = Math.max(0, i - 60); return [x.slice(from, from + 160), y.slice(from, from + 160)].map((t) => (from > 0 ? "…" : "") + t); };
const PLAYER_FIELDS = [
  "smallID", "id", "name", "type", "team", "troops", "gold",
  "tiles", "tilesOrder", "borders", "bordersOrder", "spawnTile", "disconnected",
  "attacks", "incoming", "relations", "units", "alliances", "traitorTick", "embargoes",
];

let n = 0;
let first = null;
let last = null;
for (;;) {
  const [x, y] = await Promise.all([tsLines.next(), rsLines.next()]);
  if (x.done || y.done || !x.value.trim() || !y.value.trim()) {
    if (!x.done || !y.done) console.log("(one file has more lines than the other)");
    break;
  }
  const a = JSON.parse(x.value);
  const b = JSON.parse(y.value);
  const i = n++;
  first ??= a.t;
  last = a.t;
  const diffs = [];
  for (const k of ["t", "spawnPhase", "hash", "owners", "execs", "pending", "requests"]) {
    if (a[k] !== b[k]) { const [x, y] = cut(a[k], b[k]); diffs.push(`${k}: ts=${x} rust=${y}`); }
  }
  if (a.players.length !== b.players.length) {
    diffs.push(`players: ts has ${a.players.length}, rust has ${b.players.length}`);
  }
  for (let p = 0; p < Math.min(a.players.length, b.players.length); p++) {
    for (let f = 0; f < PLAYER_FIELDS.length; f++) {
      if (a.players[p][f] !== b.players[p][f]) {
        diffs.push(
          `player #${p} (${a.players[p][2]}) ${PLAYER_FIELDS[f]}: ` +
            (([x, y]) => `ts=${x} rust=${y}`)(cut(a.players[p][f], b.players[p][f])),
        );
      }
    }
  }
  if (diffs.length > 0) {
    console.log(`First difference after tick ${a.t} (line ${i + 1}):`);
    for (const d of diffs.slice(0, 20)) console.log(`  ${d}`);
    if (diffs.length > 20) console.log(`  ... and ${diffs.length - 20} more`);
    process.exit(1);
  }
}
console.log(`All ${n} lines match (ticks ${first} to ${last}).`);
