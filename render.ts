/**
 * openfront-render: turn an OpenFront.io replay into a sped-up MP4.
 *
 * It re-simulates the game with OpenFront's own deterministic engine (from a
 * local OpenFrontIO checkout) on the main thread. Every few ticks it sends
 * what changed to a video thread, which draws the frame and streams it into
 * ffmpeg while the simulation keeps going.
 *
 * Every simulated game is cached (tile changes, units and names per tick), so
 * rendering it again, with any length, size or quality, skips the engine and
 * draws the frames on several threads at once.
 *
 * Usage:
 *   node --import tsx render.ts <game link | game ID | record.json[.gz]> [options]
 *   (from the OpenFrontIO folder, where tsx is installed; npx tsx works too but starts slower)
 *
 * Options:
 *   --engine <dir>     OpenFrontIO checkout (default: $OPENFRONT_ENGINE or ./OpenFrontIO)
 *   --out <file>       output video (default: <gameID>.mp4)
 *   --seconds <n>      length of the timelapse, before the end hold (default: 30)
 *   --fps <n>          frames per second (default: 30)
 *   --width <n>        video width in pixels, height follows the map (default: 1280)
 *   --hold <n>         seconds to hold the final frame (default: 2)
 *   --crf <n>          x264 quality, lower is better and bigger (default: 23)
 *   --preset <name>    x264 preset (default: veryfast)
 *   --ffmpeg <path>    ffmpeg binary (default: $FFMPEG or "ffmpeg")
 *   --strict           stop if the replay desyncs from the archived hashes
 *   --cache-dir <dir>  where simulated games are kept (default: .cache next to this script)
 *   --no-cache         don't read or write the cache
 *   --resim            simulate again even if the game is cached
 *   --threads <n>      drawing threads (cached games: default a third of the CPU cores, at most 6;
 *                      --rust: default 2)
 *   --meta <file>      also write the game's and video's details as JSON (as openfront-video does)
 *   --thumbnail <file> also save the last frame as a PNG
 *   --rust <exe>       simulate with engine-rs (the Rust port, built with cargo build --release)
 *                      instead of the TypeScript engine; doesn't read or write the cache
 */
import {
  type Canvas,
  createCanvas,
  GlobalFonts,
  loadImage,
  type SKRSContext2D,
} from "@napi-rs/canvas";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { isMainThread, parentPort, threadId, Worker, workerData } from "node:worker_threads";
import zlib from "node:zlib";

// ── Output helpers (the engine's own console logging is muted) ──

const realLog = console.log.bind(console);
const realErr = console.error.bind(console);
const isTTY = Boolean(process.stdout.isTTY);
const say = (msg: string) => realLog(msg);
const warn = (msg: string) => realErr(`! ${msg}`);
const fmt = (n: number) => Math.round(n).toLocaleString("en-US");
const secs = (ms: number) => `${(ms / 1000).toFixed(2)} s`;
const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

function muteEngineLogs() {
  const noop = () => {};
  console.log = noop;
  console.info = noop;
  console.debug = noop;
  console.warn = noop;
}

// ── CLI ──

interface Options {
  input: string;
  engine: string;
  out?: string;
  seconds: number;
  fps: number;
  width: number;
  hold: number;
  crf: number;
  preset: string;
  ffmpeg: string;
  strict: boolean;
  cacheDir: string;
  noCache: boolean;
  resim: boolean;
  threads: number;
  /** Whether --threads was given. */
  threadsSet?: boolean;
  rust?: string;
  meta?: string;
  thumbnail?: string;
}

function parseArgs(argv: string[]): Options {
  const opts: Partial<Options> = {
    engine: process.env.OPENFRONT_ENGINE ?? "./OpenFrontIO",
    seconds: 30,
    fps: 30,
    width: 1280,
    hold: 2,
    crf: 23,
    preset: "veryfast",
    ffmpeg: process.env.FFMPEG ?? "ffmpeg",
    strict: false,
    cacheDir: path.join(path.dirname(fileURLToPath(import.meta.url)), ".cache"),
    noCache: false,
    resim: false,
    // x264 uses many cores too, so a third of them draw (more only competes).
    threads: Math.max(1, Math.min(6, Math.round(os.availableParallelism() / 3))),
  };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) throw new Error(`missing value for ${arg}`);
      return v;
    };
    const num = () => {
      const v = Number(next());
      if (!Number.isFinite(v) || v < 0) throw new Error(`bad number for ${arg}`);
      return v;
    };
    switch (arg) {
      case "--engine": opts.engine = next(); break;
      case "--out": opts.out = next(); break;
      case "--seconds": opts.seconds = num(); break;
      case "--fps": opts.fps = num(); break;
      case "--width": opts.width = num(); break;
      case "--hold": opts.hold = num(); break;
      case "--crf": opts.crf = num(); break;
      case "--preset": opts.preset = next(); break;
      case "--ffmpeg": opts.ffmpeg = next(); break;
      case "--strict": opts.strict = true; break;
      case "--cache-dir": opts.cacheDir = next(); break;
      case "--no-cache": opts.noCache = true; break;
      case "--resim": opts.resim = true; break;
      case "--threads": opts.threads = Math.max(1, Math.round(num())); opts.threadsSet = true; break;
      case "--rust": opts.rust = next(); break;
      case "--meta": opts.meta = next(); break;
      case "--thumbnail": opts.thumbnail = next(); break;
      case "-h":
      case "--help":
        say(fs.readFileSync(new URL(import.meta.url), "utf8").split("*/")[0]);
        process.exit(0);
      default:
        if (arg.startsWith("--")) throw new Error(`unknown option ${arg}`);
        if (opts.input) throw new Error(`unexpected argument ${arg}`);
        opts.input = arg;
    }
  }
  if (!opts.input) {
    throw new Error("give a game link, game ID or record file (see --help)");
  }
  return opts as Options;
}

// ── Loading the game record ──

function gameIdFrom(input: string): string | null {
  const fromLink = input.match(/\/game\/([A-Za-z0-9_-]+)/);
  if (fromLink) return fromLink[1];
  if (/^[A-Za-z0-9_-]{4,32}$/.test(input)) return input;
  return null;
}

async function loadRecord(input: string): Promise<any> {
  if (fs.existsSync(input)) {
    let buf = fs.readFileSync(input);
    if (buf[0] === 0x1f && buf[1] === 0x8b) buf = zlib.gunzipSync(buf);
    return JSON.parse(buf.toString("utf8"));
  }
  const id = gameIdFrom(input);
  if (!id) throw new Error(`can't find a game ID in "${input}"`);
  // The documented public API (docs/API.md in OpenFrontIO). The game client's
  // own /game/:id endpoint sits behind a Cloudflare bot check.
  const url = `https://api.openfront.io/public/game/${id}`;
  const res = await fetch(url, { headers: { Accept: "application/json" } });
  if (res.status === 404) throw new Error(`game ${id} not found (${url})`);
  const isJson = res.headers.get("content-type")?.includes("json");
  if (!isJson && (res.status === 403 || res.status === 503)) {
    throw new Error(
      `${url} answered with a Cloudflare bot check (HTTP ${res.status}).\n` +
        `  Open https://api.openfront.io/game/${id} in your browser, save the page as ` +
        `${id}.json and pass the file instead.`,
    );
  }
  if (!res.ok) throw new Error(`fetching ${url} failed: HTTP ${res.status}`);
  if (!isJson) throw new Error(`${url} didn't return JSON (got ${res.headers.get("content-type")})`);
  return res.json();
}

function engineCommit(engineDir: string): string | null {
  try {
    return execFileSync("git", ["-C", engineDir, "rev-parse", "HEAD"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
  } catch {
    return null;
  }
}

// ── Colors (packed as little-endian RGBA in a Uint32) ──

const rgba = (r: number, g: number, b: number) =>
  (0xff000000 | (b << 16) | (g << 8) | r) >>> 0;
const channels = (c: number) => [c & 0xff, (c >> 8) & 0xff, (c >> 16) & 0xff];
// Hot (every recolored tile), so no array allocations.
const mix = (a: number, b: number, t: number) => {
  const ar = a & 0xff, ag = (a >> 8) & 0xff, ab = (a >> 16) & 0xff;
  const br = b & 0xff, bg = (b >> 8) & 0xff, bb = (b >> 16) & 0xff;
  return rgba(
    Math.round(ar + (br - ar) * t),
    Math.round(ag + (bg - ag) * t),
    Math.round(ab + (bb - ab) * t),
  );
};

function hsl(h: number, s: number, l: number): number {
  // Keep territory colors out of the blues (180-260) so they never look like water.
  h = ((((h % 360) + 360) % 360) * 280) / 360;
  if (h >= 180) h += 80;
  const k = (n: number) => (n + h / 30) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) =>
    Math.round(255 * (l - a * Math.max(-1, Math.min(k(n) - 3, 9 - k(n), 1))));
  return rgba(f(0), f(8), f(4));
}

function hashString(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
  return h >>> 0;
}

const css = (c: number) => {
  const [r, g, b] = channels(c);
  return `rgb(${r},${g},${b})`;
};
const fromColord = (c: any) => {
  const { r, g, b } = c.toRgb();
  return rgba(Math.round(r), Math.round(g), Math.round(b));
};
const saturate = (c: number, s: number) => {
  const [r, g, b] = channels(c);
  const luma = 0.299 * r + 0.587 * g + 0.114 * b;
  return mix(rgba(Math.round(luma), Math.round(luma), Math.round(luma)), c, s);
};
// HSV value scale, as the structure shader's darken().
const darkenValue = (c: number, v: number) => {
  const [r, g, b] = channels(c);
  return rgba(Math.round(r * v), Math.round(g * v), Math.round(b * v));
};

// Fallback palette for engines too old to have the client theme in src/client.
const OCEAN = rgba(62, 104, 156);
const SHALLOW = rgba(92, 138, 186);
const LAKE = rgba(84, 128, 176);
const PLAINS = rgba(178, 210, 132);
const HIGHLAND = rgba(198, 186, 140);
const MOUNTAIN = rgba(226, 224, 218);
const FALLOUT = rgba(120, 168, 70);
const BLACK = rgba(0, 0, 0);

// ── The game client's look (theme, terrain colors, render settings, sprites) ──

interface ClientLook {
  theme: any;
  resolvePlayerColors: (...a: any[]) => { territory: any; border: any };
  encodeTerrainTile: (tb: number, out: Uint8Array, off: number, colors?: any) => void;
  hexToRgb: (hex: string) => [number, number, number] | null;
  renderTroops?: (troops: number) => string;
  settings: any;
}

async function loadClientLook(
  eng: (p: string) => Promise<any>,
  engineDir: string,
): Promise<ClientLook | null> {
  try {
    const { SettingsTheme } = await eng("src/client/theme/ThemeProvider.ts");
    const { createThemeSettings } = await eng("src/client/render/gl/RenderSettings.ts");
    const { resolvePlayerColors } = await eng("src/client/view/PlayerColors.ts");
    const { encodeTerrainTile, hexToRgb } = await eng("src/client/render/gl/utils/ColorUtils.ts");
    const { renderTroops } = await eng("packages/engine-lib/src/Format.ts").catch(() => ({}));
    const settings = JSON.parse(
      fs.readFileSync(path.join(engineDir, "src/client/render/gl/render-settings.json"), "utf8"),
    );
    const theme = new SettingsTheme(createThemeSettings("default"));
    return { theme, resolvePlayerColors, encodeTerrainTile, hexToRgb, renderTroops, settings };
  } catch (e) {
    warn(`couldn't load the game client's colors, using a simple palette (${errorText(e)})`);
    return null;
  }
}

function formatTroops(troops: number): string {
  // Same as the engine's renderTroops: troops are stored x10.
  const n = Math.max(0, troops / 10);
  if (n >= 1e6) return `${(n / 1e6).toFixed(n >= 1e7 ? 1 : 2).replace(/\.?0+$/, "")}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(n >= 1e4 ? 1 : 2).replace(/\.?0+$/, "")}K`;
  return String(Math.floor(n));
}

/** A sprite atlas as raw RGBA, with its cell size. */
async function loadAtlas(file: string) {
  const img = await loadImage(fs.readFileSync(file));
  const c = createCanvas(img.width, img.height);
  const ctx = c.getContext("2d");
  ctx.drawImage(img, 0, 0);
  return { img, data: ctx.getImageData(0, 0, img.width, img.height), w: img.width, h: img.height };
}

// Structure atlas columns, shapes and sizes (StructurePass + structure.frag).
const STRUCTURES = ["City", "Port", "Factory", "Defense Post", "SAM Launcher", "Missile Silo"];
const STRUCTURE_SIDES = [0, 5, 6, 8, 4, 3]; // 0 = circle
const STRUCTURE_ROT = [0, Math.PI / 2, Math.PI / 6, 0, 0, Math.PI / 2];
// Unit atlas columns (UnitPass).
const UNIT_COLS: Record<string, number> = {
  Transport: 0,
  "Trade Ship": 1,
  Warship: 2,
  "Atom Bomb": 3,
  "Hydrogen Bomb": 4,
  MIRV: 5,
  SAMMissile: 6,
  Shell: 7,
  "MIRV Warhead": 8,
};
const UNIT_CELL = 13;

function polygonPath(ctx: SKRSContext2D, cx: number, cy: number, r: number, sides: number, rot: number) {
  ctx.beginPath();
  if (sides === 0) {
    ctx.arc(cx, cy, r, 0, Math.PI * 2);
    return;
  }
  // As structure.frag's sdPolygon, in screen space (y down): corners at r,
  // edge midpoints at angles rot + k * 2pi/n.
  for (let i = 0; i < sides; i++) {
    const a = rot + (Math.PI * (2 * i + 1)) / sides;
    const px = cx + r * Math.cos(a);
    const py = cy + r * Math.sin(a);
    if (i === 0) ctx.moveTo(px, py);
    else ctx.lineTo(px, py);
  }
  ctx.closePath();
}

const BAR = "rgb(255,255,255)";
const BAR_BG = "rgb(30,30,30)";

// ── Frame data shared by the threads ──

/** A player's colors, worked out on the main thread (it has the theme). */
interface Style {
  fill: number; // territory color
  border: number;
  tileFill: number; // territory color as drawn over terrain (desaturated)
  structFill: string;
  structBorder: string;
  nameShade: string;
  outline: string;
}

/** Everything a frame renderer needs besides the per-frame data. */
interface RenderInit {
  W: number;
  H: number;
  outW: number;
  outH: number;
  terrain: Uint32Array;
  resourcesDir: string;
  look: RenderLook;
}

/** Drawing settings taken from the client's render-settings.json (cached with the game). */
interface RenderLook {
  territoryAlpha: number;
  // Fallout ground (territory.frag's stale nuke), or null for the fallback look.
  staleNuke: { rgb: number[]; base: number; variation: number } | null;
  shapes: Record<string, { scale: number; iconFill: number }>;
  name: { nameScaleFactor: number; nameScaleCap: number; troopSizeMultiplier: number; outlineWidth: number };
}

interface NameLabel {
  owner: number;
  name: string;
  troops: string;
  x: number;
  y: number;
  size: number;
}

/** What changed since the last frame (main thread -> live video thread). */
interface FrameMsg {
  type: "frame";
  progress: number;
  /** Every tile's owner and fallout (first frame only). */
  full?: { owner: Uint16Array; fallout: Uint8Array };
  /** Changed tiles as [ref, owner | fallout << 16] pairs. */
  tiles: Uint32Array;
  /** Active units as [code, tile, owner, underConstruction] quads (see STRUCTURE_CODE). */
  units: Int32Array;
  names: NameLabel[];
  /** Players styled since the last frame. */
  styles: [number, Style][];
}

type ToVideo = FrameMsg | { type: "end" };
type FromVideo =
  | { type: "done" }
  | { type: "finished"; code: number; ffErr: string; drawMs: number; ffWaitMs: number; finishMs: number }
  | { type: "error"; message: string };

// Unit codes in the units array: unit atlas columns 0-11, structures 16-21.
const STRUCTURE_CODE = 16;
const isMissileCode = (code: number) => code >= 3 && code <= 8;

/**
 * Names this small aren't drawn (the renderer checks em < 6 video units, which
 * doesn't depend on the video width), so they aren't cached either. Slightly
 * looser than the renderer so rounding never drops one it would draw.
 */
function nameIsReadable(size: number, ns: RenderLook["name"], W: number): boolean {
  const base = Math.max(1, Math.floor(size));
  const nameSize = Math.max(4, Math.floor(base * ns.nameScaleFactor));
  const nameScale = Math.min(base * 0.25, ns.nameScaleCap);
  return (nameSize * nameScale * 1280) / W >= 5.9;
}

// ── ffmpeg ──

function startFfmpeg(o: {
  ffmpeg: string; out: string; outW: number; outH: number; fps: number; crf: number; preset: string;
}) {
  const ff = spawn(
    o.ffmpeg,
    [
      "-hide_banner", "-loglevel", "error", "-y",
      "-f", "rawvideo", "-pix_fmt", "rgba", "-s", `${o.outW}x${o.outH}`,
      "-r", String(o.fps), "-i", "-",
      "-c:v", "libx264", "-preset", o.preset, "-crf", String(o.crf),
      "-pix_fmt", "yuv420p", "-movflags", "+faststart",
      o.out,
    ],
    { stdio: ["pipe", "ignore", "pipe"] },
  );
  let err = "";
  let exited = false;
  ff.stderr.on("data", (d) => (err += d));
  const done = new Promise<number>((resolve, reject) => {
    ff.on("error", (e) => reject(new Error(`couldn't start ffmpeg (${o.ffmpeg}): ${e.message}`)));
    ff.on("close", (code) => {
      exited = true;
      resolve(code ?? 1);
    });
  });
  done.catch(() => {}); // callers see it through write/end
  ff.stdin.on("error", () => {}); // reported via the exit code instead
  const early = async () => {
    const code = await done.catch(() => 1);
    return new Error(`ffmpeg stopped early (exit code ${code})\n${err.trim()}`);
  };
  return {
    done,
    stderr: () => err.trim(),
    /** Writes a frame; resolves with the time spent waiting for ffmpeg. */
    async write(chunk: Uint8Array): Promise<number> {
      if (exited) throw await early();
      if (ff.stdin.write(chunk)) return 0;
      const w0 = performance.now();
      try {
        await Promise.race([once(ff.stdin, "drain"), done]);
      } catch {
        throw await early();
      }
      if (exited) throw await early();
      return performance.now() - w0;
    },
    end(): Promise<number> {
      ff.stdin.end();
      return done;
    },
  };
}

// ── Frame renderer: the map, structures, units and names on a canvas ──

async function createFrameRenderer(init: RenderInit) {
  const { W, H, outW, outH, terrain, look } = init;
  const N = W * H;

  const owner = new Uint16Array(N);
  const fallout = new Uint8Array(N);
  const map32 = new Uint32Array(terrain);
  const styles = new Map<number, Style>();
  const GRAY: Style = {
    fill: rgba(128, 128, 128), border: rgba(64, 64, 64), tileFill: rgba(128, 128, 128),
    structFill: "rgb(128,128,128)", structBorder: "rgb(64,64,64)", nameShade: "rgb(0,0,0)", outline: "rgb(128,128,128)",
  };
  const styleOf = (id: number) => styles.get(id) ?? GRAY;

  const alpha = look.territoryAlpha;
  const stale = look.staleNuke;
  const tileColor = (ref: number): number => {
    const o = owner[ref];
    const f = fallout[ref] !== 0;
    if (f && stale) {
      // As territory.frag: any fallout tile, owned or not, is flat green
      // ground brightened by a little per-tile noise.
      const n = (stale.base + (((ref * 2654435761) >>> 24) / 255) * stale.variation) * 255;
      return rgba(
        Math.min(255, Math.round(stale.rgb[0] + n)),
        Math.min(255, Math.round(stale.rgb[1] + n)),
        Math.min(255, Math.round(stale.rgb[2] + n)),
      );
    }
    if (o === 0) return f ? mix(terrain[ref], FALLOUT, 0.6) : terrain[ref];
    const x = ref % W;
    const border =
      (x > 0 && owner[ref - 1] !== o) ||
      (x < W - 1 && owner[ref + 1] !== o) ||
      (ref >= W && owner[ref - W] !== o) ||
      (ref < N - W && owner[ref + W] !== o);
    const s = styleOf(o);
    if (border) return s.border;
    return mix(terrain[ref], s.tileFill, alpha);
  };

  // Changed tiles plus their neighbors (borders move) get recolored at the next frame.
  const stamp = new Uint32Array(N);
  let stampGen = 1;
  const touched: number[] = [];
  const touch = (ref: number) => {
    if (ref < 0 || ref >= N || stamp[ref] === stampGen) return;
    stamp[ref] = stampGen;
    touched.push(ref);
  };
  const recolor = () => {
    for (const ref of touched) {
      map32[ref] = tileColor(ref);
      markTile(ref);
    }
    touched.length = 0;
    stampGen++;
  };

  // Map tiles -> video pixels.
  const src = new Int32Array(outW * outH);
  for (let y = 0; y < outH; y++) {
    const sy = Math.min(H - 1, Math.floor(((y + 0.5) * H) / outH));
    for (let x = 0; x < outW; x++) {
      const sx = Math.min(W - 1, Math.floor(((x + 0.5) * W) / outW));
      src[y * outW + x] = sy * W + sx;
    }
  }
  const canvas: Canvas = createCanvas(outW, outH);
  const ctx = canvas.getContext("2d");
  const mapImage = ctx.createImageData(outW, outH);
  const md = mapImage.data;
  const frame32 = new Uint32Array(md.buffer, md.byteOffset, md.byteLength / 4);

  // Most of the map is the same from one frame to the next, so the scaled map
  // and the canvas are only redone in BLOCK x BLOCK pixel blocks where tiles
  // changed or the previous frame drew something on top.
  const BLOCK = 32;
  const bxN = Math.ceil(outW / BLOCK);
  const byN = Math.ceil(outH / BLOCK);
  const mapDirty = new Uint8Array(bxN * byN); // tiles changed under it
  let drawnOver = new Uint8Array(bxN * byN); // overlays from the last frame
  let fullMapPending = true;
  const markRect = (blocks: Uint8Array, x0: number, y0: number, x1: number, y1: number) => {
    const bx0 = Math.max(0, Math.floor(x0 / BLOCK));
    const by0 = Math.max(0, Math.floor(y0 / BLOCK));
    const bx1 = Math.min(bxN - 1, Math.floor(x1 / BLOCK));
    const by1 = Math.min(byN - 1, Math.floor(y1 / BLOCK));
    for (let by = by0; by <= by1; by++) for (let bx = bx0; bx <= bx1; bx++) blocks[by * bxN + bx] = 1;
  };
  // The video pixels each map column and row shows (-1: none).
  const colLo = new Int32Array(W).fill(-1);
  const colHi = new Int32Array(W).fill(-1);
  const rowLo = new Int32Array(H).fill(-1);
  const rowHi = new Int32Array(H).fill(-1);
  for (let x = 0; x < outW; x++) {
    const sx = src[x] % W;
    if (colLo[sx] < 0) colLo[sx] = x;
    colHi[sx] = x;
  }
  for (let y = 0; y < outH; y++) {
    const sy = Math.floor(src[y * outW] / W);
    if (rowLo[sy] < 0) rowLo[sy] = y;
    rowHi[sy] = y;
  }
  // Called for every recolored tile.
  const markTile = (ref: number) => {
    const tx = ref % W;
    const ty = (ref - tx) / W;
    if (colLo[tx] < 0 || rowLo[ty] < 0) return;
    markRect(mapDirty, colLo[tx], rowLo[ty], colHi[tx], rowHi[ty]);
  };
  // Scratch images for putting runs of blocks, by size.
  const partImages = new Map<number, { img: ImageData; px: Uint32Array }>();
  /** Rebuilds the scaled map and the canvas where needed, before overlays are drawn. */
  const restoreMap = () => {
    if (fullMapPending) {
      for (let i = 0; i < frame32.length; i++) frame32[i] = map32[src[i]];
      ctx.putImageData(mapImage, 0, 0);
      fullMapPending = false;
      mapDirty.fill(0);
      drawnOver.fill(0);
      return;
    }
    for (let by = 0; by < byN; by++) {
      const y0 = by * BLOCK;
      const y1 = Math.min(outH, y0 + BLOCK);
      let run = -1; // first block of the current run of blocks to put
      for (let bx = 0; bx <= bxN; bx++) {
        const b = by * bxN + bx;
        const put = bx < bxN && (mapDirty[b] !== 0 || drawnOver[b] !== 0);
        if (bx < bxN && mapDirty[b]) {
          const x0 = bx * BLOCK;
          const x1 = Math.min(outW, x0 + BLOCK);
          for (let y = y0; y < y1; y++) {
            const row = y * outW;
            for (let x = x0; x < x1; x++) frame32[row + x] = map32[src[row + x]];
          }
        }
        if (put && run < 0) run = bx;
        if (!put && run >= 0) {
          // putImageData with a dirty rectangle costs as much as a full put
          // here, so the run goes through a small image of its own.
          const x0 = run * BLOCK;
          const w = Math.min(outW, bx * BLOCK) - x0;
          const h = y1 - y0;
          const key = w * 65536 + h;
          let part = partImages.get(key);
          if (!part) {
            const img = ctx.createImageData(w, h);
            part = { img, px: new Uint32Array(img.data.buffer, img.data.byteOffset, w * h) };
            partImages.set(key, part);
          }
          for (let y = 0; y < h; y++) {
            const from = (y0 + y) * outW + x0;
            part.px.set(frame32.subarray(from, from + w), y * w);
          }
          ctx.putImageData(part.img, x0, y0);
          run = -1;
        }
      }
    }
    mapDirty.fill(0);
    drawnOver.fill(0);
  };
  const barH = Math.max(3, Math.round(outH / 180));

  // A size unit that follows the video width (icons and sprites would be
  // invisible at true map scale).
  const scale = outW / W;
  const u = outW / 1280;
  const toX = (ref: number) => ((ref % W) + 0.5) * scale;
  const toY = (ref: number) => (Math.floor(ref / W) + 0.5) * scale;

  // Sprites and font from the client's resources.
  const res = (p: string) => path.join(init.resourcesDir, p);
  const iconAtlas = fs.existsSync(res("atlases/icon-atlas.png"))
    ? await loadAtlas(res("atlases/icon-atlas.png"))
    : null;
  const unitAtlas = fs.existsSync(res("atlases/unit-atlas.png"))
    ? await loadAtlas(res("atlases/unit-atlas.png"))
    : null;
  let font = "sans-serif";
  if (fs.existsSync(res("fonts/overpass-bold.woff"))) {
    GlobalFonts.registerFromPath(res("fonts/overpass-bold.woff"), "Overpass");
    font = "Overpass";
  }

  // Unit sprites colored per owner with the unit shader's gray bands:
  // light -> territory, mid -> halfway, center -> territory, dark -> border.
  const unitSprites = new Map<number, Canvas>();
  const unitSprite = (col: number, o: number): Canvas | null => {
    if (!unitAtlas) return null;
    const key = o * 16 + col;
    let c = unitSprites.get(key);
    if (c) return c;
    const s = styleOf(o);
    const mid = mix(s.fill, s.border, 0.5);
    c = createCanvas(UNIT_CELL, UNIT_CELL);
    const cx = c.getContext("2d");
    const img = cx.createImageData(UNIT_CELL, UNIT_CELL);
    const src8 = unitAtlas.data.data;
    for (let y = 0; y < UNIT_CELL; y++) {
      for (let x = 0; x < UNIT_CELL; x++) {
        const si = (y * unitAtlas.w + col * UNIT_CELL + x) * 4;
        const a = src8[si + 3];
        if (a === 0) continue;
        const gray = src8[si] / 255;
        const color = gray > 0.6 ? s.fill : gray > 0.45 ? mid : gray > 0.34 ? s.fill : s.border;
        const [r, g, b] = channels(color);
        const di = (y * UNIT_CELL + x) * 4;
        img.data[di] = r;
        img.data[di + 1] = g;
        img.data[di + 2] = b;
        img.data[di + 3] = a;
      }
    }
    cx.putImageData(img, 0, 0);
    unitSprites.set(key, c);
    return c;
  };
  const spriteScale = Math.max(scale, 1.4 * u);

  // Structures are drawn once per type, owner and build state, then copied
  // into each frame at whole pixels (shapes and icons are slow to raster).
  const structureRadius = 6.5 * u;
  const structureSprites = new Map<number, Canvas>();
  const structureSprite = (col: number, o: number, building: boolean): Canvas => {
    const key = (o * 8 + col) * 2 + (building ? 1 : 0);
    let c = structureSprites.get(key);
    if (c) return c;
    const cfg = look.shapes[STRUCTURES[col]] ?? { scale: 1, iconFill: 0.85 };
    const r = structureRadius * cfg.scale;
    const size = Math.ceil(2 * r) + 2;
    c = createCanvas(size, size);
    const sc = c.getContext("2d");
    const mid = size / 2;
    const s = styleOf(o);
    polygonPath(sc, mid, mid, r, STRUCTURE_SIDES[col], STRUCTURE_ROT[col]);
    sc.fillStyle = building ? "rgb(127,127,127)" : s.structBorder;
    sc.fill();
    // Border band: 0.06 of the 0.45 shape radius in structure.frag.
    polygonPath(sc, mid, mid, r * (1 - 0.06 / 0.45 / cfg.scale), STRUCTURE_SIDES[col], STRUCTURE_ROT[col]);
    sc.fillStyle = building ? "rgb(198,198,198)" : s.structFill;
    sc.fill();
    if (iconAtlas) {
      const cell = iconAtlas.h;
      const icon = 2 * r * cfg.iconFill * 0.75;
      sc.drawImage(iconAtlas.img, col * cell, 0, cell, cell, mid - icon / 2, mid - icon / 2, icon, icon);
    }
    structureSprites.set(key, c);
    return c;
  };
  const drawStructure = (col: number, tile: number, o: number, building: boolean) => {
    const sprite = structureSprite(col, o, building);
    const half = sprite.width / 2;
    const x = Math.round(toX(tile) - half);
    const y = Math.round(toY(tile) - half);
    ctx.drawImage(sprite, x, y);
    markRect(drawnOver, x - 1, y - 1, x + sprite.width + 1, y + sprite.height + 1);
  };
  const drawUnit = (col: number, tile: number, o: number) => {
    const sprite = unitSprite(col, o);
    const size = UNIT_CELL * spriteScale;
    const x = toX(tile);
    const y = toY(tile);
    if (sprite) {
      ctx.drawImage(sprite, x - size / 2, y - size / 2, size, size);
    } else {
      ctx.fillStyle = css(styleOf(o).fill);
      ctx.fillRect(x - 2 * u, y - 2 * u, 4 * u, 4 * u);
    }
    const r = Math.max(size / 2, 2 * u) + 2;
    markRect(drawnOver, x - r, y - r, x + r, y + r);
  };

  // Names: the engine's placement, sized like the client's name pass.
  const ns = look.name;
  const drawNames = (names: NameLabel[]) => {
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.lineJoin = "round";
    for (const n of names) {
      const base = Math.max(1, Math.floor(n.size));
      const nameSize = Math.max(4, Math.floor(base * ns.nameScaleFactor));
      const nameScale = Math.min(base * 0.25, ns.nameScaleCap);
      const em = nameSize * nameScale * scale;
      if (em < 6 * u) continue; // too small to read
      const s = styleOf(n.owner);
      const y = n.y * scale;
      // The troop line sits 1.1 baselines below the name (name.vert.glsl);
      // the font's baseline is 36/48 of its size.
      const lines: [string, number, number][] = [
        [n.name, y, em],
        [n.troops, y + em * 0.75 * 1.1, em * ns.troopSizeMultiplier],
      ];
      for (const [text, ly, size] of lines) {
        ctx.font = `${size.toFixed(1)}px ${font}`;
        // Keep the text inside the video; the game would let you pan to it.
        const half = ctx.measureText(text).width / 2 + 2 * u;
        const x = Math.min(Math.max(n.x * scale, half), Math.max(half, outW - half));
        ctx.lineWidth = Math.max(1.5 * u, size * 0.12) * (ns.outlineWidth / 1.4);
        ctx.strokeStyle = s.outline;
        ctx.strokeText(text, x, ly);
        // Generous: glyphs, outline and antialiasing all fit inside.
        const pad = ctx.lineWidth + size * 0.5 + 2;
        markRect(drawnOver, x - half - pad, ly - size - pad, x + half + pad, ly + size + pad);
        ctx.fillStyle = s.nameShade;
        ctx.fillText(text, x, ly);
      }
    }
  };

  const drawOverlays = (units: Int32Array, names: NameLabel[]) => {
    // Boats and trains, then structures, then missiles on top (as UnitPass).
    for (let pass = 0; pass < 3; pass++) {
      for (let i = 0; i < units.length; i += 4) {
        const code = units[i];
        const structure = code >= STRUCTURE_CODE;
        const missile = !structure && isMissileCode(code);
        if (pass === 0 && !structure && !missile) drawUnit(code, units[i + 1], units[i + 2]);
        if (pass === 1 && structure) drawStructure(code - STRUCTURE_CODE, units[i + 1], units[i + 2], units[i + 3] !== 0);
        if (pass === 2 && missile) drawUnit(code, units[i + 1], units[i + 2]);
      }
    }
    step("units and structures");
    drawNames(names);
  };

  let drawMs = 0;
  // OPENFRONT_DRAW_PROFILE=1: time per drawing step, printed when the thread ends.
  const profile = process.env.OPENFRONT_DRAW_PROFILE ? new Map<string, number>() : null;
  let lap = 0;
  const step = (name: string) => {
    if (!profile) return;
    const now = performance.now();
    profile.set(name, (profile.get(name) ?? 0) + now - lap);
    lap = now;
  };
  return {
    setStyle(id: number, s: Style) {
      styles.set(id, s);
    },
    /** Every tile's state at once (the first frame). */
    setFull(o: Uint16Array, f: Uint8Array) {
      owner.set(o);
      fallout.set(f);
      for (let ref = 0; ref < N; ref++) map32[ref] = tileColor(ref);
      fullMapPending = true;
    },
    /** Changed tiles as [ref, owner | fallout << 16] pairs. */
    applyTiles(t: ArrayLike<number>) {
      for (let i = 0; i < t.length; i += 2) {
        const ref = t[i];
        owner[ref] = t[i + 1] & 0xffff;
        fallout[ref] = t[i + 1] >>> 16;
        touch(ref); touch(ref - 1); touch(ref + 1); touch(ref - W); touch(ref + W);
      }
    },
    /** Draws the current state; returns the frame as RGBA. */
    draw(units: Int32Array, names: NameLabel[], progress: number): Uint8Array {
      const d0 = performance.now();
      lap = d0;
      recolor();
      step("recolor tiles");
      restoreMap();
      step("map");
      drawOverlays(units, names);
      step("names");
      const filled = Math.round(outW * progress);
      ctx.fillStyle = BAR_BG;
      ctx.fillRect(0, outH - barH, outW, barH);
      ctx.fillStyle = BAR;
      ctx.fillRect(0, outH - barH, filled, barH);
      // Same bytes as getImageData (the frame is opaque), a little faster.
      const px = canvas.data();
      step("read pixels");
      drawMs += performance.now() - d0;
      return new Uint8Array(px.buffer, px.byteOffset, px.byteLength);
    },
    get drawMs() {
      return drawMs;
    },
    /** Prints the OPENFRONT_DRAW_PROFILE times, if enabled. */
    reportProfile() {
      if (!profile) return;
      const rows = [...profile].map(([k, v]) => `${k} ${v.toFixed(0)} ms`).join(", ");
      realErr(`draw profile (thread ${threadId}): ${rows}`);
    },
  };
}

// ── Live video thread: draws frames and feeds ffmpeg while the main thread simulates ──

interface LiveJob {
  mode: "live";
  init: RenderInit;
  ffmpeg: Parameters<typeof startFfmpeg>[0];
}

async function liveVideoThread(job: LiveJob) {
  const post = (m: FromVideo) => parentPort!.postMessage(m);
  const renderer = await createFrameRenderer(job.init);
  const ff = startFfmpeg(job.ffmpeg);
  let ffWaitMs = 0;

  // Frames are handled one at a time, in order.
  let queue = Promise.resolve();
  const fail = (e: unknown) => {
    post({ type: "error", message: errorText(e) });
    process.exit(1);
  };
  ff.done.catch(fail);
  parentPort!.on("message", (msg: ToVideo) => {
    queue = queue.then(async () => {
      if (msg.type === "frame") {
        for (const [id, s] of msg.styles) renderer.setStyle(id, s);
        if (msg.full) renderer.setFull(msg.full.owner, msg.full.fallout);
        renderer.applyTiles(msg.tiles);
        ffWaitMs += await ff.write(renderer.draw(msg.units, msg.names, msg.progress));
        post({ type: "done" });
      } else {
        const f0 = performance.now();
        const code = await ff.end();
        post({
          type: "finished", code, ffErr: ff.stderr(), drawMs: renderer.drawMs, ffWaitMs,
          finishMs: performance.now() - f0,
        });
        parentPort!.close();
      }
    }).catch(fail);
  });
}

// ── The cache: a simulated game's per-tick changes ──
//
// <cache-dir>/<gameID>-<engine commit>/
//   meta.json          what the renderer needs besides the ticks (written last)
//   terrain.bin.gz     terrain colors, one RGBA Uint32 per tile
//   start.bin.gz       tick 0: every tile's owner (u16) and fallout (u8), then units + names
//   ticks-NNNN.bin.gz  CHUNK_TICKS ticks each: per tick its number, changed tiles,
//                      units and names (see writeTick / readTick)
// All numbers little-endian; every section starts 4-byte aligned.

const CACHE_VERSION = 1;
const CHUNK_TICKS = 256;

interface CacheMeta {
  version: number;
  gameID: string;
  map: string;
  mode: string;
  players: number;
  durationSec: number;
  gitCommit: string | null;
  engineCommit: string | null;
  W: number;
  H: number;
  ticks: number;
  chunks: number;
  look: RenderLook;
  styles: [number, Style][];
  names: Record<number, string>;
  hashes: { checked: number; mismatches: number; first: number | null };
}

class ByteWriter {
  buf = new Uint8Array(1 << 20);
  view = new DataView(this.buf.buffer);
  len = 0;
  private ensure(n: number) {
    if (this.len + n <= this.buf.length) return;
    let size = this.buf.length * 2;
    while (size < this.len + n) size *= 2;
    const b = new Uint8Array(size);
    b.set(this.buf.subarray(0, this.len));
    this.buf = b;
    this.view = new DataView(b.buffer);
  }
  u8(v: number) { this.ensure(1); this.view.setUint8(this.len, v); this.len += 1; }
  u16(v: number) { this.ensure(2); this.view.setUint16(this.len, v, true); this.len += 2; }
  u32(v: number) { this.ensure(4); this.view.setUint32(this.len, v, true); this.len += 4; }
  f64(v: number) { this.ensure(8); this.view.setFloat64(this.len, v, true); this.len += 8; }
  u32s(a: ArrayLike<number>) {
    this.ensure(4 * a.length);
    for (let i = 0; i < a.length; i++) this.view.setUint32(this.len + 4 * i, a[i], true);
    this.len += 4 * a.length;
  }
  i32s(a: ArrayLike<number>) {
    this.ensure(4 * a.length);
    for (let i = 0; i < a.length; i++) this.view.setInt32(this.len + 4 * i, a[i], true);
    this.len += 4 * a.length;
  }
  bytes(b: Uint8Array) { this.ensure(b.length); this.buf.set(b, this.len); this.len += b.length; }
  pad4() { while (this.len % 4) this.u8(0); }
  take(): Uint8Array {
    const out = this.buf.slice(0, this.len);
    this.len = 0;
    return out;
  }
}

class ByteReader {
  buf: Uint8Array;
  view: DataView;
  off = 0;
  constructor(buf: Uint8Array) {
    // Typed-array views below need the data 4-byte aligned.
    this.buf = buf.byteOffset % 4 === 0 ? buf : buf.slice();
    this.view = new DataView(this.buf.buffer, this.buf.byteOffset, this.buf.byteLength);
  }
  more() { return this.off < this.buf.byteLength; }
  u8() { return this.view.getUint8(this.off++); }
  u16() { const v = this.view.getUint16(this.off, true); this.off += 2; return v; }
  u32() { const v = this.view.getUint32(this.off, true); this.off += 4; return v; }
  f64() { const v = this.view.getFloat64(this.off, true); this.off += 8; return v; }
  private at() { return this.buf.byteOffset + this.off; }
  u32s(n: number) { const a = new Uint32Array(this.buf.buffer, this.at(), n); this.off += 4 * n; return a; }
  i32s(n: number) { const a = new Int32Array(this.buf.buffer, this.at(), n); this.off += 4 * n; return a; }
  u16s(n: number) { const a = new Uint16Array(this.buf.buffer, this.at(), n); this.off += 2 * n; return a; }
  u8s(n: number) { const a = this.buf.subarray(this.off, this.off + n); this.off += n; return a; }
  skip(n: number) { this.off += n; }
  pad4() { this.off = (this.off + 3) & ~3; }
}

const utf8 = new TextEncoder();
const fromUtf8 = new TextDecoder();

function writeUnitsAndNames(w: ByteWriter, units: ArrayLike<number>, names: NameLabel[]) {
  w.u32(units.length / 4);
  w.i32s(units);
  // Names, with their byte length first so readers can skip them.
  const start = w.len;
  w.u32(0);
  w.u16(names.length);
  for (const n of names) {
    const troops = utf8.encode(n.troops);
    w.u16(n.owner);
    w.f64(n.x);
    w.f64(n.y);
    w.f64(n.size);
    w.u8(troops.length);
    w.bytes(troops);
  }
  w.pad4();
  w.view.setUint32(start, w.len - start - 4, true);
}

function readUnits(r: ByteReader): Int32Array {
  return r.i32s(4 * r.u32());
}

function readNames(r: ByteReader, names: Record<number, string>): NameLabel[] {
  const len = r.u32();
  const end = r.off + len;
  const out: NameLabel[] = [];
  for (let i = r.u16(); i > 0; i--) {
    const owner = r.u16();
    const x = r.f64();
    const y = r.f64();
    const size = r.f64();
    const troops = fromUtf8.decode(r.u8s(r.u8()));
    out.push({ owner, name: names[owner] ?? "", troops, x, y, size });
  }
  r.off = end;
  return out;
}

function skipUnitsAndNames(r: ByteReader) {
  r.skip(16 * r.u32());
  r.skip(r.u32());
}

function gzipAsync(data: Uint8Array): Promise<Buffer> {
  return new Promise((resolve, reject) =>
    zlib.gzip(data, { level: 1 }, (e, out) => (e ? reject(e) : resolve(out))),
  );
}

const readGz = (file: string) => new Uint8Array(zlib.gunzipSync(fs.readFileSync(file)));
const chunkFile = (dir: string, c: number) => path.join(dir, `ticks-${String(c).padStart(4, "0")}.bin.gz`);

/** Writes a simulated game into a temporary folder and moves it into place when complete. */
class CacheWriter {
  private tmp: string;
  private w = new ByteWriter();
  private chunk = 0;
  private writes: Promise<void>[] = [];

  constructor(private dir: string) {
    this.tmp = `${dir}.tmp-${process.pid}`;
    fs.rmSync(this.tmp, { recursive: true, force: true });
    fs.mkdirSync(this.tmp, { recursive: true });
  }

  private save(name: string, data: Uint8Array) {
    // Compressed on the libuv thread pool, so the simulation isn't held up.
    this.writes.push(gzipAsync(data).then((gz) => fs.promises.writeFile(path.join(this.tmp, name), gz)));
  }

  writeStart(owner: Uint16Array, fallout: Uint8Array, units: ArrayLike<number>, names: NameLabel[]) {
    const w = new ByteWriter();
    w.bytes(new Uint8Array(owner.buffer, owner.byteOffset, owner.byteLength));
    w.bytes(fallout);
    w.pad4();
    writeUnitsAndNames(w, units, names);
    this.save("start.bin.gz", w.take());
  }

  writeTick(tick: number, tiles: ArrayLike<number>, units: ArrayLike<number>, names: NameLabel[]) {
    const w = this.w;
    w.u32(tick);
    w.u32(tiles.length / 2);
    w.u32s(tiles);
    writeUnitsAndNames(w, units, names);
    if (tick % CHUNK_TICKS === 0) this.flush();
  }

  private flush() {
    if (this.w.len === 0) return;
    this.save(path.basename(chunkFile(this.tmp, this.chunk)), this.w.take());
    this.chunk++;
  }

  async finish(terrain: Uint32Array, meta: Omit<CacheMeta, "chunks">) {
    this.flush();
    this.save("terrain.bin.gz", new Uint8Array(terrain.buffer, terrain.byteOffset, terrain.byteLength));
    await Promise.all(this.writes);
    const full: CacheMeta = { ...meta, chunks: this.chunk };
    fs.writeFileSync(path.join(this.tmp, "meta.json"), JSON.stringify(full));
    fs.rmSync(this.dir, { recursive: true, force: true });
    fs.renameSync(this.tmp, this.dir);
  }

  discard() {
    fs.rmSync(this.tmp, { recursive: true, force: true });
  }
}

function readCacheMeta(dir: string): CacheMeta | null {
  try {
    const meta = JSON.parse(fs.readFileSync(path.join(dir, "meta.json"), "utf8")) as CacheMeta;
    return meta.version === CACHE_VERSION ? meta : null;
  } catch {
    return null;
  }
}

/**
 * The tick each frame shows, the same way the simulating render picks them:
 * frame i comes after the first tick that reaches i/(frames-1) of the game.
 * Hold frames repeat the last tick.
 */
function frameSchedule(totalTicks: number, frames: number, holdFrames: number) {
  const out: { tick: number; progress: number }[] = [{ tick: 0, progress: 0 }];
  for (let i = 1; i < frames; i++) {
    const tick = Math.min(totalTicks, Math.max(1, Math.ceil((i * totalTicks) / (frames - 1))));
    out.push({ tick, progress: tick / totalTicks });
  }
  for (let i = 0; i < holdFrames; i++) out.push({ tick: totalTicks, progress: 1 });
  return out;
}

// ── Cached render: drawing threads replay the cache, the main thread encodes ──

interface DrawJob {
  mode: "draw";
  init: RenderInit;
  dir: string;
  chunks: number;
  styles: [number, Style][];
  names: Record<number, string>;
  frames: { tick: number; progress: number }[];
  /** This thread draws frames me, me + count, me + 2 * count, ... */
  me: number;
  count: number;
}

type FromDraw =
  | { type: "frame"; index: number; data: Uint8Array }
  | { type: "stats"; drawMs: number; readMs: number }
  | { type: "error"; message: string };

// Frames a drawing thread may have waiting for the encoder.
const DRAW_AHEAD = 3;

async function drawThread(job: DrawJob) {
  const post = (m: FromDraw, transfer: ArrayBuffer[] = []) => parentPort!.postMessage(m, transfer);
  const renderer = await createFrameRenderer(job.init);
  for (const [id, s] of job.styles) renderer.setStyle(id, s);

  // The main thread acks each frame once it's encoded.
  let unacked = 0;
  let wake: (() => void) | null = null;
  parentPort!.on("message", () => {
    unacked--;
    wake?.();
  });

  const mine: number[] = [];
  for (let i = job.me; i < job.frames.length; i += job.count) mine.push(i);
  let next = 0; // index into mine
  let readMs = 0;

  const drawDue = async (tick: number, r: ByteReader) => {
    if (next >= mine.length || job.frames[mine[next]].tick !== tick) {
      skipUnitsAndNames(r);
      return;
    }
    const units = readUnits(r);
    const names = readNames(r, job.names);
    while (next < mine.length && job.frames[mine[next]].tick === tick) {
      const index = mine[next++];
      const px = renderer.draw(units, names, job.frames[index].progress);
      const data = px.slice(); // own buffer, so it can be transferred
      while (unacked >= DRAW_AHEAD) await new Promise<void>((resolve) => (wake = resolve));
      unacked++;
      post({ type: "frame", index, data }, [data.buffer]);
    }
  };

  const N = job.init.W * job.init.H;
  let r0 = performance.now();
  const start = new ByteReader(readGz(path.join(job.dir, "start.bin.gz")));
  const owner = start.u16s(N);
  const fallout = start.u8s(N);
  start.pad4();
  renderer.setFull(owner, fallout);
  readMs += performance.now() - r0;
  await drawDue(0, start);

  for (let c = 0; c < job.chunks && next < mine.length; c++) {
    r0 = performance.now();
    const r = new ByteReader(readGz(chunkFile(job.dir, c)));
    readMs += performance.now() - r0;
    while (r.more() && next < mine.length) {
      const tick = r.u32();
      renderer.applyTiles(r.u32s(2 * r.u32()));
      await drawDue(tick, r);
    }
  }
  if (next < mine.length) throw new Error(`the cache ends before tick ${job.frames[mine[next]].tick}`);
  post({ type: "stats", drawMs: renderer.drawMs, readMs });
}

async function renderFromCache(opts: Options, engineDir: string, dir: string, meta: CacheMeta, t0: number) {
  say(`Using the cached simulation (${dir})`);
  say(
    `  ${meta.gameID}: ${meta.map}, ${meta.mode}, ${meta.players} players, ` +
      `${fmt(meta.ticks)} ticks (${Math.round(meta.durationSec / 60)} min of play)`,
  );
  const l0 = performance.now();
  const terrainBytes = readGz(path.join(dir, "terrain.bin.gz"));
  const terrain = new Uint32Array(terrainBytes.buffer, terrainBytes.byteOffset, terrainBytes.byteLength / 4);
  const { W, H } = meta;
  const outW = Math.max(2, Math.round(opts.width / 2) * 2);
  const outH = Math.max(2, Math.round((H * outW) / W / 2) * 2);
  const frames = Math.max(1, Math.round(opts.seconds * opts.fps));
  const holdFrames = Math.round(opts.hold * opts.fps);
  const schedule = frameSchedule(meta.ticks, frames, holdFrames);
  const count = Math.min(opts.threads, schedule.length);
  const out = path.resolve(opts.out ?? `${meta.gameID}.mp4`);
  say(
    `Drawing ${schedule.length} frames on ${count} thread${count === 1 ? "" : "s"} ` +
      `(${opts.seconds}s at ${opts.fps} fps, ${outW}x${outH})`,
  );

  const ff = startFfmpeg({ ffmpeg: opts.ffmpeg, out, outW, outH, fps: opts.fps, crf: opts.crf, preset: opts.preset });
  const init: RenderInit = { W, H, outW, outH, terrain, resourcesDir: path.join(engineDir, "resources"), look: meta.look };
  const loadMs = performance.now() - l0;

  // Frames arrive out of order; they're written to ffmpeg in order.
  const ready = new Map<number, Uint8Array>();
  let written = 0;
  let ffWaitMs = 0;
  let drawMs = 0;
  let readMs = 0;
  let failure: Error | null = null;
  let wake: (() => void) | null = null;
  const workers: Worker[] = [];
  let statsLeft = count;
  for (let me = 0; me < count; me++) {
    const job: DrawJob = {
      mode: "draw", init, dir, chunks: meta.chunks, styles: meta.styles, names: meta.names,
      frames: schedule, me, count,
    };
    const w = new Worker(new URL(import.meta.url), { workerData: job });
    w.on("message", (m: FromDraw) => {
      if (m.type === "frame") ready.set(m.index, m.data);
      else if (m.type === "stats") {
        drawMs += m.drawMs;
        readMs += m.readMs;
        statsLeft--;
      } else failure ??= new Error(m.message);
      wake?.();
    });
    w.on("error", (e) => {
      failure ??= e;
      wake?.();
    });
    workers.push(w);
  }

  const d0 = performance.now();
  let lastPrint = 0;
  try {
    while (written < schedule.length) {
      if (failure) throw failure;
      const frame = ready.get(written);
      if (!frame) {
        await new Promise<void>((resolve) => (wake = resolve));
        continue;
      }
      ready.delete(written);
      ffWaitMs += await ff.write(frame);
      workers[written % count].postMessage("ack");
      written++;
      const now = performance.now();
      if (isTTY && now - lastPrint > 250) {
        lastPrint = now;
        process.stdout.write(`\r  frame ${written}/${schedule.length}  ${((now - d0) / 1000).toFixed(1)}s elapsed\x1b[K`);
      }
    }
    while (statsLeft > 0) {
      if (failure) throw failure;
      await new Promise<void>((resolve) => (wake = resolve));
    }
  } finally {
    if (isTTY) process.stdout.write("\n");
    await Promise.all(workers.map((w) => w.terminate()));
  }
  const drawWall = performance.now() - d0;
  const f0 = performance.now();
  const code = await ff.end();
  if (code !== 0) throw new Error(`ffmpeg exited with code ${code}\n${ff.stderr()}`);
  const finishMs = performance.now() - f0;
  const total = performance.now() - t0;

  const size = fs.statSync(out).size;
  say("");
  say(`Done: ${out}`);
  say(`  ${outW}x${outH}, ${(schedule.length / opts.fps).toFixed(1)}s of video, ${(size / 1024 / 1024).toFixed(1)} MB`);
  say("");
  printTimeline(
    t0,
    t0 + total,
    [
      ["Load cache", l0, l0 + loadMs],
      [`Draw + encode (${count} thread${count === 1 ? "" : "s"} + ffmpeg)`, d0, d0 + drawWall],
      ["Finish encoding", f0, f0 + finishMs],
    ],
    [
      "",
      `Drawing: ${secs(drawMs)} of work for ${written} frames ` +
        `(${(drawMs / Math.max(1, written)).toFixed(1)} ms each), reading the cache ${secs(readMs)}`,
      `ffmpeg held up writes for ${secs(ffWaitMs)}`,
    ],
  );
  say("");
  reportSpeed(total, meta.durationSec, ", from the cache");
  reportHashes(meta.hashes, " (from the cached simulation)");
}

/** A stage of the run for printTimeline: its name and when it ran (performance.now() times). */
type Stage = [name: string, start: number, end: number];

/**
 * Prints the run as a waterfall: one bar per stage, placed on a shared time axis
 * from `t0` to `t1`, so overlapping stages show up as overlapping bars. `notes`
 * follow underneath (busy time, waits).
 */
function printTimeline(t0: number, t1: number, stages: Stage[], notes: string[] = []) {
  const COLS = 48;
  const total = Math.max(1, t1 - t0);
  const label = Math.max(...stages.map(([n]) => n.length));
  const col = (t: number) => Math.min(COLS, Math.max(0, ((t - t0) / total) * COLS));
  const axisEnd = secs(total);
  say(`  ${"".padEnd(label)}  ${"".padStart(8)}  0 s${axisEnd.padStart(COLS - 3)}`);
  for (const [name, start, end] of stages) {
    const a = Math.floor(col(start));
    const b = Math.max(a + 1, Math.ceil(col(end)));
    const bar = " ".repeat(a) + "█".repeat(Math.min(COLS, b) - a);
    say(`  ${name.padEnd(label)}  ${secs(end - start).padStart(8)}  ${bar.padEnd(COLS)}|`);
  }
  for (const n of notes) say(`  ${n}`);
}

/** The headline metric: seconds of rendering per minute of gameplay (lower is better). */
function reportSpeed(totalMs: number, gameSec: number, note = "") {
  const minutes = gameSec / 60;
  say(
    `Speed: ${(totalMs / 1000 / minutes).toFixed(2)} s of rendering per minute of gameplay` +
      ` (${(totalMs / 1000).toFixed(1)} s for ${minutes.toFixed(1)} min${note})`,
  );
}

function reportHashes(h: CacheMeta["hashes"], note = "") {
  if (h.checked === 0) {
    say(`Replay check${note}: no archived hashes to compare against`);
  } else if (h.mismatches === 0) {
    say(`Replay check${note}: all ${fmt(h.checked)} archived state hashes matched`);
  } else {
    warn(
      `Replay check${note}: ${fmt(h.mismatches)}/${fmt(h.checked)} hashes differed, ` +
        `first at tick ${h.first}. The video may not match what players saw ` +
        `(usually an engine version mismatch).`,
    );
  }
}

/**
 * Drawing settings, troop text and player colors from the client's theme (or
 * a simple palette without it). `makeStyle` takes anything shaped like the
 * engine's Player: type(), team(), id(), clientID(), smallID().
 */
function buildLook(clientLook: ClientLook | null, info: any) {
  const rs = clientLook?.settings;
  const ov = rs?.mapOverlay;
  const look: RenderLook = {
    territoryAlpha: ov?.territoryAlpha ?? 0.588,
    staleNuke: ov
      ? {
          rgb: [ov.staleNukeR, ov.staleNukeG, ov.staleNukeB].map((v: number) => Math.round(v * 255)),
          base: ov.staleNukeBase,
          variation: ov.staleNukeVariation,
        }
      : null,
    shapes: rs?.structure.shapes ?? {},
    name: rs?.name ?? { nameScaleFactor: 0.4, nameScaleCap: 3, troopSizeMultiplier: 0.6, outlineWidth: 1.4 },
  };
  const troopText: (troops: number) => string = clientLook?.renderTroops ?? formatTroops;
  const cosmetics = new Map<string, any>(
    info.players.map((p: any) => [p.clientID, p.cosmetics ?? {}]),
  );
  const shadeCss = (v: number) => css(rgba(Math.round(v * 255), Math.round(v * 255), Math.round(v * 255)));
  // PlayerType values ("BOT", "HUMAN", "NATION").
  const makeStyle = (p: any): Style => {
    const type = p.type();
    const team = p.team?.() ?? null;
    let fill: number;
    let border: number;
    if (clientLook) {
      const themed = clientLook.theme.territoryColor({ id: () => p.id(), type: () => type, team: () => team });
      const own = cosmetics.get(p.clientID?.()) ?? {};
      const c = clientLook.resolvePlayerColors(clientLook.theme, themed, own, team, false);
      fill = fromColord(c.territory);
      border = fromColord(c.border);
    } else {
      const id = p.smallID();
      const jitter = ((id * 7919) % 11) / 100;
      if (type === "BOT") fill = hsl((id * 47) % 360, 0.12, 0.5 + jitter);
      else if (team !== null) fill = hsl(hashString(String(team)) % 360, 0.65, 0.48 + jitter);
      else {
        const human = type === "HUMAN";
        fill = hsl((id * 137.508) % 360, human ? 0.72 : 0.45, human ? 0.52 : 0.6 + jitter / 2);
      }
      border = mix(fill, BLACK, 0.45);
    }
    const st = rs?.structure;
    const ns = rs?.name;
    return {
      fill,
      border,
      tileFill: saturate(fill, ov?.territorySaturation ?? 1),
      structFill: css(darkenValue(fill, st?.fillDarken ?? 0.65)),
      structBorder: css(darkenValue(border, st?.borderDarken ?? 0.1)),
      nameShade: shadeCss(
        type === "NATION" ? (ns?.nameShadeNation ?? 0.3)
        : type === "BOT" ? (ns?.nameShadeBot ?? 0.4)
        : 0,
      ),
      outline: css(fill),
    };
  };
  return { look, troopText, makeStyle };
}

/** The live video thread, with back-pressure: at most MAX_IN_FLIGHT frames queued. */
class LiveVideo {
  static readonly MAX_IN_FLIGHT = 4;
  framesWritten = 0;
  /** Time spent waiting for the video thread to catch up. */
  waitMs = 0;
  private inFlight = 0;
  private wake: (() => void) | null = null;
  private error: Error | null = null;
  private finished: Extract<FromVideo, { type: "finished" }> | null = null;
  private worker: Worker;

  constructor(job: LiveJob) {
    this.worker = new Worker(new URL(import.meta.url), {
      workerData: job,
      transferList: [job.init.terrain.buffer as ArrayBuffer],
    });
    this.worker.on("message", (m: FromVideo) => {
      if (m.type === "done") {
        this.framesWritten++;
        this.inFlight--;
      } else if (m.type === "finished") {
        this.finished = m;
      } else {
        this.error = new Error(m.message);
      }
      this.wake?.();
    });
    this.worker.on("error", (e) => {
      this.error = e;
      this.wake?.();
    });
    this.worker.on("exit", () => {
      this.error ??= this.finished ? null : new Error("the video thread stopped unexpectedly");
      this.wake?.();
    });
  }

  /** Resolves on the video thread's next message. */
  private next() {
    return new Promise<void>((resolve) => (this.wake = resolve));
  }

  check() {
    if (this.error) throw this.error;
  }

  async send(msg: FrameMsg) {
    this.check();
    const w0 = performance.now();
    while (this.inFlight >= LiveVideo.MAX_IN_FLIGHT) {
      await this.next();
      this.check();
    }
    this.waitMs += performance.now() - w0;
    const transfer: ArrayBuffer[] = [msg.tiles.buffer as ArrayBuffer, msg.units.buffer as ArrayBuffer];
    if (msg.full) transfer.push(msg.full.owner.buffer as ArrayBuffer, msg.full.fallout.buffer as ArrayBuffer);
    this.worker.postMessage(msg, transfer);
    this.inFlight++;
  }

  /** Tells the thread there are no more frames and waits for ffmpeg to finish. */
  async finish() {
    this.worker.postMessage({ type: "end" } satisfies ToVideo);
    while (!this.finished) {
      this.check();
      await this.next();
    }
    return this.finished;
  }
}

// ── Live drawing on several threads ──
//
// Every drawing thread gets every frame's tile changes and styles, so each keeps
// the whole map; frame i is drawn only by thread i % count. Frames come back out
// of order and the main thread writes them to ffmpeg in order. The renderer has
// no state besides the map and styles, so this draws what one thread would.

interface SplitJob {
  mode: "split";
  init: RenderInit;
}

type ToSplit =
  | {
      type: "frame";
      index: number;
      /** Whether this thread draws it (otherwise it only updates its map). */
      draw: boolean;
      progress: number;
      full?: FrameMsg["full"];
      tiles: Uint32Array;
      units?: Int32Array;
      names?: NameLabel[];
      styles: [number, Style][];
    }
  | { type: "ack" }
  | { type: "end" };

async function splitDrawThread(job: SplitJob) {
  const post = (m: FromDraw, transfer: ArrayBuffer[] = []) => parentPort!.postMessage(m, transfer);
  const renderer = await createFrameRenderer(job.init);
  // The main thread acks each frame once it's encoded.
  let unacked = 0;
  let wake: (() => void) | null = null;
  let queue = Promise.resolve();
  parentPort!.on("message", (msg: ToSplit) => {
    if (msg.type === "ack") {
      unacked--;
      wake?.();
      return;
    }
    queue = queue
      .then(async () => {
        if (msg.type === "end") {
          renderer.reportProfile();
          post({ type: "stats", drawMs: renderer.drawMs, readMs: 0 });
          return;
        }
        for (const [id, s] of msg.styles) renderer.setStyle(id, s);
        if (msg.full) renderer.setFull(msg.full.owner, msg.full.fallout);
        renderer.applyTiles(msg.tiles);
        if (!msg.draw) return;
        while (unacked >= DRAW_AHEAD) await new Promise<void>((resolve) => (wake = resolve));
        const data = renderer.draw(msg.units!, msg.names!, msg.progress).slice();
        unacked++;
        post({ type: "frame", index: msg.index, data }, [data.buffer]);
      })
      .catch((e) => {
        post({ type: "error", message: errorText(e) });
        process.exit(1);
      });
  });
}

/** Like LiveVideo, but drawing on `count` threads and encoding on this one. */
class SplitVideo {
  framesWritten = 0;
  /** Time spent waiting for the drawing threads to catch up. */
  waitMs = 0;
  /** When the first frame went out, the last one came back drawn, the first was encoded, ffmpeg finished. */
  firstSentAt = 0;
  lastDrawnAt = 0;
  firstWrittenAt = 0;
  encodedAt = 0;
  private sent = 0;
  private ready = new Map<number, Uint8Array>();
  private writing = false;
  private ffWaitMs = 0;
  private drawMs = 0;
  private statsLeft: number;
  private error: Error | null = null;
  private waiters: (() => void)[] = [];
  private workers: Worker[] = [];
  private ff: ReturnType<typeof startFfmpeg>;

  constructor(
    init: RenderInit,
    ffmpeg: Parameters<typeof startFfmpeg>[0],
    private count: number,
  ) {
    this.ff = startFfmpeg(ffmpeg);
    this.ff.done.catch((e) => this.fail(e));
    this.statsLeft = count;
    for (let i = 0; i < count; i++) {
      // Each thread gets its own copy of the terrain.
      const job: SplitJob = { mode: "split", init: { ...init, terrain: init.terrain.slice() } };
      const w = new Worker(new URL(import.meta.url), {
        workerData: job,
        transferList: [job.init.terrain.buffer as ArrayBuffer],
      });
      w.on("message", (m: FromDraw) => {
        if (m.type === "frame") {
          this.lastDrawnAt = performance.now();
          this.ready.set(m.index, m.data);
          void this.pump();
        } else if (m.type === "stats") {
          this.drawMs += m.drawMs;
          this.statsLeft--;
        } else this.fail(new Error(m.message));
        this.wakeAll();
      });
      w.on("error", (e) => this.fail(e));
      this.workers.push(w);
    }
  }

  private fail(e: unknown) {
    this.error ??= e instanceof Error ? e : new Error(String(e));
    this.wakeAll();
  }
  private wakeAll() {
    for (const w of this.waiters.splice(0)) w();
  }
  private next() {
    return new Promise<void>((resolve) => this.waiters.push(resolve));
  }
  check() {
    if (this.error) throw this.error;
  }

  /** Writes ready frames to ffmpeg in order, one write at a time. */
  private async pump() {
    if (this.writing) return;
    this.writing = true;
    try {
      for (let f = this.ready.get(this.framesWritten); f; f = this.ready.get(this.framesWritten)) {
        this.ready.delete(this.framesWritten);
        this.firstWrittenAt ||= performance.now();
        this.ffWaitMs += await this.ff.write(f);
        this.workers[this.framesWritten % this.count].postMessage({ type: "ack" } satisfies ToSplit);
        this.framesWritten++;
        this.wakeAll();
      }
    } catch (e) {
      this.fail(e);
    } finally {
      this.writing = false;
    }
  }

  async send(msg: FrameMsg) {
    this.check();
    // Don't run further ahead of the encoder than the threads may draw.
    const w0 = performance.now();
    while (this.sent - this.framesWritten >= this.count * (DRAW_AHEAD + 1)) {
      await this.next();
      this.check();
    }
    this.waitMs += performance.now() - w0;
    const index = this.sent++;
    this.firstSentAt ||= performance.now();
    const owner = index % this.count;
    for (let i = 0; i < this.count; i++) {
      const draw = i === owner;
      const m: ToSplit = {
        type: "frame",
        index,
        draw,
        progress: msg.progress,
        full: msg.full,
        tiles: msg.tiles,
        units: draw ? msg.units : undefined,
        names: draw ? msg.names : undefined,
        styles: msg.styles,
      };
      // Copied to every thread (structured clone), not transferred.
      this.workers[i].postMessage(m);
    }
  }

  /** Waits for every frame to be encoded, then for ffmpeg to finish. */
  async finish() {
    while (this.framesWritten < this.sent) {
      this.check();
      await this.next();
    }
    for (const w of this.workers) w.postMessage({ type: "end" } satisfies ToSplit);
    while (this.statsLeft > 0) {
      this.check();
      await this.next();
    }
    await Promise.all(this.workers.map((w) => w.terminate()));
    const f0 = performance.now();
    const code = await this.ff.end();
    this.encodedAt = performance.now();
    return {
      code,
      ffErr: this.ff.stderr(),
      drawMs: this.drawMs,
      ffWaitMs: this.ffWaitMs,
      finishMs: performance.now() - f0,
    };
  }
}

// ── Rendering with engine-rs ──

/**
 * Reads exact byte counts from a stream (engine-rs stream's stdout). The
 * stream is drained as fast as it arrives, into memory, so engine-rs never
 * blocks on a full pipe while this thread waits for the video thread.
 */
class StreamReader {
  private chunks: Buffer[] = [];
  /** Read position in chunks[0]. */
  private off = 0;
  private available = 0;
  private ended = false;
  private error: Error | null = null;
  private wake: (() => void) | null = null;
  constructor(stream: NodeJS.ReadableStream) {
    stream.on("data", (chunk: Buffer) => {
      this.chunks.push(chunk);
      this.available += chunk.length;
      this.wake?.();
    });
    stream.on("end", () => {
      this.ended = true;
      this.wake?.();
    });
    stream.on("error", (e: Error) => {
      this.error = e;
      this.wake?.();
    });
  }
  /** `n` bytes, copied so typed-array views can sit on them. */
  async take(n: number): Promise<Uint8Array> {
    while (this.available < n) {
      if (this.error) throw this.error;
      if (this.ended) throw new Error("engine-rs stopped before the end of the game");
      await new Promise<void>((resolve) => (this.wake = resolve));
    }
    const out = new Uint8Array(n);
    let filled = 0;
    while (filled < n) {
      const c = this.chunks[0];
      const len = Math.min(n - filled, c.length - this.off);
      out.set(c.subarray(this.off, this.off + len), filled);
      filled += len;
      this.off += len;
      if (this.off === c.length) {
        this.chunks.shift();
        this.off = 0;
      }
    }
    this.available -= n;
    return out;
  }
  async u32() {
    const b = await this.take(4);
    return new DataView(b.buffer).getUint32(0, true);
  }
  async u32s(n: number) {
    return new Uint32Array((await this.take(4 * n)).buffer);
  }
  async i32s(n: number) {
    return new Int32Array((await this.take(4 * n)).buffer);
  }
  async f64s(n: number) {
    return new Float64Array((await this.take(8 * n)).buffer);
  }
}

/**
 * Simulates with engine-rs (`engine-rs stream`, see its main.rs) instead of
 * the TypeScript engine. engine-rs works out which tick each frame shows and
 * sends what changed since the previous frame; this thread only turns that
 * into frames for the video thread.
 */
async function renderWithRust(opts: Options, engineDir: string, record: any, t0: number, tFetch: number) {
  const info = record.info;
  const exe = path.resolve(opts.rust!);
  if (!fs.existsSync(exe)) throw new Error(`engine-rs not found at ${exe} (build it with cargo build --release)`);

  const tInit0 = performance.now();
  muteEngineLogs();
  const eng = (p: string) => import(pathToFileURL(path.join(engineDir, p)).href);
  const clientLook = await loadClientLook(eng, engineDir);
  if (!clientLook) throw new Error("--rust needs the game client's theme (src/client in the engine checkout)");
  const { look, troopText, makeStyle } = buildLook(clientLook, info);

  // engine-rs reads the record from a file.
  const tmpRecord = path.join(os.tmpdir(), `openfront-render-${process.pid}-${info.gameID}.json`);
  fs.writeFileSync(tmpRecord, JSON.stringify(record));

  const frames = Math.max(1, Math.round(opts.seconds * opts.fps));
  const holdFrames = Math.round(opts.hold * opts.fps);
  const child = spawn(exe, ["stream", tmpRecord, engineDir, String(frames)], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  let engineErr = "";
  child.stderr!.on("data", (d) => (engineErr += d));
  const exited = once(child, "exit");
  let engineDoneMs = 0;
  void exited.then(() => (engineDoneMs = performance.now()));
  const r = new StreamReader(child.stdout!);

  try {
    // Header: "OFRS", size, ticks, terrain bytes.
    const magic = fromUtf8.decode(await r.take(4));
    if (magic !== "OFRS") throw new Error(`engine-rs sent ${JSON.stringify(magic)} instead of a stream header`);
    const W = await r.u32();
    const H = await r.u32();
    const totalTicks = await r.u32();
    const N = W * H;
    const terrainBytes = await r.take(N);
    await r.take((4 - (N % 4)) % 4);
    const t = clientLook.settings.terrain;
    const rgb = (hex: string) => clientLook.hexToRgb(hex) ?? undefined;
    const colors = {
      backgroundColor: rgb(t.backgroundColor),
      oceanColor: rgb(t.oceanColor),
      sandColor: rgb(t.sandColor),
      plainsColor: rgb(t.plainsColor),
      highlandColor: rgb(t.highlandColor),
      mountainColor: rgb(t.mountainColor),
    };
    const terrain = new Uint32Array(N);
    const px = new Uint8Array(4);
    for (let ref = 0; ref < N; ref++) {
      clientLook.encodeTerrainTile(terrainBytes[ref], px, 0, colors);
      terrain[ref] = rgba(px[0], px[1], px[2]);
    }

    const outW = Math.max(2, Math.round(opts.width / 2) * 2);
    const outH = Math.max(2, Math.round((H * outW) / W / 2) * 2);
    const out = path.resolve(opts.out ?? `${info.gameID}.mp4`);
    // The simulation and x264 share the cores here, so more threads stop paying
    // off quickly: 2 did best on a 16-thread machine.
    const threads = Math.max(1, Math.min(opts.threadsSet ? opts.threads : 2, frames + holdFrames));
    const video = new SplitVideo(
      { W, H, outW, outH, terrain, resourcesDir: path.join(engineDir, "resources"), look },
      { ffmpeg: opts.ffmpeg, out, outW, outH, fps: opts.fps, crf: opts.crf, preset: opts.preset },
      threads,
    );
    const tInit = performance.now() - tInit0;
    say(`  Map ${W}x${H}, engine-rs started in ${secs(tInit)}`);
    say(
      `Rendering ${fmt(totalTicks)} ticks into ${frames} frames with engine-rs, drawing on ${threads} thread${threads === 1 ? "" : "s"} ` +
        `(${opts.seconds}s at ${opts.fps} fps, ${outW}x${outH})`,
    );

    // Players in the order engine-rs created them, styled in that order (see main).
    const players: any[] = [];
    const names = new Map<number, string>();
    const styles = new Set<number>();
    let newStyles: [number, Style][] = [];
    const ensureStyle = (smallID: number) => {
      if (styles.has(smallID)) return;
      for (const p of players) {
        if (styles.has(p.smallID())) continue;
        styles.add(p.smallID());
        newStyles.push([p.smallID(), makeStyle(p)]);
      }
    };

    const simStart = performance.now();
    let readMs = 0;
    let sent = 0;
    let last = { units: new Int32Array(0), names: [] as NameLabel[] };
    let lastPrint = 0;
    let hashes = { checked: 0, mismatches: 0, first: null as number | null };
    for (let first = true; ; first = false) {
      const r0 = performance.now();
      const tick = await r.u32();
      if (tick === 0xffffffff) {
        const [checked, mismatches, firstBad] = await r.u32s(3);
        hashes = { checked, mismatches, first: firstBad === 0xffffffff ? null : firstBad };
        readMs += performance.now() - r0;
        break;
      }
      const repeat = await r.u32();
      const jsonLen = await r.u32();
      const added = JSON.parse(fromUtf8.decode(await r.take(jsonLen)));
      await r.take((4 - (jsonLen % 4)) % 4);
      for (const p of added) {
        names.set(p.smallID, p.name);
        players.push({
          smallID: () => p.smallID,
          id: () => p.id,
          type: () => p.type,
          team: () => p.team,
          clientID: () => p.clientID ?? undefined,
        });
      }
      const tiles = await r.u32s(2 * (await r.u32()));
      const units = await r.i32s(4 * (await r.u32()));
      const nameCount = await r.u32();
      const labels: NameLabel[] = [];
      for (let i = 0; i < nameCount; i++) {
        const owner = await r.u32();
        const [x, y, size, troops] = await r.f64s(4);
        if (size <= 0 || !nameIsReadable(size, look.name, W)) continue;
        labels.push({ owner, name: names.get(owner) ?? "", troops: troopText(troops), x, y, size });
      }
      readMs += performance.now() - r0;
      if (opts.strict && hashes.mismatches > 0) throw new Error("replay desynced (--strict)");

      for (let i = 1; i < tiles.length; i += 2) if ((tiles[i] & 0xffff) !== 0) ensureStyle(tiles[i] & 0xffff);
      for (let i = 2; i < units.length; i += 4) ensureStyle(units[i]);
      for (const l of labels) ensureStyle(l.owner);
      // Frame 0 carries the whole map: unowned, plus whatever engine-rs sent.
      let full: FrameMsg["full"];
      if (first) {
        const owner = new Uint16Array(N);
        const fallout = new Uint8Array(N);
        for (let i = 0; i < tiles.length; i += 2) {
          owner[tiles[i]] = tiles[i + 1] & 0xffff;
          fallout[tiles[i]] = tiles[i + 1] >> 16;
        }
        full = { owner, fallout };
      }
      // Sent arrays move to the video thread, so repeats and the hold use copies.
      last = { units: units.slice(), names: labels };
      for (let k = 0; k < repeat; k++) {
        await video.send({
          type: "frame",
          progress: tick / totalTicks,
          full: k === 0 ? full : undefined,
          tiles: k === 0 ? tiles : new Uint32Array(0),
          units: k === 0 ? units : last.units.slice(),
          names: labels,
          styles: newStyles,
        });
        newStyles = [];
        sent++;
      }

      const now = performance.now();
      if (now - lastPrint >= (isTTY ? 250 : 2000)) {
        lastPrint = now;
        const line =
          `  tick ${fmt(tick).padStart(fmt(totalTicks).length)}/${fmt(totalTicks)}` +
          `  ${((100 * tick) / totalTicks).toFixed(1).padStart(5)}%` +
          `  | frame ${video.framesWritten}/${frames + holdFrames}` +
          `  | ${((now - simStart) / 1000).toFixed(1)}s elapsed`;
        if (isTTY) process.stdout.write(`\r${line}\x1b[K`);
        else say(line);
      }
    }
    if (isTTY) process.stdout.write("\n");
    // Frames past the last tick, then the hold, repeat the final state.
    for (; sent < frames + holdFrames; sent++) {
      await video.send({
        type: "frame",
        progress: 1,
        tiles: new Uint32Array(0),
        units: last.units.slice(),
        names: last.names,
        styles: [],
      });
    }
    const simWall = performance.now() - simStart;
    const [code] = (await exited) as [number | null];
    if (code !== 0) throw new Error(`engine-rs exited with code ${code}\n${engineErr}`);

    const f0 = performance.now();
    const fin = await video.finish();
    const finishMs = performance.now() - f0;
    if (fin.code !== 0) throw new Error(`ffmpeg exited with code ${fin.code}\n${fin.ffErr}`);
    const total = performance.now() - t0;

    const size = fs.statSync(out).size;
    say("");
    say(`Done: ${out}`);
    say(
      `  ${outW}x${outH}, ${((frames + holdFrames) / opts.fps).toFixed(1)}s of video, ` +
        `${(size / 1024 / 1024).toFixed(1)} MB`,
    );
    say("");
    printTimeline(
      t0,
      t0 + total,
      [
        ["Fetch record", t0, t0 + tFetch],
        ["Load theme, start engine-rs", tInit0, tInit0 + tInit],
        ["Simulate (engine-rs)", simStart, engineDoneMs],
        [`Draw frames (${threads} thread${threads === 1 ? "" : "s"})`, video.firstSentAt, video.lastDrawnAt],
        ["Encode (ffmpeg)", video.firstWrittenAt, video.encodedAt],
      ],
      [
        "",
        `Drawing: ${secs(fin.drawMs)} of work for ${video.framesWritten} frames ` +
          `(${(fin.drawMs / Math.max(1, video.framesWritten)).toFixed(1)} ms each)`,
        `Main thread waited ${secs(readMs)} for engine-rs and ${secs(video.waitMs)} for drawing; ` +
          `ffmpeg held up writes for ${secs(fin.ffWaitMs)}`,
      ],
    );
    say("");
    reportSpeed(total, info.duration);
    reportHashes(hashes, " (engine-rs)");
  } catch (e) {
    // When engine-rs stops early its own message (e.g. "... is not ported yet")
    // says why; wait briefly for it to exit and show it.
    await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 2000))]);
    const why = engineErr
      .split("\n")
      .filter((l) => l.trim() && !l.startsWith("note: run with"))
      .slice(-4)
      .join("\n  ");
    if (child.exitCode !== null && child.exitCode !== 0 && why) {
      throw new Error(`engine-rs stopped (exit code ${child.exitCode}):\n  ${why}`);
    }
    throw e;
  } finally {
    if (child.exitCode === null) child.kill();
    fs.rmSync(tmpRecord, { force: true });
  }
}

// ── Main ──

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const t0 = performance.now();
  const engineDir = path.resolve(opts.engine);
  // Older checkouts keep the engine in src/core; newer ones split it into
  // packages/ (see docs/EnginePackageSplit.md).
  const layout =
    fs.existsSync(path.join(engineDir, "packages/engine/src/GameRunner.ts")) ? "packages"
    : fs.existsSync(path.join(engineDir, "src/core/GameRunner.ts")) ? "core"
    : null;
  if (!layout) {
    throw new Error(
      `${engineDir} doesn't look like an OpenFrontIO checkout (use --engine)`,
    );
  }

  // A cached game needs no engine, so look before anything else.
  const mine = engineCommit(engineDir);
  let record: any = null;
  const gameID = fs.existsSync(opts.input)
    ? (record = await loadRecord(opts.input)).info.gameID
    : gameIdFrom(opts.input);
  const cacheDir =
    !opts.noCache && gameID
      ? path.join(path.resolve(opts.cacheDir), `${gameID}-${(mine ?? "nogit").slice(0, 12)}`)
      : null;
  if (cacheDir && !opts.resim && !opts.rust) {
    const meta = readCacheMeta(cacheDir);
    if (meta) return renderFromCache(opts, engineDir, cacheDir, meta, t0);
  }

  // The engine depends on its own tsconfig (e.g. useDefineForClassFields:
  // false changes class field init order). Re-run under it if needed.
  const engineTsconfig = path.join(engineDir, "tsconfig.json");
  const currentTsconfig = process.env.TSX_TSCONFIG_PATH;
  if (!currentTsconfig || path.resolve(currentTsconfig) !== engineTsconfig) {
    const child = spawnSync(
      process.execPath,
      [...process.execArgv, ...process.argv.slice(1)],
      { stdio: "inherit", env: { ...process.env, TSX_TSCONFIG_PATH: engineTsconfig } },
    );
    process.exit(child.status ?? 1);
  }

  // 1. Fetch the record
  say(`Fetching replay ${opts.input} ...`);
  record ??= await loadRecord(opts.input);
  const info = record.info;
  const tFetch = performance.now() - t0;
  say(
    `  ${info.gameID}: ${info.config.gameMap}, ${info.config.gameMode}, ` +
      `${info.players.length} players, ${fmt(info.num_turns)} ticks ` +
      `(${Math.round(info.duration / 60)} min of play)`,
  );

  if (record.gitCommit && record.gitCommit !== "DEV" && mine && mine !== record.gitCommit) {
    warn(
      `this game was played on commit ${record.gitCommit.slice(0, 12)}, ` +
        `but the engine is at ${mine.slice(0, 12)}. The replay will probably desync.\n` +
        `  Fix: git -C "${engineDir}" fetch origin ${record.gitCommit} && ` +
        `git -C "${engineDir}" checkout ${record.gitCommit} && npm ci`,
    );
  }

  if (opts.rust) return renderWithRust(opts, engineDir, record, t0, tFetch);

  // 2. Load the engine and the map
  const tInit0 = performance.now();
  muteEngineLogs();
  const eng = (p: string) => import(pathToFileURL(path.join(engineDir, p)).href);
  const mods =
    layout === "packages"
      ? {
          runner: "packages/engine/src/GameRunner.ts",
          util: "packages/shared/src/SharedUtil.ts",
          mapType: "packages/engine-api/src/game/Maps.gen.ts",
          playerType: "packages/engine-api/src/game/GameTypes.ts",
          updates: "packages/engine-api/src/game/GameUpdates.ts",
        }
      : {
          runner: "src/core/GameRunner.ts",
          util: "src/core/Util.ts",
          mapType: "src/core/game/Game.ts",
          playerType: "src/core/game/Game.ts",
          updates: "src/core/game/GameUpdates.ts",
        };
  const { createGameRunner } = await eng(mods.runner);
  const { decompressGameRecord } = await eng(mods.util);
  const { GameMapType } = await eng(mods.mapType);
  const { PlayerType } = await eng(mods.playerType);
  const { GameUpdateType } = await eng(mods.updates);

  const mapsDir = path.join(engineDir, "resources/maps");
  const mapDir = (map: string) => {
    const key = Object.keys(GameMapType).find((k) => GameMapType[k] === map);
    if (!key) throw new Error(`unknown map ${map}`);
    return path.join(mapsDir, key.toLowerCase());
  };
  const readBin = (dir: string, f: string) => new Uint8Array(fs.readFileSync(path.join(dir, f)));
  const readManifest = (dir: string) =>
    JSON.parse(fs.readFileSync(path.join(dir, "manifest.json"), "utf8"));

  let mapArg: unknown;
  if (layout === "packages") {
    // Newer engines take the map's files already loaded.
    const dir = mapDir(info.config.gameMap);
    mapArg = {
      map: info.config.gameMap,
      mapSize: info.config.gameMapSize,
      manifest: readManifest(dir),
      mapBin: readBin(dir, "map.bin"),
      map4xBin: readBin(dir, "map4x.bin"),
      map16xBin: readBin(dir, "map16x.bin"),
    };
  } else {
    // Older engines ask a loader for them.
    mapArg = {
      getMapData(map: string) {
        const dir = mapDir(map);
        return {
          mapBin: async () => readBin(dir, "map.bin"),
          map4xBin: async () => readBin(dir, "map4x.bin"),
          map16xBin: async () => readBin(dir, "map16x.bin"),
          manifest: async () => readManifest(dir),
          webpPath: path.join(dir, "thumbnail.webp"),
        };
      },
    };
  }

  const turns = decompressGameRecord(record).turns as {
    turnNumber: number;
    intents: unknown[];
    hash?: number | null;
  }[];
  const totalTicks = turns.length;

  // Collected from the engine's per-tick callback.
  let fatal: string | undefined;
  const pendingTileBatches: (ArrayLike<number> | BigUint64Array)[] = [];
  let hashesChecked = 0;
  let hashMismatches = 0;
  let firstMismatch: number | null = null;
  interface NameView { x: number; y: number; size: number }
  const nameViews = new Map<string, NameView>();

  const runner = await createGameRunner(info, undefined, mapArg, (gu: any) => {
    if ("errMsg" in gu) {
      fatal = `${gu.errMsg}\n${gu.stack ?? ""}`;
      return;
    }
    if (gu.packedTileUpdates && gu.packedTileUpdates.length > 0) {
      pendingTileBatches.push(gu.packedTileUpdates);
    }
    // Only sent when the engine re-placed names; keep the last values.
    for (const [id, nv] of Object.entries(gu.playerNameViewData ?? {})) {
      nameViews.set(id, nv as NameView);
    }
    for (const h of gu.updates?.[GameUpdateType.Hash] ?? []) {
      const expected = turns[h.tick]?.hash;
      if (expected === undefined || expected === null) continue;
      hashesChecked++;
      if (expected !== h.hash) {
        hashMismatches++;
        firstMismatch ??= h.tick;
      }
    }
  });
  const game = runner.game;
  const W: number = game.width();
  const H: number = game.height();
  const N = W * H;

  // Every tick the engine builds and diffs a snapshot of every player for the
  // game's UI (PlayerImpl.toUpdate). Nothing in the simulation reads them back
  // and this script doesn't use them, so they're switched off: about 30% less
  // simulation time. The replay check would catch an engine that needs them.
  const playerProto = game.allPlayers()[0] && Object.getPrototypeOf(game.allPlayers()[0]);
  if (playerProto && typeof playerProto.toUpdate === "function") {
    playerProto.toUpdate = () => null;
  }

  // 3. Terrain colors and drawing settings
  const clientLook = await loadClientLook(eng, engineDir);
  const rs = clientLook?.settings;

  const terrain = new Uint32Array(N);
  if (clientLook) {
    // The client's own terrain coloring, from the raw terrain bytes.
    const t = rs.terrain;
    const rgb = (hex: string) => clientLook.hexToRgb(hex) ?? undefined;
    const colors = {
      backgroundColor: rgb(t.backgroundColor),
      oceanColor: rgb(t.oceanColor),
      sandColor: rgb(t.sandColor),
      plainsColor: rgb(t.plainsColor),
      highlandColor: rgb(t.highlandColor),
      mountainColor: rgb(t.mountainColor),
    };
    const px = new Uint8Array(4);
    for (let ref = 0; ref < N; ref++) {
      clientLook.encodeTerrainTile(game.terrainByte(ref), px, 0, colors);
      terrain[ref] = rgba(px[0], px[1], px[2]);
    }
  } else {
    for (let ref = 0; ref < N; ref++) {
      if (game.isLand(ref)) {
        const m = game.magnitude(ref);
        terrain[ref] =
          m < 10 ? mix(PLAINS, HIGHLAND, m / 20)
          : m < 20 ? mix(HIGHLAND, MOUNTAIN, (m - 10) / 25)
          : MOUNTAIN;
      } else if (game.isOcean(ref)) {
        terrain[ref] = game.isShoreline(ref) ? SHALLOW : OCEAN;
      } else {
        terrain[ref] = LAKE;
      }
    }
  }

  const { look, troopText, makeStyle } = buildLook(clientLook, info);

  // Each player's colors. The theme hands colors out in the order players
  // appear (like the client's GameView), so players are styled in that order.
  // New styles go to the video thread with the next frame.
  const styles = new Map<number, Style>();
  const newStyles: [number, Style][] = [];
  const addStyle = (id: number, p: any) => {
    const s = makeStyle(p);
    styles.set(id, s);
    newStyles.push([id, s]);
  };
  const ensureStyle = (smallID: number) => {
    if (styles.has(smallID)) return;
    for (const p of game.allPlayers()) {
      if (!styles.has(p.smallID())) addStyle(p.smallID(), p);
    }
    if (!styles.has(smallID)) addStyle(smallID, game.playerBySmallID(smallID));
  };

  // The state the frames and the cache are made of.
  const tileValue = (ref: number) => game.ownerID(ref) | (game.hasFallout(ref) ? 0x10000 : 0);
  const collectUnits = (): number[] => {
    const units: number[] = [];
    for (const unit of game.units()) {
      if (!unit.isActive()) continue;
      const type = unit.type();
      let code: number | undefined;
      const s = STRUCTURES.indexOf(type);
      if (s >= 0) code = STRUCTURE_CODE + s;
      else if (type === "Train") {
        code = unit.trainType?.() === "Engine" ? 9 : unit.isLoaded?.() ? 11 : 10;
      } else code = UNIT_COLS[type];
      if (code === undefined) continue;
      const o = unit.owner().smallID();
      ensureStyle(o);
      units.push(code, unit.tile(), o, unit.isUnderConstruction?.() ? 1 : 0);
    }
    return units;
  };
  const collectNames = (): NameLabel[] => {
    const names: NameLabel[] = [];
    for (const p of game.allPlayers()) {
      if (!p.isAlive()) continue;
      const nv = nameViews.get(p.id());
      if (!nv || nv.size <= 0 || !nameIsReadable(nv.size, look.name, W)) continue;
      ensureStyle(p.smallID());
      names.push({
        owner: p.smallID(),
        name: p.displayName(),
        troops: troopText(p.troops()),
        x: nv.x,
        y: nv.y,
        size: nv.size,
      });
    }
    return names;
  };

  // Output size: requested width, height follows the map, both even.
  const outW = Math.max(2, Math.round(opts.width / 2) * 2);
  const outH = Math.max(2, Math.round((H * outW) / W / 2) * 2);
  const out = path.resolve(opts.out ?? `${info.gameID}.mp4`);
  const cache = cacheDir ? new CacheWriter(cacheDir) : null;

  // 4. Start the video thread (drawing + ffmpeg)
  const liveJob: LiveJob = {
    mode: "live",
    init: { W, H, outW, outH, terrain: new Uint32Array(terrain), resourcesDir: path.join(engineDir, "resources"), look },
    ffmpeg: { ffmpeg: opts.ffmpeg, out, outW, outH, fps: opts.fps, crf: opts.crf, preset: opts.preset },
  };
  const video = new LiveVideo(liveJob);

  const tInit = performance.now() - tInit0;
  say(`  Map ${W}x${H}, engine ready in ${secs(tInit)}`);

  let prepMs = 0;
  let cacheMs = 0;

  // Tiles changed since the last frame (values are read when the frame is sent)
  // and in the current tick (for the cache).
  const frameStamp = new Uint32Array(N);
  let frameGen = 1;
  const frameRefs: number[] = [];
  const tickStamp = new Uint32Array(N);
  let tickGen = 1;
  const collectTiles = (): number[] => {
    const tickTiles: number[] = [];
    for (const batch of pendingTileBatches) {
      const old = batch instanceof BigUint64Array;
      const step = old ? 1 : 2; // old engines: one bigint per tile; newer: [ref, state] pairs
      for (let i = 0; i < batch.length; i += step) {
        const ref = old ? Number((batch as BigUint64Array)[i] >> 16n) : (batch[i] as number);
        if (frameStamp[ref] !== frameGen) {
          frameStamp[ref] = frameGen;
          frameRefs.push(ref);
        }
        if (cache && tickStamp[ref] !== tickGen) {
          tickStamp[ref] = tickGen;
          const v = tileValue(ref);
          if ((v & 0xffff) !== 0) ensureStyle(v & 0xffff);
          tickTiles.push(ref, v);
        }
      }
    }
    pendingTileBatches.length = 0;
    tickGen++;
    return tickTiles;
  };

  const sendFrame = async (
    progress: number,
    state: { units: number[]; names: NameLabel[] },
    full?: FrameMsg["full"],
  ) => {
    video.check();
    const p0 = performance.now();
    const tiles = new Uint32Array(frameRefs.length * 2);
    for (let i = 0; i < frameRefs.length; i++) {
      const v = tileValue(frameRefs[i]);
      if ((v & 0xffff) !== 0) ensureStyle(v & 0xffff);
      tiles[2 * i] = frameRefs[i];
      tiles[2 * i + 1] = v;
    }
    frameRefs.length = 0;
    frameGen++;
    const msg: FrameMsg = {
      type: "frame",
      progress,
      full,
      tiles,
      units: Int32Array.from(state.units),
      names: state.names,
      styles: newStyles.splice(0),
    };
    prepMs += performance.now() - p0;
    await video.send(msg);
  };

  // 5. Simulate, sending each frame's state to the video thread
  const frames = Math.max(1, Math.round(opts.seconds * opts.fps));
  const holdFrames = Math.round(opts.hold * opts.fps);
  say(
    `Rendering ${fmt(totalTicks)} ticks into ${frames} frames ` +
      `(${opts.seconds}s at ${opts.fps} fps, ${outW}x${outH}, ` +
      `~${Math.max(1, Math.round(totalTicks / frames))} ticks per frame)`,
  );

  try {
    // Frame 0 shows the empty map.
    {
      pendingTileBatches.length = 0;
      const owner = new Uint16Array(N);
      const fallout = new Uint8Array(N);
      for (let ref = 0; ref < N; ref++) {
        owner[ref] = game.ownerID(ref);
        fallout[ref] = game.hasFallout(ref) ? 1 : 0;
        if (owner[ref] !== 0) ensureStyle(owner[ref]);
      }
      const state = { units: collectUnits(), names: collectNames() };
      cache?.writeStart(owner, fallout, state.units, state.names);
      await sendFrame(0, state, { owner, fallout });
    }

    const simStart = performance.now();
    let simMs = 0;
    let nextFrame = 1;
    let lastPrint = 0;
    const printProgress = (tick: number, final = false) => {
      const now = performance.now();
      if (!final && now - lastPrint < (isTTY ? 250 : 2000)) return;
      lastPrint = now;
      const elapsed = now - simStart;
      const rate = tick / (elapsed / 1000);
      const left = rate > 0 ? (totalTicks - tick) / rate : 0;
      const line =
        `  tick ${fmt(tick).padStart(fmt(totalTicks).length)}/${fmt(totalTicks)}` +
        `  ${((100 * tick) / totalTicks).toFixed(1).padStart(5)}%` +
        `  | ${fmt(rate)} ticks/s` +
        `  | frame ${video.framesWritten}/${frames + holdFrames}` +
        `  | ${(elapsed / 1000).toFixed(1)}s elapsed` +
        (final ? "" : `, ~${left.toFixed(1)}s left`);
      if (isTTY) process.stdout.write(`\r${line}\x1b[K${final ? "\n" : ""}`);
      else say(line);
    };

    let last = { units: [] as number[], names: [] as NameLabel[] };
    for (let i = 0; i < totalTicks; i++) {
      runner.addTurn(turns[i]);
      const s0 = performance.now();
      const ok = runner.executeNextTick();
      simMs += performance.now() - s0;
      if (fatal !== undefined || ok === false) {
        if (isTTY) process.stdout.write("\n");
        throw new Error(`the engine failed at tick ${i}:\n${fatal ?? "executeNextTick returned false"}`);
      }
      if (opts.strict && hashMismatches > 0) {
        throw new Error(`replay desynced at tick ${firstMismatch} (--strict)`);
      }
      const done = i + 1;
      const c0 = performance.now();
      const tickTiles = collectTiles();
      const frameDue = nextFrame < frames && done >= Math.ceil((nextFrame * totalTicks) / (frames - 1));
      const state = cache || frameDue ? { units: collectUnits(), names: collectNames() } : null;
      if (cache && state) cache.writeTick(done, tickTiles, state.units, state.names);
      if (state) last = state;
      cacheMs += performance.now() - c0;
      while (nextFrame < frames && done >= Math.ceil((nextFrame * totalTicks) / (frames - 1))) {
        await sendFrame(done / totalTicks, state!);
        nextFrame++;
      }
      printProgress(done);
    }
    if (!cache) last = { units: collectUnits(), names: collectNames() };
    while (nextFrame < frames) {
      await sendFrame(1, last);
      nextFrame++;
    }
    for (let i = 0; i < holdFrames; i++) await sendFrame(1, last);
    printProgress(totalTicks, true);
    const simWall = performance.now() - simStart;

    // 6. Let the video thread draw what's queued and finish encoding; save the cache meanwhile
    const f0 = performance.now();
    const finishing = video.finish();
    const hashes = { checked: hashesChecked, mismatches: hashMismatches, first: firstMismatch };
    const saved = cache
      ?.finish(terrain, {
        version: CACHE_VERSION,
        gameID: info.gameID,
        map: info.config.gameMap,
        mode: info.config.gameMode,
        players: info.players.length,
        durationSec: info.duration,
        gitCommit: record.gitCommit ?? null,
        engineCommit: mine,
        W, H,
        ticks: totalTicks,
        look,
        styles: [...styles],
        names: Object.fromEntries(game.allPlayers().map((p: any) => [p.smallID(), p.displayName()])),
        hashes,
      })
      .then(() => true, (e: unknown) => {
        warn(`couldn't save the cache: ${errorText(e)}`);
        return false;
      });
    const fin = await finishing;
    const cached = await saved;
    const finishMs = performance.now() - f0;
    if (fin.code !== 0) throw new Error(`ffmpeg exited with code ${fin.code}\n${fin.ffErr}`);
    const total = performance.now() - t0;

    // 7. Report
    const size = fs.statSync(out).size;
    say("");
    say(`Done: ${out}`);
    say(
      `  ${outW}x${outH}, ${((frames + holdFrames) / opts.fps).toFixed(1)}s of video, ` +
        `${(size / 1024 / 1024).toFixed(1)} MB`,
    );
    if (cached) say(`  Cached for re-renders: ${cacheDir}`);
    say("");
    printTimeline(
      t0,
      t0 + total,
      [
        ["Fetch record", t0, t0 + tFetch],
        ["Load engine + map", tInit0, tInit0 + tInit],
        ["Simulate (main thread)", simStart, simStart + simWall],
        ["Draw + encode (video thread)", simStart, f0 + finishMs],
      ],
      [
        "",
        `Simulation: ${secs(simMs)} of work (${fmt(totalTicks / (simMs / 1000))} ticks/s); ` +
          `${cache ? "collecting state" : "collecting units"} ${secs(cacheMs)}, preparing frames ${secs(prepMs)}`,
        `Drawing: ${secs(fin.drawMs)} for ${video.framesWritten} frames ` +
          `(${(fin.drawMs / Math.max(1, video.framesWritten)).toFixed(1)} ms each)`,
        `Main thread waited ${secs(video.waitMs)} for the video thread; ffmpeg held up writes for ${secs(fin.ffWaitMs)}`,
      ],
    );
    say("");
    reportSpeed(total, info.duration);
    reportHashes(hashes);
    writeExtras(opts, out, {
      gameID: info.gameID,
      map: info.config.gameMap,
      mode: info.config.gameMode,
      players: info.players.length,
      durationSeconds: info.duration,
      ticks: totalTicks,
      width: outW,
      height: outH,
      videoSeconds: (frames + holdFrames) / opts.fps,
      bytes: size,
      renderSeconds: total / 1000,
      hashesChecked: hashes.checked,
      hashMismatches: hashes.mismatches,
    });
  } catch (e) {
    cache?.discard();
    throw e;
  }
}

/** --meta and --thumbnail (the last frame, taken from the video). */
function writeExtras(opts: Options, video: string, meta: Record<string, unknown>) {
  if (opts.meta) fs.writeFileSync(opts.meta, JSON.stringify(meta, null, 2));
  if (opts.thumbnail) {
    const r = spawnSync(
      opts.ffmpeg,
      ["-v", "error", "-y", "-sseof", "-0.5", "-i", video, "-update", "1", "-frames:v", "1", opts.thumbnail],
      { encoding: "utf8" },
    );
    if (r.status !== 0) throw new Error(`couldn't save the thumbnail: ${r.stderr || r.error}`);
  }
}

if (isMainThread) {
  main().catch((e) => {
    realErr(`\nError: ${errorText(e)}`);
    process.exit(1);
  });
} else {
  const job = workerData as LiveJob | DrawJob | SplitJob;
  const run =
    job.mode === "live" ? liveVideoThread(job)
    : job.mode === "split" ? splitDrawThread(job)
    : drawThread(job);
  run.catch((e) => {
    parentPort!.postMessage({ type: "error", message: errorText(e) });
    process.exit(1);
  });
}
