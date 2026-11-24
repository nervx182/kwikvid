// kwikvid preview spike front end. Plain JS, no build step.
"use strict";

const invoke = window.__TAURI__.core.invoke;

const PATHS = [
  { id: "baseline", kind: "baseline", note: "requestAnimationFrame only: the display/compositor rate" },
  { id: "native-wgpu", kind: "native" },
  { id: "raw-1080", kind: "raw", w: 1920, h: 1080, note: "raw RGBA over kvframe:// -> WebGL texImage2D" },
  { id: "jpeg-1080", kind: "jpeg", w: 1920, h: 1080, note: "JPEG q80 over kvframe:// -> createImageBitmap -> WebGL" },
  { id: "raw-540", kind: "raw", w: 960, h: 540, note: "half-res proxy, raw RGBA -> WebGL (upscaled)" },
];
const IN_FLIGHT = 3; // parallel frame requests, like a small decode-ahead queue
const WARMUP_MS = 1000;

const canvas = document.getElementById("view");
const live = document.getElementById("live");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const frameBase = navigator.userAgent.includes("Windows") ? "http://kvframe.localhost/" : "kvframe://localhost/";
const log = (line) => invoke("log_line", { line }).catch(() => {});
let busy = false;

// ---------- WebGL blit ----------
const gl = canvas.getContext("webgl2", { alpha: false, antialias: false, depth: false });
let tex = null;
let prog = null;
function initGl() {
  if (!gl) return;
  const vs = `#version 300 es
    out vec2 uv;
    void main() {
      vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
      uv = vec2(p.x, 1.0 - p.y);
      gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
    }`;
  const fs = `#version 300 es
    precision mediump float;
    in vec2 uv; uniform sampler2D t; out vec4 c;
    void main() { c = texture(t, uv); }`;
  const sh = (type, src) => {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    return s;
  };
  prog = gl.createProgram();
  gl.attachShader(prog, sh(gl.VERTEX_SHADER, vs));
  gl.attachShader(prog, sh(gl.FRAGMENT_SHADER, fs));
  gl.linkProgram(prog);
  tex = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, tex);
  for (const [k, v] of [
    [gl.TEXTURE_MIN_FILTER, gl.LINEAR],
    [gl.TEXTURE_MAG_FILTER, gl.LINEAR],
    [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE],
    [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE],
  ]) gl.texParameteri(gl.TEXTURE_2D, k, v);
}
function resize() {
  const dpr = window.devicePixelRatio || 1;
  canvas.width = Math.round(innerWidth * dpr);
  canvas.height = Math.round(innerHeight * dpr);
}
function blit(frame, w, h) {
  gl.bindTexture(gl.TEXTURE_2D, tex);
  if (frame instanceof Uint8Array) {
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, frame);
  } else {
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame);
  }
  gl.viewport(0, 0, canvas.width, canvas.height);
  gl.useProgram(prog);
  gl.drawArrays(gl.TRIANGLES, 0, 3);
}

// ---------- paths ----------
function newCounters() {
  return { displayed: 0, produced: 0, latencyMs: [], intervalMs: [], genMs: [], t0: performance.now() };
}

async function runWeb(path, seconds) {
  let running = true;
  let measuring = false;
  let c = newCounters();
  let seq = 0;
  let latest = null; // newest received frame not yet superseded
  let shownN = -1;
  let pending = null; // drawn last rAF, latency recorded on the next one
  let lastShowAt = null;
  let errors = 0;

  async function pump() {
    while (running) {
      const n = seq++;
      const t0 = performance.now();
      try {
        const r = await fetch(`${frameBase}${path.kind}/${path.w}/${path.h}/${n}`);
        if (!r.ok) throw new Error(`HTTP ${r.status}: ${await r.text()}`);
        const g = parseFloat(r.headers.get("X-Gen-Ms"));
        const img = path.kind === "raw" ? new Uint8Array(await r.arrayBuffer()) : await createImageBitmap(await r.blob());
        if (measuring) {
          c.produced++;
          if (!Number.isNaN(g)) c.genMs.push(g);
        }
        if (!latest || n > latest.n) {
          if (latest && latest.img.close) latest.img.close(); // texture already holds a copy
          latest = { n, t0, img };
        } else if (img.close) img.close();
      } catch (e) {
        if (errors++ < 3) log(`${path.id} fetch error: ${e}`);
        await sleep(20);
      }
    }
  }

  function onFrame() {
    if (!running) return;
    const now = performance.now();
    if (pending) {
      if (measuring) {
        c.latencyMs.push(now - pending.t0);
        if (lastShowAt !== null) c.intervalMs.push(now - lastShowAt);
        c.displayed++;
      }
      lastShowAt = now;
      pending = null;
    }
    if (latest && latest.n !== shownN) {
      blit(latest.img, path.w, path.h);
      shownN = latest.n;
      pending = { t0: latest.t0 };
    }
    requestAnimationFrame(onFrame);
  }

  const pumps = Array.from({ length: IN_FLIGHT }, pump);
  requestAnimationFrame(onFrame);
  await sleep(WARMUP_MS);
  c = newCounters();
  lastShowAt = null;
  measuring = true;
  const stopHud = hud(path.id, () => c);
  await sleep(seconds * 1000);
  const elapsed = (performance.now() - c.t0) / 1000;
  running = false;
  measuring = false;
  stopHud();
  // Do not hang on a request that never completes.
  await Promise.race([Promise.allSettled(pumps), sleep(3000)]);
  const notes = `${path.note}; in-flight=${IN_FLIGHT}; latency = request start -> first rAF after the draw; errors=${errors}`;
  return report(path.id, elapsed, c, notes);
}

async function runBaseline(path, seconds) {
  let running = true;
  let c = newCounters();
  let last = null;
  function onFrame() {
    if (!running) return;
    const now = performance.now();
    if (last !== null) c.intervalMs.push(now - last);
    last = now;
    c.displayed++;
    c.produced++;
    requestAnimationFrame(onFrame);
  }
  requestAnimationFrame(onFrame);
  await sleep(WARMUP_MS);
  c = newCounters();
  last = null;
  const stopHud = hud(path.id, () => c);
  await sleep(seconds * 1000);
  running = false;
  stopHud();
  return report(path.id, (performance.now() - c.t0) / 1000, c, path.note);
}

async function runNative(path, seconds) {
  try {
    await invoke("native_start");
  } catch (e) {
    await log(`SKIP ${path.id}: ${e}`);
    return null;
  }
  document.body.classList.add("native");
  const timer = setInterval(async () => {
    const [frames, secs] = await invoke("native_progress");
    live.textContent = `${path.id}: ${frames} frames, ${(frames / Math.max(secs, 1e-3)).toFixed(1)} fps`;
  }, 500);
  await sleep(seconds * 1000);
  clearInterval(timer);
  try {
    return await invoke("native_stop");
  } finally {
    document.body.classList.remove("native");
  }
}

function report(id, seconds, c, notes) {
  return invoke("report", {
    path: id,
    seconds,
    displayed: c.displayed,
    produced: c.produced,
    samples: { latencyMs: c.latencyMs, intervalMs: c.intervalMs, genMs: c.genMs },
    notes,
  });
}

function hud(id, get) {
  let prev = { t: performance.now(), d: 0 };
  const timer = setInterval(() => {
    const c = get();
    const t = performance.now();
    const fps = ((c.displayed - prev.d) * 1000) / (t - prev.t);
    prev = { t, d: c.displayed };
    const lat = c.latencyMs.slice(-30);
    const avg = lat.length ? lat.reduce((a, b) => a + b, 0) / lat.length : NaN;
    live.textContent = `${id}: ${fps.toFixed(1)} fps, latency ~${avg.toFixed(1)} ms`;
  }, 500);
  return () => clearInterval(timer);
}

async function runPath(path, seconds) {
  setActive(path.id);
  await log(`RUN ${path.id} for ${seconds}s`);
  let r = null;
  if (path.kind === "baseline") r = await runBaseline(path, seconds);
  else if (path.kind === "native") r = await runNative(path, seconds);
  else r = await runWeb(path, seconds);
  if (r) live.textContent = `${r.path}: ${r.fps.toFixed(1)} fps, p50 ${Number(r.latencyP50Ms).toFixed(1)} ms`;
  setActive(null);
  return r;
}

async function runAll(seconds, only = []) {
  for (const p of PATHS) if (!only.length || only.includes(p.id)) await runPath(p, seconds);
}

// ---------- UI ----------
function setActive(id) {
  for (const b of document.querySelectorAll("#buttons button")) b.classList.toggle("active", b.dataset.id === id);
}
async function exclusive(fn) {
  if (busy) return;
  busy = true;
  try {
    await fn();
  } catch (e) {
    await log(`ERROR ${e}`);
  } finally {
    busy = false;
  }
}

async function main() {
  initGl();
  resize();
  addEventListener("resize", resize);
  const cfg = await invoke("bench_config");
  await log(
    `ENV ua="${navigator.userAgent}" dpr=${devicePixelRatio} viewport=${innerWidth}x${innerHeight} ` +
      `screen=${screen.width}x${screen.height} webgl2=${!!gl} webcodecs=${"VideoDecoder" in window}`
  );
  const box = document.getElementById("buttons");
  for (const p of PATHS) {
    const b = document.createElement("button");
    b.textContent = p.id;
    b.dataset.id = p.id;
    b.onclick = () => exclusive(() => runPath(p, cfg.seconds));
    box.appendChild(b);
  }
  document.getElementById("all").onclick = () => exclusive(() => runAll(cfg.seconds));

  if (cfg.bench) {
    let code = 0;
    try {
      if (!gl) throw new Error("WebGL2 unavailable");
      await runAll(cfg.seconds, cfg.only);
    } catch (e) {
      await log(`ERROR ${e}`);
      code = 1;
    }
    await invoke("finish", { code });
  }
}

main().catch((e) => log(`ERROR main: ${e}`));
