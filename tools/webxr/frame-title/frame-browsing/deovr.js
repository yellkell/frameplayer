// DeoVR's web player (deovr.com) on the Steam Frame.
//
// It only opens its VR player for browsers it recognises as a headset. Its
// checks treat a Linux Chrome that supports WebXR and reports exactly two
// touch points as Galaxy XR, and switch the player on; this browser reports
// none, so DeoVR offered "open in the DeoVR app" instead. Report two, on
// deovr.com only.
Object.defineProperty(Navigator.prototype, "maxTouchPoints", { get: () => 2, configurable: true });

// As Galaxy XR it asks for hand tracking and draws generic hands from it. The
// Frame has no camera hand tracking (SteamVR derives the joints from the
// controllers' finger sensors), so leave hand tracking out: the player then
// shows controllers, which frame-models makes the Steam Frame's own.
if (window.XRSystem && XRSystem.prototype.requestSession) {
  const requestSession = XRSystem.prototype.requestSession;
  XRSystem.prototype.requestSession = function (mode, init) {
    if (init) {
      const drop = (list) => Array.isArray(list) ? list.filter((f) => f !== "hand-tracking") : list;
      init = { ...init, requiredFeatures: drop(init.requiredFeatures), optionalFeatures: drop(init.optionalFeatures) };
    }
    return requestSession.call(this, mode, init);
  };
}
