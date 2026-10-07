// Thumbstick scrolling for Chromium XR on the Steam Frame: pushing a
// thumbstick scrolls the page, faster the further it's pushed. While the
// laser points at the page, what's under it scrolls (a list or box in the
// page, or the page); otherwise what was under it last, or the page.
//
// The laser reaches Chromium as a pen; the sticks as Steam's virtual Xbox
// pad, which only carries the controller that isn't pointing (Steam keeps the
// pointing one for its laser), so either of the pad's sticks scrolls.
(() => {
  const DEAD_ZONE = 0.25;
  const SPEED = 1600;          // pixels per second at full deflection
  const POINTING_WINDOW = 500; // ms since the pointer last moved over this frame
  const isTop = window === window.top;

  let x = -1, y = -1, movedAt = -Infinity, frame = 0, last = 0;

  addEventListener('pointermove', (e) => {
    x = e.clientX;
    y = e.clientY;
    movedAt = performance.now();
    start();
  }, {capture: true, passive: true});
  // The top frame scrolls even when nothing points at the page, so it polls
  // whenever the page is showing; frames inside it only while pointed at.
  addEventListener('gamepadconnected', start);
  document.addEventListener('visibilitychange', start);
  start();

  function start() {
    if (!frame && !document.hidden) {
      last = 0;
      frame = requestAnimationFrame(tick);
    }
  }

  // Strongest deflection over both sticks of every gamepad.
  function stick() {
    let sx = 0, sy = 0;
    for (const pad of navigator.getGamepads()) {
      if (!pad) continue;
      for (let i = 0; i + 1 < pad.axes.length; i += 2) {
        if (Math.abs(pad.axes[i]) > Math.abs(sx)) sx = pad.axes[i];
        if (Math.abs(pad.axes[i + 1]) > Math.abs(sy)) sy = pad.axes[i + 1];
      }
    }
    return [sx, sy];
  }

  function curve(v) {
    const a = Math.abs(v);
    return a < DEAD_ZONE ? 0 : Math.sign(v) * ((a - DEAD_ZONE) / (1 - DEAD_ZONE)) ** 2;
  }

  function scrollable(el) {
    for (; el && el !== document.documentElement; el = el.parentElement || el.getRootNode().host) {
      const s = getComputedStyle(el);
      if ((el.scrollHeight > el.clientHeight && /auto|scroll|overlay/.test(s.overflowY)) ||
          (el.scrollWidth > el.clientWidth && /auto|scroll|overlay/.test(s.overflowX))) {
        return el;
      }
    }
    return document.scrollingElement;
  }

  // What to scroll: under the laser while it points here; else (top frame
  // only) what was under it last, or the page. Over a frame inside the page,
  // nothing: that frame scrolls itself while pointed at.
  function target() {
    const el = x >= 0 ? document.elementFromPoint(x, y) : null;
    if (el && (el.tagName === 'IFRAME' || el.tagName === 'FRAME')) return null;
    return el || document.scrollingElement;
  }

  function tick(t) {
    frame = 0;
    if (document.hidden) return;  // visibilitychange starts it again
    const pointing = performance.now() - movedAt < POINTING_WINDOW;
    if (!pointing && !isTop) return;  // frames inside the page wait for the laser
    const dt = last ? Math.min(0.05, (t - last) / 1000) : 0;
    last = t;
    const [sx, sy] = stick();
    const dx = curve(sx) * SPEED * dt, dy = curve(sy) * SPEED * dt;
    // During a WebXR game, or on a page that reads the gamepad itself (a game
    // played with the controller), the sticks are the page's.
    const own = document.documentElement.dataset;
    if ((dx || dy) && !own.frameXr && !own.frameGamepad) {
      const el = target();
      if (el) scrollable(el)?.scrollBy({left: dx, top: dy, behavior: 'instant'});
    }
    frame = requestAnimationFrame(tick);
  }
})();
