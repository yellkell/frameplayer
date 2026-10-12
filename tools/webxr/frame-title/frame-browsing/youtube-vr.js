// YouTube's 360° and VR180 videos in VR, looking and working like
// FramePlayer (crates/frameplayer/src/ui): its control bar and adjust panel,
// its controller layout (docs/USER-GUIDE.md), its picture adjustments.
// youtube.js adds the VR button and calls window.__frameYoutubeVr.enter().
//
// The VR view draws the page's own <video> around you, one ray per pixel as
// FramePlayer's scene shader (crates/fp-render/shaders/scene.wgsl). Desktop
// YouTube sends one view only (no 3D): 360° videos as an equi-angular
// cubemap (3x2), VR180 as about 180°x135° equirectangular.
//
// The panels are canvases drawn the way FramePlayer's widgets draw (raised
// keys, sunken trays, the accent play dome), with FramePlayer's fonts:
// Inter (OFL) and Phosphor icons (MIT), from fonts/ (youtube-fonts.js says
// where they are).
(() => {
  if (window !== window.top || !navigator.xr) return;

  // --- Look (ui/theme.rs, ui/widgets.rs) ---------------------------------
  const C = {
    text: 'rgb(236,240,244)', text2: 'rgb(150,160,174)', text3: 'rgb(104,114,128)',
    bg: 'rgb(13,16,21)', accent: 'rgb(26,159,255)', accentHover: 'rgb(77,182,255)',
    well: 'rgb(10,13,17)', keyFgOn: 'rgb(125,200,255)',
  };
  const KEY = {
    top: [58, 69, 83], bottom: [35, 42, 51], topHover: [72, 84, 98], bottomHover: [45, 54, 65],
    topOn: [31, 76, 114], bottomOn: [21, 52, 82],
  };
  const ICON = {
    play: '\u{E3D0}', pause: '\u{E39E}', rewind: '\u{E038}', forward: '\u{E036}',
    speakerHigh: '\u{E44A}', speakerLow: '\u{E44C}', speakerX: '\u{E45C}', x: '\u{E4F6}',
    hide: '\u{E224}', recenter: '\u{E1D6}', adjust: '\u{E434}', check: '\u{E182}',
    reset: '\u{E038}', wand: '\u{E6B6}', format: '\u{E626}', position: '\u{E0A4}', picture: '\u{E472}',
  };
  const INTER = 'FPInter, Inter, Roboto, sans-serif';
  const font = (weight, size) => `${weight} ${size}px ${INTER}`;
  const iconFont = (size) => `${size}px FPPhosphor`;
  const rgb = (c, a = 1) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;
  const mix = (a, b, t) => a.map((x, i) => Math.round(x + (b[i] - x) * t));
  const clamp = (x, lo, hi) => Math.min(hi, Math.max(lo, x));

  let fontsLoaded = null;
  function loadFonts() {
    const base = document.documentElement.dataset.frameFonts;
    if (fontsLoaded || !base) return fontsLoaded || Promise.resolve();
    const faces = [
      ['FPInter', 'Inter-Regular.ttf', '400'], ['FPInter', 'Inter-Medium.ttf', '500'],
      ['FPInter', 'Inter-SemiBold.ttf', '600'], ['FPInter', 'Inter-Bold.ttf', '700'],
      ['FPPhosphor', 'Phosphor.ttf', '400'],
    ].map(([family, file, weight]) => new FontFace(family, `url(${base}${file})`, { weight }));
    fontsLoaded = Promise.all(faces.map((f) => f.load().then(() => document.fonts.add(f)).catch(() => {})));
    return fontsLoaded;
  }

  // --- Drawing (ui/depth.rs) ----------------------------------------------
  function path(g, x, y, w, h, r) {
    r = Math.max(0, Math.min(r, w / 2, h / 2));
    g.beginPath();
    g.roundRect(x, y, w, h, r);
  }

  // A raised rounded rectangle: shadow (and glow), gradient body, lit edge.
  function raised(g, x, y, w, h, r, look) {
    const lift = clamp(look.lift, 0, 1.5);
    g.save();
    if (lift > 0) {
      g.shadowColor = `rgba(0,0,0,${0.51 * Math.min(lift, 1)})`;
      g.shadowBlur = 4 + 6 * lift;
      g.shadowOffsetY = 2 + 2 * lift;
      path(g, x, y, w, h, r);
      g.fillStyle = rgb(mix(look.top, look.bottom, 0.6));
      g.fill();
    }
    if (look.glow) {
      g.shadowColor = look.glow;
      g.shadowBlur = 18;
      g.shadowOffsetY = 4;
      path(g, x, y, w, h, r);
      g.fill();
    }
    g.restore();
    const grad = g.createLinearGradient(0, y, 0, y + h);
    grad.addColorStop(0, rgb(look.top));
    grad.addColorStop(1, rgb(look.bottom));
    path(g, x, y, w, h, r);
    g.fillStyle = grad;
    g.fill();
    // Lit top edge.
    const edge = g.createLinearGradient(0, y, 0, y + Math.min(h, 2 * r + 4) * 0.6);
    edge.addColorStop(0, look.light);
    edge.addColorStop(1, 'rgba(255,255,255,0)');
    path(g, x + 0.75, y + 0.75, w - 1.5, h - 1.5, r - 0.75);
    g.strokeStyle = edge;
    g.lineWidth = 1.5;
    g.stroke();
  }

  // A sunken rounded rectangle (tray, track): dark fill, shade inside its
  // top edge, a faint lit lip below.
  function sunken(g, x, y, w, h, r, fill) {
    path(g, x, y, w, h, r);
    g.fillStyle = fill;
    g.fill();
    g.save();
    path(g, x, y, w, h, r);
    g.clip();
    const shade = g.createLinearGradient(0, y, 0, y + Math.min(5, h / 2));
    shade.addColorStop(0, 'rgba(0,0,0,0.75)');
    shade.addColorStop(1, 'rgba(0,0,0,0)');
    g.fillStyle = shade;
    g.fillRect(x, y, w, Math.min(5, h / 2));
    g.restore();
    path(g, x + 1, y + 1, w - 2, h, r);
    g.strokeStyle = 'rgba(255,255,255,0.05)';
    g.lineWidth = 1;
    g.stroke();
  }

  function railFill(g, x, y, w, h) {
    if (w <= 0.5) return;
    g.save();
    g.shadowColor = 'rgba(26,159,255,0.3)';
    g.shadowBlur = 10;
    const grad = g.createLinearGradient(0, y, 0, y + h);
    grad.addColorStop(0, 'rgb(98,196,255)');
    grad.addColorStop(1, 'rgb(22,132,226)');
    path(g, x, y, Math.max(w, h), h, h / 2);
    g.fillStyle = grad;
    g.fill();
    g.restore();
  }

  function knob(g, cx, cy, r) {
    raised(g, cx - r, cy - r, 2 * r, 2 * r, r,
      { top: [255, 255, 255], bottom: [208, 215, 224], light: 'rgba(255,255,255,1)', lift: 0.8 });
  }

  function playMark(g, cx, cy, size, color) {
    const h = size, w = h * 0.88, left = cx - w / 3;
    g.beginPath();
    g.moveTo(left, cy - h / 2);
    g.lineTo(left + w, cy);
    g.lineTo(left, cy + h / 2);
    g.closePath();
    g.fillStyle = color;
    g.strokeStyle = color;
    g.lineWidth = 2;
    g.lineJoin = 'round';
    g.fill();
    g.stroke();
  }

  function pauseMark(g, cx, cy, size, color) {
    const bw = size * 0.28, gap = size * 0.22;
    g.fillStyle = color;
    for (const dx of [-(gap / 2 + bw / 2), gap / 2 + bw / 2]) {
      path(g, cx + dx - bw / 2, cy - size / 2, bw, size, bw * 0.3);
      g.fill();
    }
  }

  function text(g, s, x, y, f, color, align = 'left', baseline = 'middle') {
    g.font = f;
    g.fillStyle = color;
    g.textAlign = align;
    g.textBaseline = baseline;
    g.fillText(s, x, y);
    return g.measureText(s).width;
  }

  function measure(g, s, f) {
    g.font = f;
    return g.measureText(s).width;
  }

  // --- Panels -------------------------------------------------------------
  // A panel is a canvas (FramePlayer points × ppp pixels, with room round
  // its body for the shadow) shown as a quad in VR. Layout is immediate:
  // each draw registers hit areas; the pointer finds them on the next.
  const ROOM = { left: 20, right: 20, top: 8, bottom: 28 }; // widgets::SHADOW_ROOM
  const PPP = 1.25; // FramePlayer's pixels per point, for its panel sizes
  const RENDER = 2; // canvas pixels per point: supersampled, then mipmapped

  class Panel {
    constructor(gl, body, widthM) {
      this.body = body; // points
      this.full = [body[0] + ROOM.left + ROOM.right, body[1] + ROOM.top + ROOM.bottom];
      this.size = [widthM * this.full[0] / body[0], 0];
      this.size[1] = this.size[0] * this.full[1] / this.full[0];
      this.canvas = document.createElement('canvas');
      this.canvas.width = Math.round(this.full[0] * RENDER);
      this.canvas.height = Math.round(this.full[1] * RENDER);
      this.g = this.canvas.getContext('2d');
      this.gl = gl;
      this.texture = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, this.texture);
      for (const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.LINEAR_MIPMAP_LINEAR], [gl.TEXTURE_MAG_FILTER, gl.LINEAR],
        [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]]) gl.texParameteri(gl.TEXTURE_2D, k, v);
      // The bar is seen at a slant: anisotropic filtering keeps it sharp.
      const aniso = gl.getExtension('EXT_texture_filter_anisotropic');
      if (aniso) {
        gl.texParameterf(gl.TEXTURE_2D, aniso.TEXTURE_MAX_ANISOTROPY_EXT,
          Math.min(8, gl.getParameter(aniso.MAX_TEXTURE_MAX_ANISOTROPY_EXT)));
      }
      this.areas = [];
      this.pointer = null; // { x, y } in points
      this.active = null; // id pressed
      this.model = null;
      this.visible = false;
    }

    begin() {
      const g = this.g;
      g.setTransform(1, 0, 0, 1, 0, 0);
      g.clearRect(0, 0, this.canvas.width, this.canvas.height);
      g.setTransform(RENDER, 0, 0, RENDER, ROOM.left * RENDER, ROOM.top * RENDER);
      this.areas = [];
      return g;
    }

    end() {
      const gl = this.gl;
      gl.bindTexture(gl.TEXTURE_2D, this.texture);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, this.canvas);
      gl.generateMipmap(gl.TEXTURE_2D);
    }

    // Registers an area; returns how the pointer is on it.
    area(id, x, y, w, h, handlers = {}) {
      this.areas.push({ id, x, y, w, h, ...handlers });
      const p = this.pointer;
      const over = !!p && p.x >= x && p.x <= x + w && p.y >= y && p.y <= y + h;
      return { hover: over && (!this.active || this.active === id), down: this.active === id && over, active: this.active === id };
    }

    hit(p) {
      for (let i = this.areas.length - 1; i >= 0; i--) {
        const a = this.areas[i];
        if (p.x >= a.x && p.x <= a.x + a.w && p.y >= a.y && p.y <= a.y + a.h) return a;
      }
      return null;
    }

    // The slab every panel sits on (widgets::panel_slab).
    slab(radius) {
      const g = this.g, [w, h] = this.body;
      g.save();
      g.shadowColor = 'rgba(0,0,0,0.55)';
      g.shadowBlur = 30;
      g.shadowOffsetY = 12;
      path(g, 0, 0, w, h, radius);
      g.fillStyle = 'rgb(20,26,33)';
      g.fill();
      g.restore();
      raised(g, 0, 0, w, h, radius, { top: [29, 36, 46], bottom: [14, 18, 24], light: 'rgba(255,255,255,0.12)', lift: 0 });
    }

    // Where a ray meets the panel, in points, or null.
    intersect(origin, dir) {
      if (!this.visible || !this.model) return null;
      const inv = invert(this.model);
      const o = transform(inv, origin, 1), d = transform(inv, dir, 0);
      if (d[2] >= -1e-6) return null;
      const t = -o[2] / d[2];
      if (t <= 0) return null;
      const x = o[0] + t * d[0], y = o[1] + t * d[1];
      const [w, h] = this.size;
      if (Math.abs(x) > w / 2 || Math.abs(y) > h / 2) return null;
      return {
        x: (x / w + 0.5) * this.full[0] - ROOM.left,
        y: (0.5 - y / h) * this.full[1] - ROOM.top,
        distance: t,
      };
    }
  }

  // --- Widgets (ui/widgets.rs) --------------------------------------------
  function keyFace(g, x, y, size, state, selected, radius = Infinity) {
    const t = state.hover ? 1 : 0, s = selected ? 1 : 0, down = state.down;
    const dy = down ? 0.5 : -1.5 * t;
    const fx = x + 3, fy = y + 3 + dy, fw = size[0] - 6, fh = size[1] - 6;
    const shade = (c) => (down ? mix(c, [0, 0, 0], 0.15) : c);
    raised(g, fx, fy, fw, fh, Math.min(radius, fh / 2), {
      top: shade(mix(mix(KEY.top, KEY.topHover, t), KEY.topOn, s)),
      bottom: shade(mix(mix(KEY.bottom, KEY.bottomHover, t), KEY.bottomOn, s)),
      light: s ? 'rgba(125,200,255,0.4)' : `rgba(255,255,255,${(70 + 30 * t) / 255})`,
      lift: down ? 0.2 : 0.6 + 0.6 * t,
      glow: s ? 'rgba(26,159,255,0.35)' : null,
    });
    return { cx: fx + fw / 2, cy: fy + fh / 2, fg: s ? C.keyFgOn : t ? '#fff' : C.text };
  }

  // Primary (accent) button face.
  function primaryFace(g, x, y, w, h, state, radius, enabled) {
    const t = state.hover && enabled ? 1 : 0, down = state.down && enabled;
    const dy = down ? 0.5 : -1.5 * t;
    raised(g, x + 3, y + 3 + dy, w - 6, h - 6, radius, {
      top: mix([92, 190, 255], [128, 208, 255], t), bottom: mix([16, 112, 196], [24, 132, 224], t),
      light: 'rgba(255,255,255,0.5)', lift: down ? 0.2 : 0.7 + 0.5 * t,
      glow: `rgba(26,159,255,${0.3 + 0.2 * t})`,
    });
    return { cx: x + w / 2, cy: y + h / 2 + dy };
  }

  // A tooltip above a widget, drawn last (see cursor()).
  function tip(panel, label, cx, top) {
    panel.tipLater = [label, cx, top];
  }

  function drawTip(panel, label, cx, top) {
    const g = panel.g, f = font(600, 15), w = measure(g, label, f) + 16, h = 26;
    const x = clamp(cx - w / 2, 0, panel.body[0] - w);
    path(g, x, top - h - 8, w, h, 8);
    g.fillStyle = C.text;
    g.fill();
    text(g, label, x + w / 2, top - h / 2 - 8, f, C.bg, 'center');
  }

  // A round key with an icon; returns true when clicked.
  function key(panel, id, x, y, size, icon, { selected = false, tipText, onClick } = {}) {
    const st = panel.area(id, x, y, size, size, { onClick });
    const face = keyFace(panel.g, x, y, [size, size], st, selected);
    text(panel.g, icon, face.cx, face.cy, iconFont(size * 0.46), face.fg, 'center');
    if (st.hover && tipText) tip(panel, tipText, x + size / 2, y);
  }

  function slider(panel, id, x, y, w, value, lo, hi, onChange) {
    const inset = 11, x0 = x + inset, x1 = x + w - inset;
    const toValue = (p) => lo + clamp((p.x - x0) / (x1 - x0), 0, 1) * (hi - lo);
    const st = panel.area(id, x, y, w, 36, { onDrag: (p) => onChange(toValue(p)) });
    const t = st.hover || st.active ? 1 : 0;
    const f = hi > lo ? clamp((value - lo) / (hi - lo), 0, 1) : 0;
    const h = 7 + 2 * t, cy = y + 18, kx = x0 + (x1 - x0) * f;
    sunken(panel.g, x0, cy - h / 2, x1 - x0, h, h / 2, C.well);
    railFill(panel.g, x0, cy - h / 2, kx - x0, h);
    knob(panel.g, kx, cy, 9 + 2 * t);
  }

  function button(panel, id, x, y, label, icon, kind, { enabled = true, onClick } = {}) {
    const g = panel.g, h = 48, f = font(600, 18), fi = iconFont(22);
    const gap = 10, tw = measure(g, label, f), iw = icon ? measure(g, icon, fi) + gap : 0;
    const w = Math.max(tw + iw + 2 * 20, h);
    const st = panel.area(id, x, y, w, h, enabled ? { onClick } : {});
    let cx, cy, fg;
    g.globalAlpha = enabled ? 1 : 0.5;
    if (kind === 'primary') {
      ({ cx, cy } = primaryFace(g, x, y, w, h, st, 12, enabled));
      fg = '#fff';
    } else {
      ({ cx, cy, fg } = keyFace(g, x, y, [w, h], st, false, 12));
    }
    let tx = cx - (tw + iw) / 2;
    if (icon) {
      text(g, icon, tx, cy, fi, fg);
      tx += iw;
    }
    text(g, label, tx, cy, f, fg);
    g.globalAlpha = 1;
    return w;
  }

  function chip(panel, id, x, y, label, icon, selected, onClick) {
    const g = panel.g, h = 40, f = font(500, 16), fi = iconFont(19);
    const tw = measure(g, label, f), iw = icon ? measure(g, icon, fi) + 8 : 0;
    const w = Math.max(tw + iw + 32, h);
    const st = panel.area(id, x, y, w, h, { onClick });
    const face = keyFace(g, x, y, [w, h], st, selected, h / 2);
    let tx = face.cx - (tw + iw) / 2;
    if (icon) {
      text(g, icon, tx, face.cy, fi, face.fg);
      tx += iw;
    }
    text(g, label, tx, face.cy, f, face.fg);
    return w;
  }

  function sectionLabel(g, s, x, y) {
    g.letterSpacing = '1.4px';
    text(g, s.toUpperCase(), x, y, font(600, 13), C.text3, 'left', 'top');
    g.letterSpacing = '0px';
  }

  function clock(seconds) {
    seconds = Math.max(0, Math.floor(seconds || 0));
    const h = Math.floor(seconds / 3600), m = Math.floor(seconds / 60) % 60, s = seconds % 60;
    const ss = String(s).padStart(2, '0');
    return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
  }

  // --- Video formats --------------------------------------------------------
  const EAC = 0, EQUIRECT = 1;
  const FORMATS = {
    eac360: { label: 'EAC 360° (YouTube)', short: 'EAC 360°', layout: EAC, hFov: 360, vFov: 180 },
    erp360: { label: '360° 2D', short: '360°', layout: EQUIRECT, hFov: 360, vFov: 180 },
    erp180: { label: '180° 2D', short: '180°', layout: EQUIRECT, hFov: 180, vFov: 180 },
    yt180: { label: 'VR180 (YouTube)', short: 'VR180', layout: EQUIRECT, hFov: 180, vFov: 135 },
  };
  const DEFAULTS = {
    format: null, yaw: 0, pitch: 0, roll: 0, zoom: 1, height: 0, forward: 0,
    brightness: 0, contrast: 1, saturation: 1, gamma: 1,
  };
  const PICTURE_KEYS = ['yaw', 'pitch', 'roll', 'zoom', 'height', 'forward'];
  const PANELS_KEY = 'frame-vr-panels'; // where you moved the panels
  const SPEEDS = [0.5, 0.75, 1, 1.25, 1.5, 2];

  // --- Shaders --------------------------------------------------------------
  const SPHERE_VERT = `#version 300 es
    out vec2 ndc;
    void main() {
      ndc = vec2(gl_VertexID == 1 ? 3.0 : -1.0, gl_VertexID == 2 ? 3.0 : -1.0);
      gl_Position = vec4(ndc, 0.0, 1.0);
    }`;
  // One ray per pixel: the view direction picks the texel.
  const SPHERE_FRAG = `#version 300 es
    precision highp float;
    in vec2 ndc;
    out vec4 color;
    uniform sampler2D video;
    uniform mat4 invProj, camToWorld;
    uniform int mode;
    uniform vec2 fov; // radians
    uniform float yaw, pitch, roll, zoom;
    uniform vec3 viewer; // where you are in the sphere (height, forward), m
    uniform vec4 picture; // brightness, contrast, saturation, gamma
    const float PI = 3.14159265;
    const float RADIUS = 3.0; // the sphere the video is on when you move

    // From the reference space into the video's (yaw, pitch, roll).
    vec3 toVideo(vec3 d) {
      float c = cos(yaw), s = sin(yaw);
      d = vec3(c * d.x + s * d.z, d.y, -s * d.x + c * d.z);
      c = cos(pitch); s = sin(pitch);
      d = vec3(d.x, c * d.y + s * d.z, -s * d.y + c * d.z);
      c = cos(roll); s = sin(roll);
      return vec3(c * d.x - s * d.y, s * d.x + c * d.y, d.z);
    }

    // Seen from off the centre, near parts of the picture grow and slide
    // against far ones (FramePlayer's off_centre).
    vec3 offCentre(vec3 d, vec3 p) {
      if (dot(p, p) < 1e-8) return d;
      vec3 q = p * min(1.0, 0.9 * RADIUS / length(p));
      float b = dot(q, d);
      float t = -b + sqrt(b * b - dot(q, q) + RADIUS * RADIUS);
      return normalize(q + t * d);
    }

    // Angle-preserving zoom: scales the angle from the forward axis.
    vec3 zoomDir(vec3 d) {
      if (abs(zoom - 1.0) < 1e-4) return d;
      float r = length(d.xy);
      if (r < 1e-6) return d;
      float t2 = min(acos(clamp(-d.z, -1.0, 1.0)) / zoom, PI);
      return vec3(d.xy / r * sin(t2), -cos(t2));
    }

    vec2 eacFace(float a, float b) {
      return vec2(atan(a), atan(b)) * (2.0 / PI) + 0.5;
    }

    // YouTube's equi-angular cubemap: top row left|front|right, bottom row
    // down|back|up with the bottom faces rotated.
    vec2 eacUv(vec3 d) {
      vec3 m = abs(d);
      int face; float a, b;
      if (m.z >= m.x && m.z >= m.y) {
        if (d.z < 0.0) { face = 1; a = d.x / m.z; b = -d.y / m.z; }
        else { face = 4; a = -d.x / m.z; b = -d.y / m.z; }
      } else if (m.x >= m.y) {
        if (d.x > 0.0) { face = 2; a = d.z / m.x; b = -d.y / m.x; }
        else { face = 0; a = -d.z / m.x; b = -d.y / m.x; }
      } else {
        if (d.y > 0.0) { face = 5; a = d.x / m.y; b = d.z / m.y; }
        else { face = 3; a = d.x / m.y; b = -d.z / m.y; }
      }
      vec2 f = eacFace(a, b);
      float col = float(face % 3), row = float(face / 3);
      if (face == 3 || face == 5) f = vec2(1.0 - f.y, 1.0 - f.x);
      else if (face == 4) f = vec2(1.0 - f.y, f.x);
      return vec2((col + f.x) / 3.0, (row + f.y) / 2.0);
    }

    void main() {
      vec4 p = invProj * vec4(ndc, 1.0, 1.0);
      vec3 d = toVideo(normalize(mat3(camToWorld) * normalize(p.xyz / p.w)));
      d = zoomDir(offCentre(d, toVideo(viewer)));
      vec2 uv;
      if (mode == ${EAC}) {
        uv = eacUv(d);
      } else {
        float lon = atan(d.x, -d.z), lat = asin(clamp(d.y, -1.0, 1.0));
        uv = vec2(0.5 + lon / fov.x, 0.5 - lat / fov.y);
        if (any(lessThan(uv, vec2(0.0))) || any(greaterThan(uv, vec2(1.0)))) {
          color = vec4(0.0, 0.0, 0.0, 1.0);
          return;
        }
      }
      // Picture adjustments in the video's (gamma) space, as FramePlayer.
      vec3 g = texture(video, uv).rgb;
      g = (g - 0.5) * picture.y + 0.5 + picture.x;
      float luma = dot(g, vec3(0.2126, 0.7152, 0.0722));
      g = mix(vec3(luma), g, picture.z);
      g = pow(max(g, vec3(0.0)), vec3(1.0 / max(picture.w, 0.05)));
      color = vec4(clamp(g, 0.0, 1.0), 1.0);
    }`;
  const UI_VERT = `#version 300 es
    in vec3 position;
    in vec2 uv;
    uniform mat4 viewProj, model;
    out vec2 texCoord;
    void main() {
      texCoord = uv;
      gl_Position = viewProj * model * vec4(position, 1.0);
    }`;
  const UI_FRAG = `#version 300 es
    precision mediump float;
    in vec2 texCoord;
    uniform sampler2D panel;
    uniform vec4 tint;
    uniform bool textured;
    out vec4 color;
    void main() {
      color = textured ? texture(panel, texCoord) : tint;
    }`;

  function compile(gl, vert, frag) {
    const program = gl.createProgram();
    for (const [type, src] of [[gl.VERTEX_SHADER, vert], [gl.FRAGMENT_SHADER, frag]]) {
      const shader = gl.createShader(type);
      gl.shaderSource(shader, src);
      gl.compileShader(shader);
      if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) console.error(gl.getShaderInfoLog(shader));
      gl.attachShader(program, shader);
    }
    gl.linkProgram(program);
    const uniform = {};
    const count = gl.getProgramParameter(program, gl.ACTIVE_UNIFORMS);
    for (let i = 0; i < count; i++) {
      const name = gl.getActiveUniform(program, i).name;
      uniform[name] = gl.getUniformLocation(program, name);
    }
    return { program, uniform };
  }

  // --- The VR view ----------------------------------------------------------
  let session = null;

  async function enter(player, video, detect) {
    if (session) return;
    session = await navigator.xr.requestSession('immersive-vr');
    const fontsReady = loadFonts();
    const canvas = document.createElement('canvas');
    const gl = canvas.getContext('webgl2', { xrCompatible: true, antialias: false, depth: false });
    session.updateRenderState({ baseLayer: new XRWebGLLayer(session, gl, { depth: false, antialias: false }) });
    const space = await session.requestReferenceSpace('local');

    const sphere = compile(gl, SPHERE_VERT, SPHERE_FRAG);
    const ui = compile(gl, UI_VERT, UI_FRAG);
    const texture = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, texture);
    for (const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.LINEAR], [gl.TEXTURE_MAG_FILTER, gl.LINEAR],
      [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]]) gl.texParameteri(gl.TEXTURE_2D, k, v);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 1, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array([0, 0, 0, 255]));

    // Panels: FramePlayer's control bar and adjust panel (app.rs: 1400x268
    // and 900x1020 pixels at 1.25 pixels per point).
    const bar = new Panel(gl, [1400 / PPP, 268 / PPP], 1.2);
    const adjust = new Panel(gl, [900 / PPP, 1020 / PPP], 0.75);
    const panels = [bar, adjust];
    const quads = new Map();
    for (const panel of panels) {
      const [w, h] = panel.size.map((s) => s / 2);
      const buffer = gl.createBuffer();
      gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
      gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([
        -w, -h, 0, 0, 1, w, -h, 0, 1, 1, -w, h, 0, 0, 0, w, h, 0, 1, 0]), gl.STATIC_DRAW);
      quads.set(panel, buffer);
    }
    const lines = gl.createBuffer();
    const attrib = { position: gl.getAttribLocation(ui.program, 'position'), uv: gl.getAttribLocation(ui.program, 'uv') };
    const bindAttribs = (buffer) => {
      gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
      gl.enableVertexAttribArray(attrib.position);
      gl.vertexAttribPointer(attrib.position, 3, gl.FLOAT, false, 20, 0);
      gl.enableVertexAttribArray(attrib.uv);
      gl.vertexAttribPointer(attrib.uv, 2, gl.FLOAT, false, 20, 12);
    };

    // State.
    let videoId = player.getVideoData().video_id;
    let detected = detect(); // a FORMATS key
    let settings, saved;
    const storeKey = () => `frame-vr:${videoId}`;
    function loadSettings() {
      try {
        saved = { ...DEFAULTS, ...JSON.parse(localStorage.getItem(storeKey()) || '{}') };
      } catch (e) {
        saved = { ...DEFAULTS };
      }
      settings = { ...saved };
    }
    loadSettings();
    const dirty = () => Object.keys(DEFAULTS).some((k) => settings[k] !== saved[k]);
    const format = () => FORMATS[settings.format] || FORMATS[detected];

    let anchorYaw = 0, anchorAt = [0, 0, 0], centred = false;
    let uiVisible = true, adjustOpen = false, adjustTab = 0, usedAt = 0;
    let uploadedFrames = -1, uploadedTime = -1, checkedAt = 0, lastTime = 0, drawnAt = 0, drawnLook = '';
    let unmuteVolume = 100, bothGrips = false, scrub = null;
    let drag = null; // grip + trigger: { source, yaw, pitch, from }
    // Where you moved the panels: yaw / pitch offsets in degrees about you,
    // kept across sessions (FramePlayer's panel_offsets).
    let offsets = { bar: [0, 0], adjust: [0, 0] };
    try {
      offsets = { ...offsets, ...JSON.parse(localStorage.getItem(PANELS_KEY) || '{}') };
    } catch (e) {}
    let grab = null; // grip on a panel: { source, name, yaw, pitch, from }
    const held = new Map(); // input source -> buttons pressed, seek repeat
    const pointers = new Map(); // input source -> { panel, hit } this frame
    const rays = []; // lasers: [origin, end]

    // Where the panels hang: the head's position and yaw when recentred.
    function recentre(pose) {
      const m = pose.transform.matrix;
      anchorYaw = Math.atan2(m[8], m[10]);
      anchorAt = [m[12], m[13], m[14]];
    }
    function anchor() {
      return multiply(translation(anchorAt), rotationY(anchorYaw));
    }
    // world.rs place() / place_around().
    const place = (a, yawDeg, distance, offsetY, tiltDeg) =>
      multiply(multiply(multiply(a, rotationY(rad(yawDeg))), translation([0, offsetY, -distance])), rotationX(rad(-tiltDeg)));
    const placeAround = (a, yawDeg, pitchDeg, distance, offsetY) =>
      multiply(multiply(multiply(a, rotationY(rad(yawDeg))), rotationX(rad(pitchDeg))), translation([0, offsetY, -distance]));

    const togglePlay = () => (video.paused ? player.playVideo() : player.pauseVideo());
    const seek = (seconds) => player.seekTo(Math.max(0, player.getCurrentTime() + seconds), true);
    const toggleMute = () => {
      if (player.isMuted() || player.getVolume() === 0) {
        player.unMute();
        if (player.getVolume() === 0) player.setVolume(unmuteVolume || 100);
      } else {
        unmuteVolume = player.getVolume();
        player.mute();
      }
    };
    const showUi = (on, time) => {
      uiVisible = on;
      usedAt = time;
    };

    // --- The control bar (ui/player.rs control_bar) ---
    function drawBar() {
      const p = bar, g = p.begin();
      p.slab(28);
      const W = p.body[0], m = { x: 28, y: 18 };
      const duration = player.getDuration() || 0;
      const position = scrub !== null ? scrub : player.getCurrentTime();
      const f = format();
      // Title and format.
      // Rows as FramePlayer lays them out (measured from its ui-preview).
      const TITLE_Y = 26.6, SEEK_Y = 75.6, ROW_Y = 132.8;
      const pillLabel = f.short, pf = font(600, 14);
      const pw = measure(g, pillLabel, pf) + 20, ph = 27;
      const px = W - m.x - pw, py = TITLE_Y + 15 - ph / 2;
      const pst = p.area('format', px, py, pw, ph, { onClick: () => { const showing = adjustOpen && adjustTab === 0; adjustOpen = !showing; adjustTab = 0; } });
      const t = pst.hover ? 1 : 0;
      raised(g, px, py, pw, ph, 8, { top: mix([46, 54, 65], [62, 72, 86], t), bottom: mix([33, 40, 49], [44, 52, 63], t), light: 'rgba(255,255,255,0.1)', lift: 0.3 });
      text(g, pillLabel, px + pw / 2, py + ph / 2, pf, t ? C.text : C.text2, 'center');
      if (pst.hover) tip(p, 'Change the format', px + pw / 2, py);
      g.save();
      g.beginPath();
      g.rect(m.x, TITLE_Y, px - m.x - 16, 30);
      g.clip();
      text(g, player.getVideoData().title || '', m.x, TITLE_Y + 15, font(600, 21), C.text);
      g.restore();
      // Times either side of the seek bar.
      const rowY = SEEK_Y;
      const lt = text(g, clock(position), m.x, rowY + 18, font(500, 16), C.text2);
      const sx = m.x + lt + 14, sw = W - m.x - 68 - 14 - sx;
      text(g, clock(duration), sx + sw + 14, rowY + 18, font(500, 16), C.text2);
      const toTime = (pt) => clamp((pt.x - sx) / sw, 0, 1) * duration;
      const sst = p.area('seek', sx, rowY, sw, 36, {
        onDrag: (pt) => { scrub = toTime(pt); },
        onRelease: () => { if (scrub !== null) player.seekTo(scrub, true); scrub = null; },
      });
      const active = sst.hover || sst.active ? 1 : 0;
      const th = 8 + 4 * active, ty = rowY + 18 - th / 2;
      sunken(g, sx, ty, sw, th, th / 2, C.well);
      const frac = duration ? clamp(position / duration, 0, 1) : 0;
      if (sst.hover && !sst.active && p.pointer) {
        const hx = clamp(p.pointer.x, sx, sx + sw);
        if (hx > sx + sw * frac) {
          path(g, sx + sw * frac, ty, hx - (sx + sw * frac), th, th / 2);
          g.fillStyle = 'rgba(255,255,255,0.16)';
          g.fill();
        }
      }
      railFill(g, sx, ty, sw * frac, th);
      knob(g, sx + sw * frac, rowY + 18, 8 + 4 * active);
      if ((sst.hover || sst.active) && p.pointer && duration) {
        const label = clock(scrub !== null ? scrub : toTime(p.pointer)), lf = font(600, 15);
        const bw = measure(g, label, lf) + 16, bh = 26;
        const bx = clamp(clamp(p.pointer.x, sx, sx + sw) - bw / 2, 0, W - bw);
        path(g, bx, ty - bh - 10, bw, bh, 8);
        g.fillStyle = C.text;
        g.fill();
        text(g, label, bx + bw / 2, ty - bh / 2 - 10, lf, C.bg, 'center');
      }
      // Controls: three groups on one row, transport centred.
      const row = { y: ROW_Y, h: 72 };
      const cy = row.y + row.h / 2;
      // Left well: speed, mute, volume.
      const speed = player.getPlaybackRate();
      const speedLabel = `${String(+speed.toFixed(2))}×`, sf = font(600, 17);
      const spw = Math.max(measure(g, speedLabel, sf) + 34, 52);
      const lw = 5 + spw + 4 + 52 + 4 + 130 + 6 + 5, lh = row.h;
      sunken(g, m.x, cy - lh / 2, lw, lh, lh / 2, C.well);
      let x = m.x + 5;
      const kst = p.area('speed', x, cy - 23, spw, 46, { onClick: () => player.setPlaybackRate(SPEEDS.find((s) => s > speed + 1e-3) || SPEEDS[0]) });
      const sface = keyFace(g, x, cy - 23, [spw, 46], kst, Math.abs(speed - 1) > 1e-3);
      text(g, speedLabel, sface.cx, sface.cy, sf, sface.fg, 'center');
      if (kst.hover) tip(p, 'Speed', x + spw / 2, cy - 23);
      x += spw + 4;
      const muted = player.isMuted() || player.getVolume() === 0;
      const vol = muted ? 0 : player.getVolume();
      key(p, 'mute', x, cy - 26, 52, vol === 0 ? ICON.speakerX : vol < 50 ? ICON.speakerLow : ICON.speakerHigh,
        { tipText: muted ? 'Unmute' : 'Mute', onClick: toggleMute });
      x += 52 + 4;
      slider(p, 'volume', x, cy - 18, 130, vol, 0, 100, (v) => {
        player.setVolume(Math.round(v));
        if (v > 0 && player.isMuted()) player.unMute();
      });
      // Centre: back, play / pause, forward.
      const cw = 56 + 16 + 72 + 16 + 56;
      x = W / 2 - cw / 2;
      skip(p, 'rewind', x, cy - 28, -10);
      x += 56 + 16;
      const playSt = p.area('play', x, cy - 36, 72, 72, { onClick: togglePlay });
      const pt = playSt.hover ? 1 : 0, down = playSt.down;
      const r = 34 + 2 * pt - (down ? 2 : 0), pcx = x + 36, pcy = cy + (down ? 0.5 : -pt);
      raised(g, pcx - r, pcy - r, 2 * r, 2 * r, r, {
        top: mix([104, 198, 255], [140, 214, 255], pt), bottom: mix([14, 106, 190], [22, 128, 222], pt),
        light: 'rgba(255,255,255,0.59)', lift: down ? 0.3 : 0.9, glow: `rgba(26,159,255,${0.45 + 0.25 * pt})`,
      });
      if (video.paused) playMark(g, pcx, pcy, 28, '#fff');
      else pauseMark(g, pcx, pcy, 26, '#fff');
      if (playSt.hover) tip(p, video.paused ? 'Play' : 'Pause', pcx, cy - 36);
      x += 72 + 16;
      skip(p, 'forward', x, cy - 28, 10);
      // Right well: adjust, recenter, hide, close.
      const keys = [
        ['adjust', ICON.adjust, 'Adjust', () => { adjustOpen = !adjustOpen; }, adjustOpen],
        ['recenter', ICON.recenter, 'Recenter', () => { recentreNext = true; }],
        ['hide', ICON.hide, 'Hide', () => { uiVisible = false; }],
        ['close', ICON.x, 'Close', () => session.end()],
      ];
      const rw = 5 + keys.length * 52 + (keys.length - 1) * 4 + 5;
      const rx = W - m.x - rw;
      sunken(g, rx, cy - lh / 2, rw, lh, lh / 2, C.well);
      keys.forEach(([id, icon, label, onClick, selected], i) =>
        key(p, id, rx + 5 + i * 56, cy - 26, 52, icon, { selected: !!selected, tipText: label, onClick }));
      cursor(p);
      p.end();
    }

    function skip(p, id, x, y, secs) {
      const st = p.area(id, x, y, 56, 56, { onClick: () => seek(secs) });
      const face = keyFace(p.g, x, y, [56, 56], st, false);
      text(p.g, secs < 0 ? ICON.rewind : ICON.forward, face.cx, face.cy, iconFont(32), face.fg, 'center');
      text(p.g, String(Math.abs(secs)), face.cx, face.cy + 1, font(700, 10), face.fg, 'center');
      if (st.hover) tip(p, secs < 0 ? 'Rewind' : 'Forward', x + 28, y);
    }

    function cursor(p) {
      if (p.tipLater) drawTip(p, ...p.tipLater);
      p.tipLater = null;
      if (!p.pointer) return;
      p.g.beginPath();
      p.g.arc(p.pointer.x, p.pointer.y, 6, 0, 2 * Math.PI);
      p.g.fillStyle = 'rgba(255,255,255,0.95)';
      p.g.fill();
      p.g.strokeStyle = 'rgba(0,0,0,0.4)';
      p.g.lineWidth = 1.5;
      p.g.stroke();
    }

    // --- The adjust panel (ui/player.rs adjust_panel) ---
    const TABS = [[ICON.format, 'Format'], [ICON.position, 'Position'], [ICON.picture, 'Picture']];
    function drawAdjust() {
      const p = adjust, g = p.begin();
      p.slab(28);
      const W = p.body[0], H = p.body[1], m = 22;
      text(g, 'Adjust', m, m + 16, font(700, 26), C.text);
      // Tabs: a sunken tray of equal segments.
      const ty = 68, th = 66, tw = W - 2 * m;
      sunken(g, m, ty, tw, th, 16, C.well);
      const sw = (tw - 8) / TABS.length;
      TABS.forEach(([icon, label], i) => {
        const sx = m + 4 + i * sw, sy = ty + 4;
        const st = p.area(`tab${i}`, sx, sy, sw, th - 8, { onClick: () => { adjustTab = i; } });
        const t = st.hover ? 1 : 0, s = i === adjustTab ? 1 : 0, lift = Math.max(t * (1 - s), s);
        if (lift > 0.01) {
          raised(g, sx + 2, sy + 2 - t * (1 - s), sw - 4, th - 12, 11, {
            top: mix(KEY.top, KEY.topHover, t), bottom: mix(KEY.bottom, KEY.bottomHover, t),
            light: `rgba(255,255,255,${0.24 * lift})`, lift: st.down ? 0.2 : 0.7 * lift,
          });
        }
        const fg = s ? '#fff' : t ? C.text : C.text2;
        text(g, icon, sx + sw / 2, sy + (th - 8) / 2 - 10, iconFont(22), s ? C.accentHover : fg, 'center');
        text(g, label, sx + sw / 2, sy + (th - 8) / 2 + 14, font(600, 13), fg, 'center');
      });
      let y = ty + th + 10;
      if (adjustTab === 0) y = formatPage(p, m, y, W - 2 * m);
      else if (adjustTab === 1) y = rowsPage(p, m, y, W - 2 * m, [
        ['Yaw', 'yaw', -180, 180, 0, '°', 0], ['Pitch', 'pitch', -90, 90, 0, '°', 0],
        ['Roll', 'roll', -45, 45, 0, '°', 0], ['Zoom', 'zoom', 0.5, 2.5, 1, '×', 2],
        ['Height', 'height', -1, 1, 0, ' m', 2, 'Raise or lower yourself in the scene.'],
        ['Forward', 'forward', -1, 1, 0, ' m', 2, 'Move closer to or back from the scene.'],
      ], 'Or hold a grip and the trigger and drag the picture; press the left thumbstick to undo. ' +
        'Right grip and thumbstick up / down changes your height.');
      else y = rowsPage(p, m, y, W - 2 * m, [
        ['Brightness', 'brightness', -0.5, 0.5, 0, '', 2], ['Contrast', 'contrast', 0.5, 1.5, 1, '', 2],
        ['Saturation', 'saturation', 0, 2, 1, '', 2], ['Gamma', 'gamma', 0.5, 2, 1, '', 2],
      ]);
      // Footer: save and reset.
      const fy = H - 18 - 48;
      const bw = button(p, 'save', m, fy, 'Save for this video', ICON.check, 'primary', {
        enabled: dirty(),
        onClick: () => {
          saved = { ...settings };
          try {
            localStorage.setItem(storeKey(), JSON.stringify(saved));
          } catch (e) {}
        },
      });
      button(p, 'reset', m + bw + 10, fy, 'Reset', ICON.reset, 'secondary', {
        onClick: () => { settings = { ...DEFAULTS, format: settings.format }; },
      });
      cursor(p);
      p.end();
    }

    function formatPage(p, x, y, w) {
      const g = p.g;
      sectionLabel(g, 'Format', x, y + 10);
      text(g, `Detected ${FORMATS[detected].label}`, x, y + 46, font(400, 17), C.text2);
      y += 69;
      let cx = x;
      const items = [['auto', 'Automatic', ICON.wand, settings.format === null, () => { settings.format = null; }]];
      for (const [id, f] of Object.entries(FORMATS)) {
        items.push([id, f.label, null, settings.format === id, () => { settings.format = id; }]);
      }
      for (const [id, label, icon, selected, onClick] of items) {
        const cw = measure(g, label, font(500, 16)) + (icon ? measure(g, icon, iconFont(19)) + 8 : 0) + 32;
        if (cx + cw > x + w) {
          cx = x;
          y += 52;
        }
        cx += chip(p, `fmt-${id}`, cx, y, label, icon, selected, onClick) + 8;
      }
      return y + 52;
    }

    // A raised card of slider rows (widgets::rows / slider_control).
    function rowsPage(p, x, y, w, rows, hint) {
      const g = p.g;
      // Text left of the slider, wrapped (widgets::Rows).
      const textW = w - 20 - 40 - 10 - 74 - 10 - 240 - 12 - 20;
      const wrap = (str, f) => {
        g.font = f;
        const lines = [];
        let line = '';
        for (const word of str.split(' ')) {
          const next = line ? `${line} ${word}` : word;
          if (g.measureText(next).width > textW && line) {
            lines.push(line);
            line = word;
          } else {
            line = next;
          }
        }
        return line ? [...lines, line] : lines;
      };
      const descs = rows.map((r) => (r[7] ? wrap(r[7], font(400, 15)) : []));
      const heights = descs.map((d) => (d.length ? 72 + 19 * d.length - 3 : 72));
      const ch = heights.reduce((a, b) => a + b, 0) + 8;
      raised(g, x, y, w, ch, 14, { top: [30, 37, 47], bottom: [21, 26, 33], light: 'rgba(255,255,255,0.1)', lift: 0.5 });
      let ry = y + 4;
      rows.forEach(([title, keyName, lo, hi, def, suffix, decimals, desc], i) => {
        const rh = heights[i];
        if (i > 0) ry += heights[i - 1];
        if (i > 0) {
          g.fillStyle = 'rgba(0,0,0,0.47)';
          g.fillRect(x + 20, ry - 0.5, w - 40, 1);
          g.fillStyle = 'rgba(255,255,255,0.05)';
          g.fillRect(x + 20, ry + 0.5, w - 40, 1);
        }
        if (desc) {
          const lines = descs[i], top = ry + rh / 2 - (22 + 19 * lines.length) / 2;
          text(g, title, x + 20, top + 11, font(500, 18), C.text);
          lines.forEach((line, j) => text(g, line, x + 20, top + 33 + 19 * j, font(400, 15), C.text2));
        } else {
          text(g, title, x + 20, ry + rh / 2, font(500, 18), C.text);
        }
        const value = settings[keyName];
        let cx = x + w - 20 - 40;
        if (Math.abs(value - def) > 1e-4) {
          const st = p.area(`reset-${keyName}`, cx, ry + rh / 2 - 20, 40, 40, { onClick: () => { settings[keyName] = def; } });
          if (st.hover) {
            g.beginPath();
            g.arc(cx + 20, ry + rh / 2, 19, 0, 2 * Math.PI);
            g.fillStyle = 'rgba(255,255,255,0.08)';
            g.fill();
            tip(p, 'Reset', cx + 20, ry + rh / 2 - 20);
          }
          text(g, ICON.reset, cx + 20, ry + rh / 2, iconFont(20), st.hover ? '#fff' : C.text2, 'center');
        }
        cx -= 10 + 74;
        text(g, `${value.toFixed(decimals)}${suffix}`, cx + 37, ry + rh / 2, font(500, 16), C.text2, 'center');
        cx -= 10 + 240;
        slider(p, `s-${keyName}`, cx, ry + rh / 2 - 18, 240, value, lo, hi, (v) => { settings[keyName] = v; });
      });
      y += ch + 10;
      if (hint) {
        g.font = font(400, 15);
        const words = hint.split(' ');
        let line = '';
        for (const word of words) {
          const next = line ? `${line} ${word}` : word;
          if (g.measureText(next).width > w && line) {
            text(g, line, x, y + 10, font(400, 15), C.text3);
            y += 21;
            line = word;
          } else {
            line = next;
          }
        }
        if (line) text(g, line, x, y + 10, font(400, 15), C.text3);
        y += 21;
      }
      return y;
    }

    // --- Controllers (FramePlayer's layout; slots as patch 0006 lays them
    // out: Touch's 4 A/X and 5 B/Y, then the Frame-only controls from 7. The
    // left gamepad's 4/5 mirror the right controller's X/Y.) ---
    const RIGHT = { a: 4, b: 5, x: 7, y: 8, bumper: 9, menu: 10 };
    const LEFT = { up: 7, down: 8, left: 9, right: 10, bumper: 11, view: 12 };
    let recentreNext = false;

    function controls(frame, time) {
      const dt = lastTime ? Math.min(0.1, (time - lastTime) / 1000) : 0;
      lastTime = time;
      let grips = 0;
      rays.length = 0;
      pointers.clear();
      for (const panel of panels) panel.pointer = null;
      for (const source of session.inputSources) {
        const pad = source.gamepad;
        if (!pad) continue;
        const right = source.handedness === 'right';
        const was = held.get(source) || { buttons: [], dir: 0, repeat: 0, press: null };
        const down = (i) => !!(pad.buttons[i] && pad.buttons[i].pressed);
        const pressed = (i) => down(i) && !was.buttons[i];
        const x = pad.axes[2] || 0, y = pad.axes[3] || 0;
        const grip = down(1);
        if (grip) grips++;

        // Laser: the nearest panel this controller points at.
        const rayPose = frame.getPose(source.targetRaySpace, space);
        let dir = null, hit = null, hitPanel = null;
        if (rayPose) {
          const m = rayPose.transform.matrix;
          const origin = [m[12], m[13], m[14]];
          dir = [-m[8], -m[9], -m[10]];
          for (const panel of panels) {
            const h = panel.intersect(origin, dir);
            if (h && (!hit || h.distance < hit.distance)) {
              hit = h;
              hitPanel = panel;
            }
          }
          if (uiVisible) {
            const len = hit ? hit.distance : 3;
            rays.push([origin, origin.map((o, i) => o + dir[i] * len)]);
          }
        }
        if (hitPanel) {
          hitPanel.pointer = { x: hit.x, y: hit.y };
          pointers.set(source, { panel: hitPanel, hit });
          usedAt = time;
        }

        // Grip on a panel moves it about you; it stays where you leave it.
        const aim = dir && [deg(Math.atan2(-dir[0], -dir[2]) - anchorYaw), deg(Math.asin(clamp(dir[1], -1, 1)))];
        if (pressed(1) && hitPanel && aim && !grab) {
          const name = hitPanel === bar ? 'bar' : 'adjust';
          grab = { source, name, yaw: aim[0], pitch: aim[1], from: offsets[name].slice() };
        }
        if (grab && grab.source === source) {
          if (!grip || !aim) {
            try {
              localStorage.setItem(PANELS_KEY, JSON.stringify(offsets));
            } catch (e) {}
            grab = null;
          } else {
            offsets[grab.name] = [wrapDeg(grab.from[0] + wrapDeg(aim[0] - grab.yaw)), clamp(grab.from[1] + aim[1] - grab.pitch, -45, 45)];
            usedAt = time;
          }
        }
        const grabbing = !!grab && grab.source === source;

        // Trigger: with grip, drag the picture; on a panel, press what's
        // under the pointer; elsewhere, show or hide the controls.
        let press = was.press;
        if (pressed(0) && !grabbing) {
          if (grip && dir) {
            drag = { source, yaw: Math.atan2(-dir[0], -dir[2]), pitch: Math.asin(clamp(dir[1], -1, 1)), from: { ...settings } };
          } else if (hitPanel) {
            const a = hitPanel.hit(hit);
            press = a ? { panel: hitPanel, area: a } : null;
            if (a) {
              hitPanel.active = a.id;
              if (a.onDrag) a.onDrag(hit);
            }
          } else {
            showUi(!uiVisible, time);
          }
        }
        if (press) {
          const a = press.area, panel = press.panel;
          const p = pointers.get(source);
          const pt = p && p.panel === panel ? p.hit : null;
          if (down(0)) {
            // Sliders and the seek bar follow the pointer while held.
            if (a.onDrag && pt) a.onDrag(pt);
          } else {
            // A button acts on release over it.
            if (a.onRelease) a.onRelease(pt);
            else if (a.onClick && pt && (panel.hit(pt) || {}).id === a.id) a.onClick(pt);
            panel.active = null;
            press = null;
          }
        }
        if (drag && drag.source === source) {
          if (!down(0) || !grip || !dir) {
            drag = null;
          } else {
            // The picture follows the hand.
            settings.yaw = wrapDeg(drag.from.yaw + deg(Math.atan2(-dir[0], -dir[2]) - drag.yaw));
            settings.pitch = clamp(drag.from.pitch + deg(Math.asin(clamp(dir[1], -1, 1)) - drag.pitch), -90, 90);
          }
        }

        if (right) {
          if (pressed(RIGHT.a)) togglePlay();
          if (pressed(RIGHT.b) || pressed(RIGHT.menu)) session.end();
          if (pressed(RIGHT.x)) player.previousVideo();
          if (pressed(RIGHT.y)) player.nextVideo();
          if (pressed(RIGHT.bumper)) seek(60);
          if (pressed(3)) toggleMute();
        } else {
          if (pressed(LEFT.down)) togglePlay();
          if (pressed(LEFT.up)) showUi(!uiVisible, time);
          if (pressed(LEFT.left)) {
            adjustOpen = !adjustOpen;
            showUi(true, time);
          }
          if (pressed(LEFT.view)) recentreNext = true;
          if (pressed(LEFT.bumper)) seek(-60);
          if (pressed(3)) {
            // Undo the picture adjustments since the last save.
            for (const k of PICTURE_KEYS) settings[k] = saved[k];
          }
        }
        let stickDir = 0, repeat = was.repeat;
        if (grabbing) {
          // The stick does nothing while you move a panel.
        } else if (!right && grip) {
          // Grip + stick: left / right turns the picture, up / down zooms.
          settings.yaw = wrapDeg(settings.yaw + x * dt * 86);
          settings.zoom = clamp(settings.zoom * Math.exp(-y * dt * 1.2), 0.5, 2.5);
        } else if (right && grip) {
          // Right grip + stick up / down: your height in the scene.
          if (Math.abs(y) > 0.3) settings.height = clamp(settings.height - y * dt * 0.5, -1, 1);
        } else {
          // Left / right seeks 10 s, repeating while held.
          stickDir = x > 0.7 ? 1 : x < -0.7 ? -1 : 0;
          if (stickDir && stickDir !== was.dir) {
            seek(10 * stickDir);
            repeat = time + 500;
          } else if (stickDir && time >= repeat) {
            seek(10 * stickDir);
            repeat = time + 250;
          }
          // Up / down: volume (right), tilt the picture (left).
          if (!stickDir && Math.abs(y) > 0.3) {
            if (right) {
              player.setVolume(Math.round(clamp(player.getVolume() - y * dt * 60, 0, 100)));
              if (player.isMuted()) player.unMute();
            } else {
              settings.pitch = clamp(settings.pitch + y * dt * 57, -90, 90);
            }
          }
        }
        if (stickDir || pad.buttons.some((b) => b.pressed)) usedAt = time;
        held.set(source, { buttons: pad.buttons.map((b) => b.pressed), dir: stickDir, repeat, press });
      }
      // Both grips together recentre.
      if (grips === 2 && !bothGrips && !drag && !grab) recentreNext = true;
      bothGrips = grips === 2;
    }

    const IDENTITY = new Float32Array([1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]);
    function draw(time, frame) {
      session.requestAnimationFrame(draw);
      const pose = frame.getViewerPose(space);
      if (!pose) return;
      if (!centred || recentreNext) {
        recentre(pose);
        centred = true;
        recentreNext = false;
        usedAt = time;
      }
      controls(frame, time);
      // Next / previous video: its settings and projection, or leave on a
      // flat one.
      if (time - checkedAt > 1000) {
        checkedAt = time;
        const id = player.getVideoData().video_id;
        if (id !== videoId) {
          videoId = id;
          detected = detect();
          if (!detected) return session.end();
          loadSettings();
        }
      }
      // While playing, the controls hide once they're left alone.
      const pointing = pointers.size > 0;
      if (uiVisible && !video.paused && !adjustOpen && !pointing && time - usedAt > 4000) uiVisible = false;
      bar.visible = uiVisible;
      adjust.visible = uiVisible && adjustOpen;
      // Placement (app.rs): the bar below the picture, turned with its yaw
      // and pitch; the adjust panel to the right, where it was.
      const a = anchor();
      const barAnchor = multiply(multiply(a, rotationY(rad(settings.yaw + offsets.bar[0]))), rotationX(rad(settings.pitch + offsets.bar[1])));
      bar.model = place(barAnchor, 0, 1.05, -0.42, 28);
      adjust.model = placeAround(a, -48 + offsets.adjust[0], offsets.adjust[1], 1.15, -0.08);

      // Upload only when the decoder has a new frame, not on every VR frame
      // (requestVideoFrameCallback runs at the page's rate, a few per second
      // behind a VR session).
      gl.bindTexture(gl.TEXTURE_2D, texture);
      const frames = video.getVideoPlaybackQuality().totalVideoFrames;
      const seeked = video.paused && video.currentTime !== uploadedTime;
      if (video.readyState >= 2 && (frames !== uploadedFrames || seeked)) {
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, video);
        uploadedFrames = frames;
        uploadedTime = video.currentTime;
      }
      // Redraw the panels when what they show changes (the pointer, a press,
      // the page they're on), and every 200 ms for the time. Not while one
      // is being moved: that only moves its quad.
      const at = (p) => (p.pointer ? `${Math.round(p.pointer.x)},${Math.round(p.pointer.y)}` : '-');
      const look = [at(bar), at(adjust), bar.active, adjust.active, adjustOpen, adjustTab, video.paused, scrub].join('|');
      const changed = look !== drawnLook && !grab;
      if (uiVisible && ((changed && time - drawnAt > 22) || time - drawnAt > 200)) {
        drawBar();
        if (adjust.visible) drawAdjust();
        drawnAt = time;
        drawnLook = look;
      }

      const f = format();
      const layer = session.renderState.baseLayer;
      gl.bindFramebuffer(gl.FRAMEBUFFER, layer.framebuffer);
      const lineData = new Float32Array(rays.flatMap(([p, q]) => [...p, 0, 0, ...q, 0, 0]));
      for (const view of pose.views) {
        const vp = layer.getViewport(view);
        gl.viewport(vp.x, vp.y, vp.width, vp.height);
        gl.disable(gl.BLEND);
        gl.useProgram(sphere.program);
        gl.activeTexture(gl.TEXTURE0);
        gl.bindTexture(gl.TEXTURE_2D, texture);
        const u = sphere.uniform;
        gl.uniform1i(u.video, 0);
        gl.uniform1i(u.mode, f.layout);
        gl.uniform2f(u.fov, rad(f.hFov), rad(f.vFov));
        gl.uniform1f(u.yaw, -anchorYaw - rad(settings.yaw));
        gl.uniform1f(u.pitch, rad(settings.pitch));
        gl.uniform1f(u.roll, rad(settings.roll));
        gl.uniform1f(u.zoom, settings.zoom);
        const vs = Math.sin(anchorYaw), vc = Math.cos(anchorYaw);
        gl.uniform3f(u.viewer, -vs * settings.forward, settings.height, -vc * settings.forward);
        gl.uniform4f(u.picture, settings.brightness, settings.contrast, settings.saturation, settings.gamma);
        gl.uniformMatrix4fv(u.invProj, false, invert(view.projectionMatrix));
        gl.uniformMatrix4fv(u.camToWorld, false, view.transform.matrix);
        gl.drawArrays(gl.TRIANGLES, 0, 3);
        if (!uiVisible) continue;
        gl.enable(gl.BLEND);
        gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
        gl.useProgram(ui.program);
        gl.uniformMatrix4fv(ui.uniform.viewProj, false, multiply(view.projectionMatrix, invert(view.transform.matrix)));
        gl.uniform1i(ui.uniform.panel, 0);
        for (const panel of panels) {
          if (!panel.visible) continue;
          gl.uniformMatrix4fv(ui.uniform.model, false, panel.model);
          gl.uniform1i(ui.uniform.textured, 1);
          gl.bindTexture(gl.TEXTURE_2D, panel.texture);
          bindAttribs(quads.get(panel));
          gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
        }
        if (lineData.length) {
          gl.uniformMatrix4fv(ui.uniform.model, false, IDENTITY);
          gl.uniform1i(ui.uniform.textured, 0);
          gl.uniform4f(ui.uniform.tint, 1, 1, 1, 0.8);
          gl.bindBuffer(gl.ARRAY_BUFFER, lines);
          gl.bufferData(gl.ARRAY_BUFFER, lineData, gl.STREAM_DRAW);
          bindAttribs(lines);
          gl.drawArrays(gl.LINES, 0, lineData.length / 5);
        }
      }
    }

    // Leaving VR (Close, B, the Steam menu) pauses the video: the page
    // isn't in view, and its sound shouldn't carry on.
    session.addEventListener('end', () => {
      session = null;
      player.pauseVideo();
    });
    // Panels redraw once the fonts are in.
    fontsReady.then(() => { drawnAt = 0; });
    session.requestAnimationFrame(draw);
  }

  // --- Maths (column-major 4x4) ---------------------------------------------
  const rad = (d) => (d * Math.PI) / 180;
  const deg = (r) => (r * 180) / Math.PI;
  const wrapDeg = (d) => ((((d + 180) % 360) + 360) % 360) - 180;

  function translation([x, y, z]) {
    return new Float32Array([1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, x, y, z, 1]);
  }
  function rotationY(a) {
    const c = Math.cos(a), s = Math.sin(a);
    return new Float32Array([c, 0, -s, 0, 0, 1, 0, 0, s, 0, c, 0, 0, 0, 0, 1]);
  }
  function rotationX(a) {
    const c = Math.cos(a), s = Math.sin(a);
    return new Float32Array([1, 0, 0, 0, 0, c, s, 0, 0, -s, c, 0, 0, 0, 0, 1]);
  }
  function transform(m, v, w) {
    return [0, 1, 2].map((i) => m[i] * v[0] + m[4 + i] * v[1] + m[8 + i] * v[2] + m[12 + i] * w);
  }
  function multiply(a, b) {
    const out = new Float32Array(16);
    for (let col = 0; col < 4; col++) {
      for (let row = 0; row < 4; row++) {
        let sum = 0;
        for (let k = 0; k < 4; k++) sum += a[k * 4 + row] * b[col * 4 + k];
        out[col * 4 + row] = sum;
      }
    }
    return out;
  }
  function invert(m) {
    const [a00, a01, a02, a03, a10, a11, a12, a13, a20, a21, a22, a23, a30, a31, a32, a33] = m;
    const b00 = a00 * a11 - a01 * a10, b01 = a00 * a12 - a02 * a10, b02 = a00 * a13 - a03 * a10;
    const b03 = a01 * a12 - a02 * a11, b04 = a01 * a13 - a03 * a11, b05 = a02 * a13 - a03 * a12;
    const b06 = a20 * a31 - a21 * a30, b07 = a20 * a32 - a22 * a30, b08 = a20 * a33 - a23 * a30;
    const b09 = a21 * a32 - a22 * a31, b10 = a21 * a33 - a23 * a31, b11 = a22 * a33 - a23 * a32;
    const det = 1 / (b00 * b11 - b01 * b10 + b02 * b09 + b03 * b08 - b04 * b07 + b05 * b06);
    return new Float32Array([
      (a11 * b11 - a12 * b10 + a13 * b09) * det, (a02 * b10 - a01 * b11 - a03 * b09) * det,
      (a31 * b05 - a32 * b04 + a33 * b03) * det, (a22 * b04 - a21 * b05 - a23 * b03) * det,
      (a12 * b08 - a10 * b11 - a13 * b07) * det, (a00 * b11 - a02 * b08 + a03 * b07) * det,
      (a32 * b02 - a30 * b05 - a33 * b01) * det, (a20 * b05 - a22 * b02 + a23 * b01) * det,
      (a10 * b10 - a11 * b08 + a13 * b06) * det, (a01 * b08 - a00 * b10 - a03 * b06) * det,
      (a30 * b04 - a31 * b02 + a33 * b00) * det, (a21 * b02 - a20 * b04 - a23 * b00) * det,
      (a11 * b07 - a10 * b09 - a12 * b06) * det, (a00 * b09 - a01 * b07 + a02 * b06) * det,
      (a31 * b01 - a30 * b03 - a32 * b00) * det, (a20 * b03 - a21 * b01 + a22 * b00) * det,
    ]);
  }

  window.__frameYoutubeVr = { enter };
})();
