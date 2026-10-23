// YouTube on the Steam Frame.
//
// 1. VP9 instead of AV1. Chromium XR decodes both in software on the Frame
//    (its video hardware has no AV1, and VP9 there is unsafe; see patch
//    0018). VP9 keeps up with 4K60, AV1 doesn't; YouTube still prefers AV1
//    (ads, 8K, many 360° videos), so report AV1 unsupported on YouTube.
//
// 2. VR180 in 3D. Browsers get VR180 as one eye ("vlayout=mono" streams);
//    YouTube's visionOS client gets the same itags side by side. On a VR180
//    video, ask the player API as visionOS and give the page's player those
//    streams and their streaming session, so YouTube's own player plays 3D.
//
// 3. 360° and VR180 videos in VR. YouTube has no WebXR mode on desktop. On a
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
  if (!navigator.xr) return;

  // --- 2. VR180 in 3D -------------------------------------------------------
  const stereoIds = new Set(); // videos whose streams were swapped for 3D
  const isVr180 = (r) => !!(r && r.streamingData && r.playerConfig && r.playerConfig.vrConfig &&
    r.playerConfig.vrConfig.partialSpherical);

  function visionosRequest(videoId) {
    let visitor = window.ytcfg && window.ytcfg.get ? window.ytcfg.get('VISITOR_DATA') : '';
    if (!visitor) {
      const m = document.documentElement.innerHTML.match(/"VISITOR_DATA":"([^"]+)"/);
      visitor = m ? m[1] : '';
    }
    const headers = { 'Content-Type': 'application/json', 'X-YouTube-Client-Name': '101', 'X-YouTube-Client-Version': '1.02' };
    if (visitor) headers['X-Goog-Visitor-Id'] = visitor;
    const body = JSON.stringify({
      videoId, contentCheckOk: true, racyCheckOk: true,
      context: { client: { clientName: 'VISIONOS', clientVersion: '1.02', deviceMake: 'Apple', deviceModel: 'RealityDevice17,1',
        osName: 'visionOS', osVersion: '26.5.23O471', hl: 'en', visitorData: visitor } },
    });
    return { url: '/youtubei/v1/player?prettyPrint=false', headers, body };
  }

  // Puts the visionOS response's streams and session into the page's one.
  function toStereo(response, visionos) {
    const sd = visionos && visionos.streamingData;
    const config = visionos && visionos.playerConfig && visionos.playerConfig.mediaCommonConfig &&
      visionos.playerConfig.mediaCommonConfig.mediaUstreamerRequestConfig;
    if (!sd || !sd.serverAbrStreamingUrl || !config || !response.playerConfig.mediaCommonConfig) return;
    const stereo = new Map((sd.adaptiveFormats || []).filter((f) => f.projectionType === 'MESH').map((f) => [f.itag, f]));
    if (!stereo.size) return;
    response.streamingData.adaptiveFormats = (response.streamingData.adaptiveFormats || []).flatMap((f) => {
      if (f.projectionType !== 'MESH') return [f];
      const s = stereo.get(f.itag);
      if (!s) return [];
      for (const k of ['lastModified', 'xtags', 'contentLength', 'initRange', 'indexRange', 'approxDurationMs', 'width', 'height']) {
        if (s[k] !== undefined) f[k] = s[k];
      }
      return [f];
    });
    response.streamingData.serverAbrStreamingUrl = sd.serverAbrStreamingUrl;
    response.playerConfig.mediaCommonConfig.mediaUstreamerRequestConfig = config;
    stereoIds.add(response.videoDetails.videoId);
    meshUrls.set(response.videoDetails.videoId, new Map([...stereo].filter(([, f]) => f.url).map(([itag, f]) => [itag, f.url])));
  }

  // Each stream's projection mesh, which the page's stream lacks: VR180
  // streams aren't all equirectangular (YouTube's 4K ones are, the lower
  // ones put more pixels in the middle), and the mesh says which. Read from
  // the start of the visionOS stream, as YouTube's player reads it.
  const meshUrls = new Map(); // videoId -> itag -> visionOS stream URL
  const meshes = new Map(); // `${videoId}:${itag}` -> Promise of tables or null

  const find = (bytes, seq, from = 0) => {
    outer: for (let i = from; i + seq.length <= bytes.length; i++) {
      for (let j = 0; j < seq.length; j++) if (bytes[i + j] !== seq[j]) continue outer;
      return i;
    }
    return -1;
  };

  // The mshp payload: from an MP4's mshp (YouTube: ytmp) box, or a WebM's
  // Projection > ProjectionPrivate element.
  function meshPayload(bytes) {
    for (const box of ['mshp', 'ytmp']) {
      const i = find(bytes, [...box].map((c) => c.charCodeAt(0)));
      if (i >= 4) return bytes.subarray(i + 4, i - 4 + new DataView(bytes.buffer, bytes.byteOffset).getUint32(i - 4));
    }
    const projection = find(bytes, [0x76, 0x70]);
    const i = projection < 0 ? -1 : find(bytes, [0x76, 0x72], projection);
    if (i < 0) return null;
    // EBML size: a variable-length integer, its length in the leading zeros.
    const length = 9 - Math.max(1, bytes[i + 2].toString(2).length);
    let size = bytes[i + 2] & (0xff >> length);
    for (let k = 1; k < length; k++) size = size * 256 + bytes[i + 2 + k];
    return bytes.subarray(i + 2 + length, i + 2 + length + size);
  }

  // Spherical Video V2 mesh projection: version and flags, CRC, encoding,
  // then 'mesh' boxes, each coordinates and delta-coded vertex indices.
  // Returns angle -> texture coordinate tables, if the mesh is one
  // (YouTube's VR180 meshes are: each u is one longitude, each v one
  // latitude).
  async function parseMesh(bytes) {
    const payload = meshPayload(bytes);
    if (!payload || payload.length < 12) return null;
    const encoding = String.fromCharCode(...payload.subarray(8, 12));
    let data = payload.subarray(12);
    if (encoding === 'dfl8') {
      const stream = new Blob([data]).stream().pipeThrough(new DecompressionStream('deflate-raw'));
      data = new Uint8Array(await new Response(stream).arrayBuffer());
    } else if (encoding !== 'raw ') {
      return null;
    }
    // The first mesh (both eyes' are the same for YouTube's VR180).
    if (data.length < 16 || String.fromCharCode(...data.subarray(4, 8)) !== 'mesh') return null;
    const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
    const coordCount = view.getUint32(8) & 0x7fffffff;
    const coords = new Float32Array(coordCount);
    for (let i = 0; i < coordCount; i++) coords[i] = view.getFloat32(12 + 4 * i);
    let at = 12 + 4 * coordCount;
    const vertexCount = view.getUint32(at) & 0x7fffffff;
    at = (at + 4) * 8; // bits from here
    const read = (n) => {
      let v = 0;
      for (let i = 0; i < n; i++, at++) v = v * 2 + ((data[at >> 3] >> (7 - (at & 7))) & 1);
      return v;
    };
    const width = Math.ceil(Math.log2(coordCount * 2));
    const index = [0, 0, 0, 0, 0];
    const lonAt = new Map(), latAt = new Map();
    for (let i = 0; i < vertexCount; i++) {
      for (let k = 0; k < 5; k++) {
        const z = read(width);
        index[k] += z % 2 ? -(z + 1) / 2 : z / 2;
      }
      const [x, y, z, u, v] = index.map((n) => coords[n]);
      const r = Math.hypot(x, y, z) || 1;
      const lat = (Math.asin(y / r) * 180) / Math.PI;
      if (Math.abs(lat) < 89.9) lonAt.set(u, (Math.atan2(x, -z) * 180) / Math.PI);
      latAt.set(v, lat);
    }
    // Angle (whole degrees, -90..90) -> texture coordinate, -1 outside.
    const table = (pairs) => {
      const s = [...pairs].map(([t, deg]) => [deg, t]).sort((a, c) => a[0] - c[0]);
      const out = new Float32Array(181).fill(-1);
      for (let d = -90, j = 0; d <= 90; d++) {
        while (j + 1 < s.length && s[j + 1][0] < d) j++;
        if (j + 1 >= s.length || d < s[0][0]) continue;
        const [d0, t0] = s[j], [d1, t1] = s[j + 1];
        if (d > d1) continue;
        out[d + 90] = d1 > d0 ? t0 + ((t1 - t0) * (d - d0)) / (d1 - d0) : t0;
      }
      return out;
    };
    return { u: table(lonAt), v: table(latAt) };
  }

  window.__frameYoutubeMesh = (videoId, itag) => {
    const key = `${videoId}:${itag}`;
    if (!meshes.has(key)) {
      const url = meshUrls.get(videoId) && meshUrls.get(videoId).get(itag);
      meshes.set(key, !url ? Promise.resolve(null) : fetch(`${url}&range=0-16383`)
        .then((r) => (r.ok ? r.arrayBuffer() : null))
        .then((buffer) => (buffer ? parseMesh(new Uint8Array(buffer)) : null))
        .catch(() => null));
    }
    return meshes.get(key);
  };

  // The first video comes in the page (ytInitialPlayerResponse): the player
  // reads it straight away, so ask synchronously.
  let initial;
  Object.defineProperty(window, 'ytInitialPlayerResponse', {
    configurable: true,
    get: () => initial,
    set: (response) => {
      initial = response;
      if (!isVr180(response)) return;
      try {
        const req = visionosRequest(response.videoDetails.videoId);
        const xhr = new XMLHttpRequest();
        xhr.open('POST', req.url, false);
        for (const [k, v] of Object.entries(req.headers)) xhr.setRequestHeader(k, v);
        xhr.send(req.body);
        if (xhr.status === 200) toStereo(response, JSON.parse(xhr.responseText));
      } catch (e) {}
    },
  });

  // Player responses inside an API reply (player is one, get_watch holds
  // one among the page's other data).
  function playerResponses(data, depth = 0, found = []) {
    if (!data || typeof data !== 'object' || depth > 4) return found;
    if (data.streamingData && data.videoDetails) found.push(data);
    else for (const v of Object.values(data)) playerResponses(v, depth + 1, found);
    return found;
  }

  // Later videos (in-page navigation) come from the player and get_watch
  // APIs.
  const fetch = window.fetch;
  window.fetch = async function (input, init) {
    const response = await fetch.apply(this, arguments);
    const url = typeof input === 'string' ? input : (input && input.url) || '';
    if (!/\/youtubei\/v1\/(player|get_watch)\b/.test(url) || !response.ok) return response;
    try {
      const data = await response.clone().json();
      let swapped = false;
      for (const r of playerResponses(data).filter(isVr180)) {
        const req = visionosRequest(r.videoDetails.videoId);
        const visionos = await fetch(req.url, { method: 'POST', headers: req.headers, body: req.body, credentials: 'include' });
        if (!visionos.ok) continue;
        toStereo(r, await visionos.json());
        swapped = swapped || stereoIds.has(r.videoDetails.videoId);
      }
      if (!swapped) return response;
      return new Response(JSON.stringify(data), { status: response.status, statusText: response.statusText, headers: response.headers });
    } catch (e) {
      return response;
    }
  };

  // --- 3. Spherical videos in VR ------------------------------------------
  let button = null;

  // The current video's format (a youtube-vr.js FORMATS key), from the
  // player's own response; null if flat. Desktop YouTube marks 360° and
  // VR180 videos "MESH": 360° comes as an equi-angular cubemap, VR180
  // (partialSpherical) as one eye, 180°x180° equirectangular, or side by
  // side once swapped for 3D. Older 360° uploads are equirectangular.
  function spherical() {
    const player = document.querySelector('#movie_player');
    const response = player && player.getPlayerResponse && player.getPlayerResponse();
    if (!response || !response.streamingData) return null;
    const formats = (response.streamingData.adaptiveFormats || []).filter((f) => f.projectionType);
    const projection = formats.length ? formats[0].projectionType : 'RECTANGULAR';
    if (projection === 'RECTANGULAR') return null;
    const vr = response.playerConfig && response.playerConfig.vrConfig;
    if (vr && vr.partialSpherical) return stereoIds.has(response.videoDetails.videoId) ? 'yt180sbs' : 'yt180';
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
