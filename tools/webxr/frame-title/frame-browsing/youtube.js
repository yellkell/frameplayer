// YouTube on the Steam Frame.
//
// 1. VP9 instead of AV1. Chromium XR decodes both in software on the Frame
//    (its video hardware has no AV1, and VP9 there is unsafe; see patch
//    0018). VP9 keeps up with 4K60, AV1 doesn't; YouTube still prefers AV1
//    (ads, 8K, many 360° videos), so report AV1 unsupported on YouTube.
//
// 2. 360° and VR180 videos in VR. YouTube has no WebXR mode on desktop. On a
//    spherical video, a VR button in the player's controls opens the VR view
//    of youtube-vr.js, which draws the page's own <video> around you with
//    FramePlayer's controls; YouTube's player keeps playing it.
(() => {
  // --- 1. VP9 instead of AV1 ---------------------------------------------
  const isAv1 = (type) => /av01/i.test(String(type));
  for (const MS of [globalThis.MediaSource, globalThis.ManagedMediaSource]) {
    if (!MS || !MS.isTypeSupported) continue;
    const isTypeSupported = MS.isTypeSupported;
    MS.isTypeSupported = function (type) {
      return isAv1(type) ? false : isTypeSupported.call(this, type);
    };
  }
  if (globalThis.MediaCapabilities && MediaCapabilities.prototype.decodingInfo) {
    const decodingInfo = MediaCapabilities.prototype.decodingInfo;
    MediaCapabilities.prototype.decodingInfo = function (config) {
      if (config && config.video && isAv1(config.video.contentType)) {
        return Promise.resolve({ supported: false, smooth: false, powerEfficient: false });
      }
      return decodingInfo.call(this, config);
    };
  }
  const canPlayType = HTMLMediaElement.prototype.canPlayType;
  HTMLMediaElement.prototype.canPlayType = function (type) {
    return isAv1(type) ? '' : canPlayType.call(this, type);
  };

  if (window !== window.top) return;

  // --- 2. Spherical videos in VR ------------------------------------------
  if (!navigator.xr) return;

  let button = null;

  // The current video's format (a youtube-vr.js FORMATS key), from the
  // player's own response; null if flat. Desktop YouTube marks 360° and
  // VR180 videos "MESH": 360° comes as an equi-angular cubemap, VR180
  // (partialSpherical) as one eye, about 180°x135° equirectangular. Older
  // 360° uploads are equirectangular.
  function spherical() {
    const player = document.querySelector('#movie_player');
    const response = player && player.getPlayerResponse && player.getPlayerResponse();
    if (!response || !response.streamingData) return null;
    const formats = (response.streamingData.adaptiveFormats || []).filter((f) => f.projectionType);
    const projection = formats.length ? formats[0].projectionType : 'RECTANGULAR';
    if (projection === 'RECTANGULAR') return null;
    const vr = response.playerConfig && response.playerConfig.vrConfig;
    if (vr && vr.partialSpherical) return 'yt180';
    return projection === 'MESH' ? 'eac360' : 'erp360';
  }

  function update() {
    const controls = document.querySelector('#movie_player .ytp-right-controls');
    if (!spherical() || !controls) {
      if (button) button.remove();
      return;
    }
    if (!button) {
      button = document.createElement('button');
      button.className = 'ytp-button frame-vr-button';
      button.title = 'View in VR';
      button.textContent = 'VR';
      button.style.cssText = 'font: 700 15px/1 Roboto, Arial, sans-serif; color: #fff; ' +
        'width: auto; padding: 0 10px; vertical-align: top;';
      button.addEventListener('click', (e) => {
        e.stopPropagation();
        const player = document.querySelector('#movie_player');
        const video = player && player.querySelector('video');
        if (video && spherical() && window.__frameYoutubeVr) window.__frameYoutubeVr.enter(player, video, spherical);
      });
    }
    if (button.parentNode !== controls) controls.prepend(button);
  }

  // YouTube is a single-page app: check again after each navigation and
  // whenever the player swaps videos.
  document.addEventListener('yt-navigate-finish', update);
  document.addEventListener('yt-player-updated', update);
  setInterval(update, 2000);
})();
