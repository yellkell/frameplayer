// Thumbstick scrolling for Chromium XR on the Steam Frame: while the laser
// points at a page, pushing a thumbstick scrolls whatever is under it (the
// page, or a scrollable box in it), faster the further it's pushed.
//
// The laser reaches Chromium as a pen; the sticks as Steam's virtual Xbox
// pad. Which of the pad's two sticks carries the controller you point with
// depends on Steam (it changed between sessions), so either one scrolls.
(() => {
  const DEAD_ZONE = 0.25;
  const SPEED = 1600;          // pixels per second at full deflection
  const POINTING_WINDOW = 500; // ms since the pointer last moved

  let x = -1, y = -1, movedAt = -Infinity, frame = 0, last = 0;

  addEventListener('pointermove', (e) => {
    x = e.clientX;
    y = e.clientY;
    movedAt = performance.now();
    if (!frame) {
      last = 0;
      frame = requestAnimationFrame(tick);
    }
  }, {capture: true, passive: true});
  document.addEventListener('pointerleave', () => { x = y = -1; }, {passive: true});

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

  function tick(t) {
    frame = 0;
    // Laser elsewhere (no pointer movement), page hidden, or a WebXR game
    // running: stop polling until the pointer moves again.
    if (x < 0 || document.hidden || document.documentElement.dataset.frameXr ||
        performance.now() - movedAt > POINTING_WINDOW) {
      return;
    }
    const dt = last ? Math.min(0.05, (t - last) / 1000) : 0;
    last = t;
    const [sx, sy] = stick();
    const dx = curve(sx) * SPEED * dt, dy = curve(sy) * SPEED * dt;
    if (dx || dy) {
      const target = document.elementFromPoint(x, y);
      // The frame under the pointer scrolls itself (its own copy of this script).
      if (target && target.tagName !== 'IFRAME' && target.tagName !== 'FRAME') {
        scrollable(target)?.scrollBy({left: dx, top: dy, behavior: 'instant'});
      }
    }
    frame = requestAnimationFrame(tick);
  }
})();
