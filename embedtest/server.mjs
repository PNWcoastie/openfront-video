// Discord embed timing test: how slowly can a video arrive and still embed?
//
// Serves one test video behind pages with the same Open Graph tags as
// openfront.video, and sends the video slowly, spread over a chosen number of
// seconds. Every request is logged (who, what range, how many bytes went out,
// whether the client hung up), so the log shows what Discord's proxy did.
//
//   node server.mjs            (needs Node 18+, no packages)
//
// Settings (environment): PORT (8090), VIDEO (./test.webm), VIDEO_TYPE
// (video/webm), VIDEO_WIDTH/VIDEO_HEIGHT (1920x1080), PUBLIC_URL
// (http://localhost:PORT), LOG (./embedtest.log).
//
// Modes, each at 0, 5, 6, 7, 8, 9, 10, 15, 30 and 60 s:
//   slow     the video trickles over N s, with its real Content-Length and byte ranges
//   chunked  the video trickles over N s with no length (chunked), ranges ignored
//   page     the page itself is held N s, then the video comes at full speed
//   front    the first FRONT_BYTES (1 MB) go out at once, the rest trickles over N s
// Every link carries a fresh ID, so Discord can't answer from its cache.

import fs from "node:fs";
import http from "node:http";
import zlib from "node:zlib";
import crypto from "node:crypto";

const PORT = Number(process.env.PORT ?? 8090);
const VIDEO = process.env.VIDEO ?? "./test.webm";
const TYPE = process.env.VIDEO_TYPE ?? "video/webm";
const W = Number(process.env.VIDEO_WIDTH ?? 1920);
const H = Number(process.env.VIDEO_HEIGHT ?? 1080);
const PUBLIC = (process.env.PUBLIC_URL ?? `http://localhost:${PORT}`).replace(/\/$/, "");
const LOG = process.env.LOG ?? "./embedtest.log";
const DELAYS = [0, 5, 6, 7, 8, 9, 10, 15, 30, 60];
const MODES = ["slow", "chunked", "page", "front"];
const FRONT = Number(process.env.FRONT_BYTES ?? 1e6);

const recent = [];
// Without the video the server still starts, and says what's missing.
let video = Buffer.alloc(0);
let problem = "";
try {
  video = fs.readFileSync(VIDEO);
  if (TYPE === "video/webm" && video.subarray(0, 4).toString("hex") !== "1a45dfa3") {
    problem = `${VIDEO} isn't a WebM file (${video.length} bytes, starts with ${JSON.stringify(video.subarray(0, 16).toString("latin1"))}); the download probably got an HTML page`;
  }
} catch (e) {
  problem = `can't read ${VIDEO}: ${e.message}`;
}

function log(line) {
  const l = `${new Date().toISOString()} ${line}`;
  console.log(l);
  recent.push(l);
  if (recent.length > 1000) recent.shift();
  fs.appendFile(LOG, l + "\n", () => {});
}

// A plain dark gray PNG for og:image (Discord wants an image next to the video).
function png(w, h) {
  const crcTable = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (buf) => {
    let c = 0xffffffff;
    for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type, data) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const td = Buffer.concat([Buffer.from(type), data]);
    const c = Buffer.alloc(4);
    c.writeUInt32BE(crc(td));
    return Buffer.concat([len, td, c]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 2; // RGB
  const row = Buffer.alloc(1 + w * 3, 0x30);
  row[0] = 0; // filter: none
  const raw = Buffer.concat(Array.from({ length: h }, () => row));
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}
const thumb = png(W, H);

const esc = (s) => String(s).replace(/[&<>"]/g, (c) => `&#${c.charCodeAt(0)};`);
const newId = () => crypto.randomBytes(4).toString("hex");
const who = (req) =>
  `${req.headers["x-forwarded-for"] ?? req.socket.remoteAddress} "${req.headers["user-agent"] ?? "-"}"`;

function index(res) {
  const rows = MODES.map((mode) => {
    const links = DELAYS.map((s) => {
      const url = `${PUBLIC}/t/${mode}/${newId()}-${s}`;
      return `<li><a href="${url}">${url}</a></li>`;
    }).join("");
    return `<h2>${mode}</h2><ul>${links}</ul>`;
  }).join("");
  res.writeHead(200, { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" });
  res.end(`<!doctype html><meta charset="utf-8"><title>embed test</title>
<style>body{font:15px/1.5 system-ui;margin:24px;background:#16181d;color:#ddd}a{color:#7cb7ff}</style>
<h1>Discord embed timing test</h1>
${problem ? `<p style="color:#ff8080"><b>Problem:</b> ${esc(problem)}</p>` : ""}
<p>Fresh links on every reload. Video: ${esc(VIDEO)}, ${(video.length / 1e6).toFixed(1)} MB.
slow = video trickles over N s with Content-Length; chunked = trickles with no length;
page = the page is held N s; front = first ${(FRONT / 1e6).toFixed(1)} MB at once, then trickles. Log: <a href="/log">/log</a></p>${rows}`);
}

function page(res, mode, id, secs) {
  const v = `${PUBLIC}/f/${mode}/${id}-${secs}.${TYPE.split("/")[1]}`;
  const img = `${PUBLIC}/i/${id}.png`;
  const t = `Embed test: ${mode}, ${secs} s`;
  const html = `<!doctype html><html><head><meta charset="utf-8"><title>${esc(t)}</title>
<meta property="og:site_name" content="embed test">
<meta property="og:type" content="video.other">
<meta property="og:title" content="${esc(t)}">
<meta property="og:description" content="${esc(mode)} ${secs} s (${id})">
<meta property="og:url" content="${PUBLIC}/t/${mode}/${id}-${secs}">
<meta property="og:image" content="${img}">
<meta property="og:image:width" content="${W}"><meta property="og:image:height" content="${H}">
<meta property="og:video" content="${v}">
<meta property="og:video:url" content="${v}">
<meta property="og:video:secure_url" content="${v}">
<meta property="og:video:type" content="${TYPE}">
<meta property="og:video:width" content="${W}"><meta property="og:video:height" content="${H}">
<meta name="twitter:card" content="player">
<meta name="twitter:title" content="${esc(t)}">
<meta name="twitter:image" content="${img}">
<meta name="twitter:player" content="${v}">
<meta name="twitter:player:stream" content="${v}">
<meta name="twitter:player:stream:content_type" content="${TYPE}">
</head><body><video src="${v}" controls width="640"></video></body></html>`;
  const send = () => {
    res.writeHead(200, { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" });
    res.end(html);
  };
  if (mode === "page" && secs > 0) setTimeout(send, secs * 1000);
  else send();
}

// Sends body[start..end] spread evenly so the whole file would take `secs`.
function trickle(req, res, mode, secs, tag) {
  const total = video.length;
  let start = 0;
  let end = total - 1;
  const headers = { "content-type": TYPE, "cache-control": "no-store" };
  let status = 200;
  const range = req.headers.range;
  if (mode === "chunked") {
    // No length and no ranges: the client can't tell how much is coming.
  } else {
    headers["accept-ranges"] = "bytes";
    const m = range && /^bytes=(\d*)-(\d*)$/.exec(range);
    if (m && (m[1] || m[2])) {
      if (m[1]) {
        start = Number(m[1]);
        end = m[2] ? Math.min(Number(m[2]), total - 1) : total - 1;
      } else {
        start = Math.max(0, total - Number(m[2]));
      }
      if (start > end || start >= total) {
        res.writeHead(416, { "content-range": `bytes */${total}` });
        res.end();
        log(`${tag} 416 range=${range}`);
        return;
      }
      status = 206;
      headers["content-range"] = `bytes ${start}-${end}/${total}`;
    }
    headers["content-length"] = String(end - start + 1);
  }
  res.writeHead(status, headers);
  if (req.method === "HEAD") {
    res.end();
    log(`${tag} ${status} HEAD range=${range ?? "-"}`);
    return;
  }
  const t0 = Date.now();
  // Full speed when secs = 0 (or page mode); otherwise the file's rate over `secs`.
  // Bytes due right away; the rest goes out evenly over `secs` (full speed when
  // secs = 0 or in page mode).
  const head = mode === "front" ? Math.min(FRONT, end + 1 - start) : 0;
  const rate = mode === "page" || secs === 0 ? Infinity : (total - head) / secs; // bytes per second
  let pos = start;
  let done = false;
  const finish = (how) => {
    if (done) return;
    done = true;
    clearInterval(timer);
    log(`${tag} ${status} range=${range ?? "-"} ${how}: sent ${pos - start}/${end - start + 1} bytes in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
  };
  const step = () => {
    if (done) return;
    const due = rate === Infinity ? end + 1 : Math.min(end + 1, start + head + Math.floor(((Date.now() - t0) / 1000) * rate));
    if (due > pos) {
      res.write(video.subarray(pos, due));
      pos = due;
    }
    if (pos > end) {
      res.end();
      finish("complete");
    }
  };
  const timer = setInterval(step, 100);
  res.on("close", () => finish(pos > end ? "complete" : "CLIENT HUNG UP"));
  step();
}

http
  .createServer((req, res) => {
    const url = new URL(req.url, "http://x");
    const p = url.pathname;
    let m;
    if (p === "/") return index(res);
    if (p === "/log") {
      res.writeHead(200, { "content-type": "text/plain; charset=utf-8", "cache-control": "no-store" });
      return res.end(recent.slice(-500).join("\n") + "\n");
    }
    if ((m = /^\/t\/(slow|chunked|page|front)\/([0-9a-f]+)-(\d+)$/.exec(p))) {
      log(`page  ${m[1]} ${m[3]}s id=${m[2]} ${who(req)}`);
      return page(res, m[1], m[2], Number(m[3]));
    }
    if ((m = /^\/f\/(slow|chunked|page|front)\/([0-9a-f]+)-(\d+)\.\w+$/.exec(p))) {
      const tag = `video ${m[1]} ${m[3]}s id=${m[2]} ${who(req)}`;
      log(`${tag} start`);
      return trickle(req, res, m[1], Number(m[3]), tag);
    }
    if ((m = /^\/i\/([0-9a-f]+)\.png$/.exec(p))) {
      log(`image id=${m[1]} ${who(req)}`);
      res.writeHead(200, { "content-type": "image/png", "content-length": thumb.length, "cache-control": "no-store" });
      return res.end(thumb);
    }
    log(`404 ${req.method} ${p} ${who(req)}`);
    res.writeHead(404);
    res.end("not found");
  })
  .listen(PORT, () => {
    log(`embed test on :${PORT}, ${PUBLIC}, video ${VIDEO} (${video.length} bytes)`);
    if (problem) log(`PROBLEM: ${problem}`);
  });
