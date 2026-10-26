// Send videos to FramePlayer: checks the code shown in the headset, then
// sends dropped files one at a time (PUT /api/upload) with progress.
"use strict";

const $ = (id) => document.getElementById(id);
let code = "";
const queue = [];
let busy = false;

function size(bytes) {
  return bytes >= 2 ** 30 ? (bytes / 2 ** 30).toFixed(1) + " GB" : Math.round(bytes / 2 ** 20) + " MB";
}

async function api(path) {
  const r = await fetch(path, { headers: { "X-Upload-Code": code } });
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body.error || "The headset did not answer (" + r.status + ").");
  return body;
}

async function showSpace() {
  try {
    const s = await api("/api/space");
    $("space").textContent = s.free == null ? "" : size(s.free) + " free on the headset. Videos go into " + s.folder + ".";
  } catch (e) {
    $("space").textContent = e.message;
  }
}

$("code-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  code = $("code").value.trim();
  $("code-error").textContent = "";
  try {
    await api("/api/space");
  } catch (e) {
    $("code-error").textContent = e.message;
    return;
  }
  try { sessionStorage.setItem("fp-upload-code", code); } catch (_) {}
  $("code-card").classList.add("hidden");
  $("drop-card").classList.remove("hidden");
  showSpace();
});

function add(files) {
  for (const file of files) {
    const li = document.createElement("li");
    li.innerHTML = '<div class="row"><span class="name"></span><span class="state">Waiting</span></div><div class="bar"><span></span></div>';
    li.querySelector(".name").textContent = file.name;
    $("queue").appendChild(li);
    queue.push({ file, li });
  }
  $("done").classList.add("hidden");
  next();
}

function next() {
  if (busy) return;
  const item = queue.shift();
  if (!item) {
    if ($("queue").children.length) $("done").classList.remove("hidden");
    showSpace();
    return;
  }
  busy = true;
  const state = item.li.querySelector(".state");
  const bar = item.li.querySelector(".bar span");
  const started = performance.now();
  const xhr = new XMLHttpRequest();
  xhr.open("PUT", "/api/upload?name=" + encodeURIComponent(item.file.name));
  xhr.setRequestHeader("X-Upload-Code", code);
  xhr.upload.onprogress = (e) => {
    if (!e.lengthComputable) return;
    const secs = (performance.now() - started) / 1000;
    const rate = e.loaded / Math.max(secs, 0.1);
    const left = rate > 0 ? Math.round((e.total - e.loaded) / rate) : 0;
    bar.style.width = (100 * e.loaded / e.total).toFixed(1) + "%";
    state.textContent = Math.floor(100 * e.loaded / e.total) + "% · " + size(rate) + "/s · " +
      (left >= 60 ? Math.ceil(left / 60) + " min" : left + " s") + " left";
  };
  const finish = (ok, text) => {
    state.textContent = text;
    state.className = "state " + (ok ? "ok" : "bad");
    if (ok) bar.style.width = "100%";
    busy = false;
    next();
  };
  xhr.onload = () => {
    let body = {};
    try { body = JSON.parse(xhr.responseText); } catch (_) {}
    if (xhr.status === 200) finish(true, "Sent");
    else finish(false, body.error || "Failed (" + xhr.status + ")");
  };
  xhr.onerror = () => finish(false, "Connection lost. Is the headset awake and Wi-Fi upload still on?");
  xhr.send(item.file);
}

const drop = $("drop");
$("files").addEventListener("change", (e) => { add(e.target.files); e.target.value = ""; });
["dragenter", "dragover"].forEach((t) => drop.addEventListener(t, (e) => { e.preventDefault(); drop.classList.add("over"); }));
["dragleave", "drop"].forEach((t) => drop.addEventListener(t, () => drop.classList.remove("over")));
drop.addEventListener("drop", (e) => { e.preventDefault(); add(e.dataTransfer.files); });
// Dropping a file anywhere else must not navigate away from the page.
window.addEventListener("dragover", (e) => e.preventDefault());
window.addEventListener("drop", (e) => e.preventDefault());

window.addEventListener("beforeunload", (e) => { if (busy) e.preventDefault(); });

try {
  const saved = sessionStorage.getItem("fp-upload-code");
  if (saved) { $("code").value = saved; $("code-form").requestSubmit(); }
} catch (_) {}
