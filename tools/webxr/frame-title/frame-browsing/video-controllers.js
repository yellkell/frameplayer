// VR video players on the Steam Frame: controllers, not generic hands.
//
// The Frame has no camera hand tracking: SteamVR derives hand joints from
// the controllers' finger sensors. Video players that are offered hand
// tracking draw generic hands from those joints instead of the controllers
// you're holding. When a page that has a <video> starts a VR session, leave
// optional hand tracking out, so the player shows controllers (frame-models
// makes them the Steam Frame's own). Pages without video (games) and pages
// that require hand tracking keep it.
if (window.XRSystem && XRSystem.prototype.requestSession) {
  const requestSession = XRSystem.prototype.requestSession;
  XRSystem.prototype.requestSession = function (mode, init) {
    if (init && Array.isArray(init.optionalFeatures) && document.querySelector("video")) {
      init = { ...init, optionalFeatures: init.optionalFeatures.filter((f) => f !== "hand-tracking") };
    }
    return requestSession.call(this, mode, init);
  };
}
