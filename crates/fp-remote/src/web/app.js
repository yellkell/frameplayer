// FramePlayer web remote. Every string from the server is rendered with
// textContent or as an attribute value; nothing is ever parsed as HTML.
'use strict';

(function () {
  const $ = (id) => document.getElementById(id);
  const el = {
    conn: $('conn'), title: $('np-title'), seek: $('seek'), pos: $('pos'), dur: $('dur'),
    back: $('back'), play: $('play'), fwd: $('fwd'), speed: $('speed'),
    recenter: $('recenter'), stop: $('stop'),
    typeForm: $('type-form'), typeText: $('type-text'), typeStatus: $('type-status'),
    searchForm: $('search-form'), q: $('q'), results: $('results'), more: $('more'),
    libStatus: $('lib-status'),
  };

  let status = null;      // last status from the server
  let clockOffset = 0;    // phone clock minus server clock, ms
  let dragging = false;   // user is moving the seek slider
  let unpaired = false;
  const PAGE = 30;
  let libQuery = '';
  let libOffset = 0;

  function setConn(text, bad) {
    el.conn.textContent = text;
    el.conn.classList.toggle('bad', !!bad);
  }

  function fmt(t) {
    if (!isFinite(t) || t < 0) t = 0;
    t = Math.floor(t);
    const h = Math.floor(t / 3600);
    const m = Math.floor((t % 3600) / 60);
    const s = String(t % 60).padStart(2, '0');
    return h > 0 ? h + ':' + String(m).padStart(2, '0') + ':' + s : m + ':' + s;
  }

  function api(path, opts) {
    const o = Object.assign({ credentials: 'same-origin', cache: 'no-store' }, opts || {});
    return fetch(path, o).then((r) => {
      if (r.status === 401) {
        unpaired = true;
        setConn('Not paired: scan the QR code in the headset', true);
        throw new Error('not paired');
      }
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r;
    });
  }

  function postJson(path, body) {
    return api(path, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
  }

  function send(cmd) {
    return postJson('/api/command', cmd).catch((e) => setConn('Command failed: ' + e.message, true));
  }

  function position() {
    if (!status) return 0;
    if (!status.playing) return status.position;
    const serverNow = Date.now() - clockOffset;
    const p = status.position + Math.max(0, serverNow - status.sampled_at_ms) / 1000 * status.speed;
    return status.duration > 0 ? Math.min(p, status.duration) : p;
  }

  function applyStatus(s) {
    if (!s || typeof s !== 'object') return;
    clockOffset = Date.now() - Number(s.now_ms || Date.now());
    status = s;
    const open = typeof s.location === 'string' && s.location.length > 0;
    el.title.textContent = open ? (s.title || s.location) : 'Nothing playing';
    el.play.textContent = s.playing ? 'Pause' : 'Play';
    el.play.setAttribute('aria-label', s.playing ? 'Pause' : 'Play');
    for (const b of [el.play, el.back, el.fwd, el.stop]) b.disabled = !open;
    el.seek.disabled = !open || !(s.duration > 0);
    el.seek.max = String(s.duration > 0 ? s.duration : 0);
    el.dur.textContent = fmt(s.duration);
    selectSpeed(Number(s.speed) || 1);
    tick();
  }

  function selectSpeed(v) {
    let best = null;
    for (const o of el.speed.options) {
      if (Math.abs(Number(o.value) - v) < 0.001) best = o;
    }
    if (!best) {
      best = document.createElement('option');
      best.value = String(v);
      best.textContent = v + '×';
      el.speed.appendChild(best);
    }
    if (document.activeElement !== el.speed) best.selected = true;
  }

  function tick() {
    if (!status || dragging) return;
    const p = position();
    el.seek.value = String(p);
    el.pos.textContent = fmt(p);
    el.seek.setAttribute('aria-valuetext', fmt(p) + ' of ' + fmt(status.duration));
  }

  function refreshStatus() {
    return api('/api/status').then((r) => r.json()).then(applyStatus);
  }

  function connect() {
    if (!window.EventSource) {
      setInterval(() => refreshStatus().then(() => setConn('Connected'), () => {}), 2000);
      return;
    }
    const es = new EventSource('/api/events');
    es.onopen = () => setConn('Connected');
    es.onmessage = (e) => {
      try { applyStatus(JSON.parse(e.data)); setConn('Connected'); } catch (_) { /* ignore */ }
    };
    es.onerror = () => {
      if (unpaired) { es.close(); return; }
      setConn('Reconnecting…', true);
      // Distinguishes "headset unreachable" from "pairing revoked".
      refreshStatus().catch(() => { if (unpaired) es.close(); });
    };
  }

  // Transport
  el.play.addEventListener('click', () => send({ cmd: status && status.playing ? 'pause' : 'play' }));
  el.back.addEventListener('click', () => send({ cmd: 'seek_relative', delta: -10 }));
  el.fwd.addEventListener('click', () => send({ cmd: 'seek_relative', delta: 10 }));
  el.stop.addEventListener('click', () => send({ cmd: 'stop' }));
  el.recenter.addEventListener('click', () => send({ cmd: 'recenter' }));
  el.speed.addEventListener('change', () => send({ cmd: 'set_speed', speed: Number(el.speed.value) }));
  el.seek.addEventListener('input', () => {
    dragging = true;
    el.pos.textContent = fmt(Number(el.seek.value));
  });
  el.seek.addEventListener('change', () => {
    const position = Number(el.seek.value);
    send({ cmd: 'seek', position: position }).finally(() => { dragging = false; });
    if (status) { status.position = position; status.sampled_at_ms = Date.now() - clockOffset; }
  });

  // Typing into the headset
  el.typeForm.addEventListener('submit', (e) => {
    e.preventDefault();
    const text = el.typeText.value;
    postJson('/api/text', { text: text }).then(() => {
      el.typeStatus.classList.remove('bad');
      el.typeStatus.textContent = 'Sent to headset';
      el.typeText.value = '';
    }, (err) => {
      el.typeStatus.classList.add('bad');
      el.typeStatus.textContent = 'Not sent: ' + err.message;
    });
  });

  // Library
  function itemRow(item) {
    const li = document.createElement('li');
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'item';
    const title = String(item.title || item.location || '');
    btn.setAttribute('aria-label', 'Play ' + title);

    let thumb;
    if (item.has_thumbnail && Number.isSafeInteger(item.id)) {
      thumb = document.createElement('img');
      thumb.src = '/api/thumb/' + encodeURIComponent(String(item.id));
      thumb.alt = '';
      thumb.loading = 'lazy';
      thumb.decoding = 'async';
    } else {
      thumb = document.createElement('span');
    }
    thumb.className = 'thumb';
    thumb.setAttribute('aria-hidden', 'true');

    const meta = document.createElement('span');
    meta.className = 'meta';
    const t = document.createElement('span');
    t.className = 't';
    t.textContent = title;
    const s = document.createElement('span');
    s.className = 's';
    const parts = [];
    if (typeof item.duration === 'number' && item.duration > 0) parts.push(fmt(item.duration));
    if (item.format_label) parts.push(String(item.format_label));
    s.textContent = parts.join(' · ');
    meta.append(t, s);
    btn.append(thumb, meta);

    btn.addEventListener('click', () => {
      el.libStatus.textContent = 'Opening ' + title + '…';
      send({ cmd: 'open', location: String(item.location) });
    });
    li.appendChild(btn);
    return li;
  }

  function search(query, offset) {
    libQuery = query;
    libOffset = offset;
    el.libStatus.classList.remove('bad');
    if (offset === 0) el.libStatus.textContent = 'Searching…';
    const url = '/api/library?q=' + encodeURIComponent(query) + '&limit=' + PAGE + '&offset=' + offset;
    return api(url).then((r) => r.json()).then((d) => {
      const items = Array.isArray(d.items) ? d.items : [];
      if (offset === 0) el.results.replaceChildren();
      for (const it of items) el.results.appendChild(itemRow(it));
      libOffset = offset + items.length;
      el.more.hidden = items.length < PAGE;
      const n = el.results.children.length;
      el.libStatus.textContent = n === 0 ? 'No results' : n + (n === 1 ? ' result' : ' results');
    }, (err) => {
      el.libStatus.classList.add('bad');
      el.libStatus.textContent = 'Search failed: ' + err.message;
    });
  }

  el.searchForm.addEventListener('submit', (e) => {
    e.preventDefault();
    search(el.q.value.trim(), 0);
  });
  el.more.addEventListener('click', () => search(libQuery, libOffset));

  setInterval(tick, 250);
  refreshStatus().then(() => { setConn('Connected'); connect(); search('', 0); }, () => {
    if (!unpaired) { setConn('Headset unreachable', true); connect(); }
  });
})();
