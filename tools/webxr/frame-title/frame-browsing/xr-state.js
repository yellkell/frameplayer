// Marks <html data-frame-xr> while the page has an immersive WebXR session,
// so scroll.js (in the extension's own world) leaves the sticks to the game.
(() => {
  const xr = globalThis.XRSystem && XRSystem.prototype;
  if (!xr || !xr.requestSession) return;
  const requestSession = xr.requestSession;
  xr.requestSession = async function (mode, ...rest) {
    const session = await requestSession.call(this, mode, ...rest);
    if (mode !== 'inline') {
      document.documentElement.dataset.frameXr = '1';
      session.addEventListener('end', () => delete document.documentElement.dataset.frameXr);
    }
    return session;
  };
})();
