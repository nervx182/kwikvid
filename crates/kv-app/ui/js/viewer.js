// Keeps the native preview layer (ADR 0007) aligned with the viewer element.
// The viewer is transparent; Rust draws the video underneath it. Its
// rectangle is sent on load, resize, layout change and fullscreen, at most
// once per animation frame and only when it changed.

/** The rectangle to report for an element: CSS px plus devicePixelRatio. */
export function viewerRect(el, scale) {
  const r = el.getBoundingClientRect();
  return { x: r.left, y: r.top, w: r.width, h: r.height, scale };
}

function sameRect(a, b) {
  return (
    a !== null &&
    a.x === b.x &&
    a.y === b.y &&
    a.w === b.w &&
    a.h === b.h &&
    a.scale === b.scale
  );
}

/**
 * Watch `el` and call send(rect) whenever its rectangle changes. Returns a
 * function that stops watching.
 */
export function initViewer(el, send) {
  if (!el) return () => {};
  let last = null;
  let pending = 0;
  const flush = () => {
    pending = 0;
    const rect = viewerRect(el, window.devicePixelRatio || 1);
    if (sameRect(last, rect)) return;
    last = rect;
    Promise.resolve(send(rect)).catch((e) => console.warn("set_viewer_rect failed", e));
  };
  const schedule = () => {
    if (!pending) pending = requestAnimationFrame(flush);
  };
  // Size changes of the viewer itself, and of the whole layout (a pane
  // resizing moves the viewer without necessarily resizing it).
  const observer = new ResizeObserver(schedule);
  observer.observe(el);
  observer.observe(document.documentElement);
  const events = ["resize", "fullscreenchange", "scroll"];
  for (const ev of events) window.addEventListener(ev, schedule, true);
  // devicePixelRatio changes when the window moves to another display.
  let dpr = null;
  const watchDpr = () => {
    dpr = matchMedia(`(resolution: ${window.devicePixelRatio || 1}dppx)`);
    dpr.addEventListener("change", onDpr, { once: true });
  };
  const onDpr = () => {
    schedule();
    watchDpr();
  };
  watchDpr();
  schedule();
  return () => {
    observer.disconnect();
    for (const ev of events) window.removeEventListener(ev, schedule, true);
    dpr.removeEventListener("change", onDpr);
    if (pending) cancelAnimationFrame(pending);
  };
}
