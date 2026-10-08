// Runs in the page's own world and tells scroll.js (in the extension's
// world) when to leave the sticks alone, through attributes on <html>:
// - data-frame-xr while the page has an immersive WebXR session;
// - data-frame-gamepad once the page reads gamepads itself (a game played
//   with the controller).
(() => {
  const root = document.documentElement;
  const nav = globalThis.Navigator && Navigator.prototype;
  if (nav && nav.getGamepads) {
    const getGamepads = nav.getGamepads;
    nav.getGamepads = function (...args) {
      root.dataset.frameGamepad = '1';
      return getGamepads.apply(this, args);
    };
  }
  const xr = globalThis.XRSystem && XRSystem.prototype;
  if (!xr || !xr.requestSession) return;
  const requestSession = xr.requestSession;
  xr.requestSession = async function (mode, ...rest) {
    const session = await requestSession.call(this, mode, ...rest);
    if (mode !== 'inline') {
      root.dataset.frameXr = '1';
      session.addEventListener('end', () => delete root.dataset.frameXr);
    }
    return session;
  };
})();
