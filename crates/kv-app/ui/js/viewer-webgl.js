// The WebGL preview fallback (ADR 0007, src/webgl.rs): when the backend is
// "webgl" (Linux, Windows, or macOS with KV_PREVIEW=webgl) the viewer gets
// a <canvas> behind its overlays. Each animation frame fetches the newest
// preview frame (raw RGBA, at most 960x540) from api.frameUrl(seq), which
// answers 204 while nothing newer exists, uploads it to a WebGL2 texture
// and draws it letterboxed to the project's aspect ratio on black.

const ASPECTS = { landscape16x9: 16 / 9, portrait9x16: 9 / 16, square1x1: 1 };
/** Wait this long after a failed fetch before trying again. */
const RETRY_MS = 500;

let projectAspect = null;
let onAspect = () => {};

/** The largest rect of `aspect` (w/h) that fits in cw x ch, centred. */
export function letterbox(cw, ch, aspect) {
  if (!(cw > 0 && ch > 0 && aspect > 0)) return { x: 0, y: 0, w: 0, h: 0 };
  let w = cw;
  let h = Math.min(ch, Math.max(1, Math.round(cw / aspect)));
  if (cw / ch > aspect) {
    h = ch;
    w = Math.min(cw, Math.max(1, Math.round(ch * aspect)));
  }
  return { x: Math.floor((cw - w) / 2), y: Math.floor((ch - h) / 2), w, h };
}

/** Follow the project's aspect ratio (called with every snapshot). */
export function renderViewerWebGl(snapshot) {
  const a = ASPECTS[snapshot.project.settings.aspect_ratio] ?? null;
  if (a !== projectAspect) {
    projectAspect = a;
    onAspect();
  }
}

const VS = `#version 300 es
out vec2 uv;
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  uv = vec2(p.x, 1.0 - p.y);
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;
const FS = `#version 300 es
precision mediump float;
in vec2 uv;
uniform sampler2D t;
out vec4 c;
void main() { c = texture(t, uv); }`;

function program(gl) {
  const shader = (type, src) => {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    return s;
  };
  const p = gl.createProgram();
  gl.attachShader(p, shader(gl.VERTEX_SHADER, VS));
  gl.attachShader(p, shader(gl.FRAGMENT_SHADER, FS));
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
  return p;
}

/** Draw `api` preview frames in `viewer`; returns a function that stops. */
function start(viewer, api) {
  const canvas = document.createElement("canvas");
  canvas.className = "viewer-canvas";
  canvas.dataset.testid = "viewer-canvas";
  canvas.dataset.seq = "0";
  // Behind the viewer's overlays (transport bar, debug text), which are
  // later siblings; the viewer is positioned by transport.css.
  Object.assign(canvas.style, {
    position: "absolute",
    inset: "0",
    width: "100%",
    height: "100%",
    zIndex: "-1",
    display: "block",
  });
  viewer.style.isolation = "isolate";
  const gl = canvas.getContext("webgl2", { alpha: false, antialias: false, depth: false });
  if (!gl) {
    console.warn("viewer-webgl: WebGL2 is not available; preview off");
    return () => {};
  }
  viewer.prepend(canvas);
  const prog = program(gl);
  const tex = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, tex);
  for (const [k, v] of [
    [gl.TEXTURE_MIN_FILTER, gl.LINEAR],
    [gl.TEXTURE_MAG_FILTER, gl.LINEAR],
    [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE],
    [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE],
  ]) {
    gl.texParameteri(gl.TEXTURE_2D, k, v);
  }
  gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);

  let seq = 0;
  let frame = null; // { w, h } of the texture
  let inflight = false;
  let retryAt = 0;
  let dirty = true;
  let raf = 0;
  let stopped = false;
  onAspect = () => (dirty = true);

  function draw() {
    dirty = false;
    gl.viewport(0, 0, canvas.width, canvas.height);
    gl.clearColor(0, 0, 0, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    if (!frame) return;
    const r = letterbox(canvas.width, canvas.height, projectAspect ?? frame.w / frame.h);
    // GL's viewport origin is bottom-left.
    gl.viewport(r.x, canvas.height - r.y - r.h, r.w, r.h);
    gl.useProgram(prog);
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  }

  async function fetchFrame() {
    inflight = true;
    try {
      const res = await fetch(api.frameUrl(seq));
      if (res.status === 200) {
        const w = Number(res.headers.get("X-Frame-Width"));
        const h = Number(res.headers.get("X-Frame-Height"));
        const s = Number(res.headers.get("X-Frame-Seq"));
        const data = new Uint8Array(await res.arrayBuffer());
        // An answer older than what is shown (or malformed) is dropped.
        if (!stopped && s > seq && w > 0 && h > 0 && data.length === w * h * 4) {
          gl.bindTexture(gl.TEXTURE_2D, tex);
          gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, data);
          frame = { w, h };
          seq = s;
          canvas.dataset.seq = String(s);
          dirty = true;
        }
      } else if (res.status !== 204) {
        throw new Error(`HTTP ${res.status}`);
      }
    } catch (e) {
      if (!retryAt) console.warn("viewer-webgl: frame fetch failed", e);
      retryAt = performance.now() + RETRY_MS;
    } finally {
      inflight = false;
    }
  }

  function tick(now) {
    raf = requestAnimationFrame(tick);
    const dpr = window.devicePixelRatio || 1;
    const w = Math.max(1, Math.round(viewer.clientWidth * dpr));
    const h = Math.max(1, Math.round(viewer.clientHeight * dpr));
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
      dirty = true;
    }
    if (!inflight && now >= retryAt) fetchFrame();
    if (dirty) draw();
  }
  raf = requestAnimationFrame(tick);

  // For tests: draw, then read one canvas pixel (CSS px from the canvas's
  // top-left) as [r, g, b, a]. Reading in the same task as the draw works
  // without preserveDrawingBuffer.
  canvas.kvPixel = (x, y) => {
    draw();
    const dpr = canvas.width / (canvas.clientWidth || 1);
    const px = new Uint8Array(4);
    const gx = Math.min(canvas.width - 1, Math.floor(x * dpr));
    const gy = Math.min(canvas.height - 1, Math.floor(y * dpr));
    gl.readPixels(gx, canvas.height - 1 - gy, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px);
    return Array.from(px);
  };

  return () => {
    stopped = true;
    cancelAnimationFrame(raf);
    canvas.remove();
  };
}

/**
 * Start the WebGL viewer in `viewer` if the backend says "webgl" (`api`:
 * getPreviewBackend and frameUrl from api.js). Resolves to a stop function.
 */
export async function initViewerWebGl(viewer, api) {
  if (!viewer) return () => {};
  let backend;
  try {
    backend = await api.getPreviewBackend();
  } catch (e) {
    console.warn("get_preview_backend failed", e);
    return () => {};
  }
  return backend === "webgl" ? start(viewer, api) : () => {};
}
