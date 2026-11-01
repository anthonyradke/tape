// What the main window and the overlay have in common: the app's view, setting controls, and the
// gallery with its player. Each page defines render(), called when the view changes.
"use strict";

const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

const FPS = [30, 60, 120, 144, 165, 240];
const HEIGHTS = [2160, 1440, 1080, 720];
const LENGTHS = [[30, "30 seconds"], [60, "1 minute"], [120, "2 minutes"], [300, "5 minutes"], [600, "10 minutes"], [900, "15 minutes"], [1200, "20 minutes"]];
const CODECS = [["h264", "H.264", "plays everywhere"], ["hevc", "HEVC", "smaller files, less widely playable"], ["av1", "AV1", "smallest files, newest GPUs only"]];
const PRESETS = [["fast", "Fast"], ["balanced", "Balanced"], ["quality", "Quality"]];
const METHODS = [["auto", "Automatic"], ["duplication", "Desktop Duplication"], ["graphics", "Windows Graphics Capture"]];
const KEYS = [["saveClip", "Save clip"], ["toggleRecording", "Record"], ["toggleReplay", "Instant replay"], ["overlay", "Overlay"]];

let view = null;
let toastTimer = 0;

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function clock(seconds) {
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const pad = (n) => String(n).padStart(2, "0");
  return h ? `${h}:${pad(m)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
}

function bytes(n) {
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${Math.round(n / 1e6)} MB`;
  return `${Math.round(n / 1e3)} KB`;
}

function when(ms) {
  const d = new Date(ms);
  const today = new Date().toDateString() === d.toDateString();
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  return today ? `Today, ${time}` : `${d.toLocaleDateString([], { month: "short", day: "numeric" })}, ${time}`;
}

function toast(text, bad) {
  const t = $("toast");
  t.textContent = text;
  t.className = bad ? "toast show bad" : "toast show";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.className = "toast"), bad ? 6000 : 2500);
}

// ---------- settings ----------

const select = (key, options, value, disabled) =>
  `<select data-key="${key}" ${disabled ? "disabled" : ""}>${options
    .map(([v, label, off]) => `<option value="${esc(v)}" ${String(v) === String(value) ? "selected" : ""} ${off ? "disabled" : ""}>${esc(label)}</option>`)
    .join("")}</select>`;

const seg = (key, options, value, disabled) =>
  `<div class="seg">${options
    .map(([v, label]) => `<button data-seg="${key}" data-value="${v}" class="${v === value ? "on" : ""}" ${disabled ? "disabled" : ""}>${label}</button>`)
    .join("")}</div>`;

const toggle = (key, on, label) =>
  `<button class="toggle ${on ? "on" : ""}" data-toggle="${key}" role="switch" aria-checked="${!!on}" aria-label="${esc(label)}"></button>`;

const row = (label, control) => `<div class="row"><div class="label">${label}</div><div class="control">${control}</div></div>`;

const range = (key, min, max, step, unit, label) =>
  `<input type="range" data-range="${key}" min="${min}" max="${max}" step="${step}" value="${view.settings[key]}" aria-label="${label}"><span class="num" id="n-${key}">${view.settings[key]}${unit}</span>`;

function deviceOptions(devices, value, offLabel) {
  const options = [["default", "Windows default"], ...devices.map((d) => [d.id, d.name]), ["", offLabel]];
  // Keep a saved device on the list while it's unplugged, so opening this doesn't change it.
  if (value && value !== "default" && !devices.some((d) => d.id === value)) options.splice(1, 0, [value, "Saved device (not connected)"]);
  return options;
}

function heightOptions() {
  const { settings: s, probe: p } = view;
  const current = p.monitors.find((m) => m.id === s.monitor) || p.monitors.find((m) => m.primary) || p.monitors[0];
  const native = current ? `Monitor, ${current.width}×${current.height}` : "Monitor";
  const heights = [[0, native], ...HEIGHTS.filter((h) => !current || h < current.height).map((h) => [h, `${h}p`])];
  if (s.resolution && !heights.some(([h]) => h === s.resolution)) heights.push([s.resolution, `${s.resolution}p`]);
  return heights;
}

async function refresh() {
  view = await invoke("view");
  render();
}

async function save(change) {
  const settings = structuredClone(view.settings);
  change(settings);
  view.settings = settings; // shown right away, confirmed by the reply
  view = await invoke("save_settings", { settings });
  render();
}

document.addEventListener("change", (e) => {
  const key = e.target.dataset.key;
  if (!key) return;
  const numeric = typeof view.settings[key] === "number";
  const value = numeric ? Number(e.target.value) : e.target.value;
  save((s) => (s[key] = value));
});

document.addEventListener("input", (e) => {
  const key = e.target.dataset.range;
  if (!key) return;
  const unit = key === "bitrateMbps" ? " Mbps" : "%";
  $(`n-${key}`).textContent = e.target.value + unit;
});

// Applied on release: a bitrate change restarts the encoder, so not on every step of a drag.
document.addEventListener("change", (e) => {
  const key = e.target.dataset.range;
  if (!key) return;
  e.target.blur();
  save((s) => (s[key] = Number(e.target.value)));
});

// ---------- gallery ----------

const Gallery = (() => {
  const THUMB_WIDTH = 320;
  const THUMB_HEIGHT = 180;
  const GIVE_UP_MS = 8000;
  const KINDS = [["all", "All"], ["clip", "Clips"], ["recording", "Recordings"]];

  let box = null;
  let clips = [];
  let game = "";
  let kind = "all";
  let current = -1; // index into shown() of the clip in the player
  let making = false;
  const unreadable = new Set(); // no point retrying these until the next run

  const shown = () => clips.filter((c) => (!game || c.game === game) && (kind === "all" || c.kind === kind));

  function gameOptions() {
    const counts = new Map();
    for (const c of clips) counts.set(c.game, (counts.get(c.game) || 0) + 1);
    const names = [...counts.keys()].sort((a, b) => a.localeCompare(b));
    return [["", `All games (${clips.length})`], ...names.map((n) => [n, `${n} (${counts.get(n)})`])];
  }

  const card = (c) => `<button class="card" data-play="${esc(c.path)}" title="${esc(c.name)}">
      <span class="thumb">${c.thumb ? `<img src="${esc(convertFileSrc(c.thumb))}" alt="" loading="lazy">` : ""}
        ${c.seconds != null ? `<span class="length">${clock(c.seconds)}</span>` : ""}
        ${c.kind === "recording" ? `<span class="kind">Recording</span>` : ""}</span>
      <span class="title">${esc(c.game)}</span>
      <span class="meta">${when(c.modified)} · ${bytes(c.bytes)}</span>
    </button>`;

  function renderList() {
    if (!box) return;
    const list = shown();
    const empty = clips.length ? "Nothing here matches." : "Nothing saved yet. Clips show up here as soon as you save them.";
    box.innerHTML = `<div class="gallery-bar">
        <div class="cap">Gallery</div>
        <select class="game-pick" aria-label="Game">${gameOptions().map(([v, label]) => `<option value="${esc(v)}" ${v === game ? "selected" : ""}>${esc(label)}</option>`).join("")}</select>
        <div class="seg">${KINDS.map(([v, label]) => `<button data-kind="${v}" class="${v === kind ? "on" : ""}">${label}</button>`).join("")}</div>
      </div>
      <div class="grid">${list.length ? list.map(card).join("") : `<div class="empty">${empty}</div>`}</div>`;
  }

  function once(video, event) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => done(reject, new Error("timed out")), GIVE_UP_MS);
      const failed = () => done(reject, new Error(video.error?.message || "can't read this video"));
      const ok = () => done(resolve);
      function done(settle, value) {
        clearTimeout(timer);
        video.removeEventListener(event, ok);
        video.removeEventListener("error", failed);
        settle(value);
      }
      video.addEventListener(event, ok);
      video.addEventListener("error", failed);
    });
  }

  // Thumbnails are grabbed from the video itself and kept on disk, so each is only made once.
  // Returns what was saved. Throws if the webview can't decode the file.
  async function makeThumb(clip) {
    const video = document.createElement("video");
    video.muted = true;
    video.preload = "auto";
    video.crossOrigin = "anonymous"; // or the canvas below can't be read back
    const ready = once(video, "loadedmetadata");
    video.src = convertFileSrc(clip.path);
    try {
      await ready;
      const seconds = Number.isFinite(video.duration) ? video.duration : 0;
      const seeked = once(video, "seeked");
      video.currentTime = Math.min(1.5, seconds / 2);
      await seeked;
      const canvas = document.createElement("canvas");
      canvas.width = THUMB_WIDTH;
      canvas.height = THUMB_HEIGHT;
      const scale = Math.min(THUMB_WIDTH / video.videoWidth, THUMB_HEIGHT / video.videoHeight);
      const [w, h] = [video.videoWidth * scale, video.videoHeight * scale];
      canvas.getContext("2d").drawImage(video, (THUMB_WIDTH - w) / 2, (THUMB_HEIGHT - h) / 2, w, h);
      const blob = await new Promise((resolve) => canvas.toBlob(resolve, "image/jpeg", 0.72));
      const jpeg = [...new Uint8Array(await blob.arrayBuffer())];
      const thumb = await invoke("save_thumb", { key: clip.key, seconds, jpeg });
      return { thumb, seconds };
    } finally {
      video.removeAttribute("src");
      video.load();
    }
  }

  // One at a time, newest first, and never while something's playing.
  async function fillThumbs() {
    if (making) return;
    making = true;
    for (;;) {
      const next = clips.find((c) => !c.thumb && !unreadable.has(c.key));
      if (!next || current >= 0) break;
      try {
        Object.assign(next, await makeThumb(next));
        renderList();
      } catch {
        unreadable.add(next.key);
      }
    }
    making = false;
  }

  async function load() {
    clips = await invoke("clips");
    if (game && !clips.some((c) => c.game === game)) game = "";
    renderList();
    fillThumbs();
  }

  // ---------- player ----------

  const player = () => $("player");
  const video = () => $("player-video");

  function show(index) {
    const list = shown();
    const clip = list[index];
    if (!clip) return;
    current = index;
    const v = video();
    $("player-note").hidden = true;
    v.hidden = false;
    v.src = convertFileSrc(clip.path);
    v.play().catch(() => {});
    $("player-title").textContent = clip.game;
    $("player-meta").textContent = `${clip.name} · ${when(clip.modified)} · ${bytes(clip.bytes)}`;
    $("player-prev").disabled = index === 0;
    $("player-next").disabled = index === list.length - 1;
    player().hidden = false;
  }

  function stop() {
    if (current < 0) return;
    current = -1;
    player().hidden = true;
    const v = video();
    v.pause();
    v.removeAttribute("src");
    v.load();
    fillThumbs();
  }

  function play(path) {
    show(shown().findIndex((c) => c.path === path));
  }

  const isOpen = () => current >= 0;

  function mount(element) {
    box = element;
    document.body.insertAdjacentHTML("beforeend", `<div class="player" id="player" hidden role="dialog" aria-label="Player">
      <div class="player-bar">
        <div class="player-info"><div class="player-title" id="player-title"></div><div class="player-meta" id="player-meta"></div></div>
        <button class="btn quiet small" id="player-prev" data-player="prev">Newer</button>
        <button class="btn quiet small" id="player-next" data-player="next">Older</button>
        <button class="btn quiet small" data-player="reveal">Show in folder</button>
        <button class="btn quiet small" data-player="external">Open with default app</button>
        <button class="btn small" data-player="close">Close</button>
      </div>
      <video id="player-video" controls></video>
      <div class="player-note" id="player-note" hidden>This video can't play here, usually because Windows is missing the format's codec. It should still open in your default app or in a browser.</div>
    </div>`);
    const v = video();
    v.volume = Number(localStorage.getItem("volume") ?? 1);
    v.addEventListener("volumechange", () => localStorage.setItem("volume", v.volume));
    v.addEventListener("error", () => {
      if (current < 0 || !v.getAttribute("src")) return;
      v.hidden = true;
      $("player-note").hidden = false;
    });
  }

  document.addEventListener("change", (e) => {
    if (!e.target.matches(".game-pick")) return;
    game = e.target.value;
    renderList();
  });

  document.addEventListener("click", (e) => {
    const target = e.target.closest("[data-play],[data-kind],[data-player]");
    if (!target) return;
    const { play: path, kind: wanted, player: action } = target.dataset;
    if (path) return play(path);
    if (wanted) {
      kind = wanted;
      return renderList();
    }
    const clip = shown()[current];
    switch (action) {
      case "prev": return show(current - 1);
      case "next": return show(current + 1);
      case "close": return stop();
      case "reveal": return clip && invoke("reveal_file", { path: clip.path });
      case "external": return clip && invoke("open_file", { path: clip.path });
    }
  });

  return { mount, load, play, stop, isOpen, makeThumb };
})();
