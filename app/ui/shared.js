// What the main window and the overlay have in common: the app's view, setting controls, and the
// gallery with its player. Each page defines render(), called when the view changes.
"use strict";

const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

const FPS = [30, 60, 120, 144, 165, 240];
const HEIGHTS = [[2160, "2160p (4K)"], [1440, "1440p (2K)"], [1080, "1080p"], [720, "720p"]];
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

const currentMonitor = () => {
  const { settings: s, probe: p } = view;
  return p.monitors.find((m) => m.id === s.monitor) || p.monitors.find((m) => m.primary) || p.monitors[0];
};

// Sizes go by the monitor's short edge, so these mean the same on one turned on its side.
function heightOptions() {
  const s = view.settings;
  const current = currentMonitor();
  const short = current ? Math.min(current.width, current.height) : 0;
  const native = current ? `Monitor, ${current.width}×${current.height}` : "Monitor";
  const heights = [[0, native], ...HEIGHTS.filter(([h]) => h !== short)];
  if (s.resolution && !heights.some(([h]) => h === s.resolution)) heights.push([s.resolution, `${s.resolution}p`]);
  return heights;
}

// Said wherever the resolution can be changed, since more pixels here isn't more picture.
function upscaleNote() {
  const s = view.settings;
  const current = currentMonitor();
  if (!current || s.resolution <= Math.min(current.width, current.height)) return "";
  return `<p class="note">Upscaled from ${current.width}×${current.height}. Files get larger and the picture gains no detail, but sites like YouTube keep higher resolutions at better quality. Raise the bitrate to match.</p>`;
}

function renderReplaySwitch() {
  const on = view.settings.replay;
  const key = view.settings.hotkeys.toggleReplay;
  const el = $("t-replay");
  el.classList.toggle("on", on);
  el.setAttribute("aria-checked", on);
  el.title = `Turn instant replay ${on ? "off" : "on"}${key ? ` (${key})` : ""}`;
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
  const unit = key === "bitrateMbps" ? " Mbps" : key === "micDelayMs" ? " ms" : "%";
  $(`n-${key}`).textContent = e.target.value + unit;
});

// Applied on release: a bitrate change restarts the encoder, so not on every step of a drag.
document.addEventListener("change", (e) => {
  const key = e.target.dataset.range;
  if (!key) return;
  e.target.blur();
  save((s) => (s[key] = Number(e.target.value)));
});

// ---------- shared links ----------

// The share server's storage against R2's free allowance, the links on it, and clips on their way
// up. Warns once on crossing each level, so a nearly full bucket is never a surprise.
const Cloud = (() => {
  const WARN = 0.8;
  const FULL = 0.95;
  let usage = null; // null until known, or while sharing isn't set up
  const jobs = new Map(); // path -> { stage, progress }
  let changed = () => {};

  const level = (u) => (u ? u.used / u.free : 0);
  const tone = (u) => (level(u) >= FULL ? "bad" : level(u) >= WARN ? "warn" : "");

  function left(ms) {
    const hours = Math.max(0, Math.round((ms - Date.now()) / 3600000));
    if (hours < 1) return "under an hour";
    if (hours < 48) return `${hours} h`;
    return `${Math.round(hours / 24)} days`;
  }

  // Apple Watch style: a thick round-capped arc on a dim track, the number in the middle.
  function ring(u, size, label = true) {
    const stroke = Math.max(3, Math.round(size / 9));
    const r = (size - stroke) / 2;
    const round = 2 * Math.PI * r;
    const f = Math.min(1, level(u));
    const arc = f > 0 ? `<circle class="ring-arc" cx="${size / 2}" cy="${size / 2}" r="${r}" stroke-width="${stroke}" stroke-dasharray="${Math.max(0.01, f * round)} ${round}" transform="rotate(-90 ${size / 2} ${size / 2})"/>` : "";
    const middle = label && u ? `<span class="ring-mid"><b>${Math.round(f * 100)}%</b><small>used</small></span>` : "";
    return `<span class="ring ${tone(u)}" style="width:${size}px;height:${size}px"><svg viewBox="0 0 ${size} ${size}" style="width:${size}px;height:${size}px"><circle class="ring-track" cx="${size / 2}" cy="${size / 2}" r="${r}" stroke-width="${stroke}"/>${arc}</svg>${middle}</span>`;
  }

  // T3-style limit rows: what's used, a thin bar, and when it next eases off.
  function bars(u) {
    const f = Math.min(1, level(u));
    const next = [...u.clips].sort((a, b) => a.expires - b.expires)[0];
    const capNote = `Uploads stop at ${bytes(u.cap)}, so the free plan is never exceeded.`;
    return `<div class="limit">
        <div class="limit-head"><span>Storage</span><span>${bytes(u.used)} of ${bytes(u.free)}</span></div>
        <div class="limit-bar ${tone(u)}"><i style="width:${(f * 100).toFixed(1)}%"></i><b style="left:${((u.cap / u.free) * 100).toFixed(1)}%"></b></div>
        <div class="limit-sub">${u.clips.length ? `${u.clips.length} link${u.clips.length === 1 ? "" : "s"} live. Next expires in ${left(next.expires)}, freeing ${bytes(next.bytes)}.` : "No live links."} ${capNote}</div>
      </div>`;
  }

  function linkRows(u) {
    if (!u.clips.length) return `<div class="empty">Nothing shared right now. Links last ${u.linkDays} days.</div>`;
    return u.clips
      .map((l) => `<div class="link-row">
          <div class="link-info"><div class="link-name" title="${esc(l.name)}">${esc(l.name.replace(/\.(DVR\.)?mp4$/i, ""))}</div>
          <div class="link-meta">${bytes(l.bytes)} · expires in ${left(l.expires)}</div></div>
          <button class="btn sm press" data-link-copy="${esc(l.url)}">Copy link</button>
          <button class="btn sm ghost danger press" data-link-delete="${esc(l.id)}">Delete</button>
        </div>`)
      .join("");
  }

  // Crossing up into a level says so once; dropping back resets it.
  function warn(u) {
    const now = level(u) >= FULL ? 2 : level(u) >= WARN ? 1 : 0;
    const before = Number(localStorage.getItem("cloudWarned") || 0);
    localStorage.setItem("cloudWarned", now);
    if (now > before) {
      toast(now === 2 ? `Cloud storage is ${Math.round(level(u) * 100)}% full. New links are refused near the limit; delete some or let them expire.` : `Cloud storage is ${Math.round(level(u) * 100)}% full.`, now === 2);
    }
  }

  function set(u) {
    usage = u;
    if (u) warn(u);
    changed();
  }

  async function load(fresh) {
    try {
      set(await invoke("share_usage", { fresh: !!fresh }));
    } catch (err) {
      if (fresh) toast(String(err), true);
    }
  }

  const linkFor = (key) => usage?.clips.find((l) => l.source === key && l.expires > Date.now());

  // Converts, uploads and copies, or copies the live link it already has.
  async function share(clip) {
    if (jobs.has(clip.path)) return invoke("cancel_share");
    jobs.set(clip.path, { stage: "converting", progress: 0 });
    changed();
    try {
      const url = await invoke("share_clip", { path: clip.path });
      toast(`Link copied: ${url}`);
    } catch (err) {
      if (String(err) !== "Cancelled.") toast(String(err), true);
    } finally {
      jobs.delete(clip.path);
      changed();
    }
  }

  // What the player's button says for a clip.
  function label(clip) {
    const job = jobs.get(clip.path);
    if (job) return `${job.stage === "uploading" ? "Uploading" : "Converting"} ${Math.round(job.progress * 100)}% · Cancel`;
    return linkFor(clip.key) ? "Copy link again" : "Copy link";
  }

  function onChange(f) {
    const previous = changed;
    changed = () => {
      previous();
      f();
    };
  }

  document.addEventListener("click", async (e) => {
    const copy = e.target.closest("[data-link-copy]");
    if (copy) {
      await invoke("copy_text", { text: copy.dataset.linkCopy });
      return toast("Link copied");
    }
    const del = e.target.closest("[data-link-delete]");
    if (del) {
      del.disabled = true;
      try {
        set(await invoke("unshare", { id: del.dataset.linkDelete }));
        toast("Link deleted");
      } catch (err) {
        del.disabled = false;
        toast(String(err), true);
      }
    }
  });

  listen("share", (e) => {
    const job = e.payload;
    if (job.stage === "converting" || job.stage === "uploading") jobs.set(job.path, job);
    changed();
  });
  listen("share-usage", (e) => set(e.payload));
  invoke("share_jobs").then((list) => list.forEach((j) => jobs.set(j.path, j)));

  return { load, ring, bars, linkRows, linkFor, share, label, onChange, left, usage: () => usage, tone };
})();

// ---------- gallery ----------

const Gallery = (() => {
  const THUMB_WIDTH = 320;
  const THUMB_HEIGHT = 180;
  const GIVE_UP_MS = 8000;
  const KINDS = [["all", "All"], ["clip", "Clips"], ["recording", "Recordings"]];

  let box = null;
  let playerChanged = () => {};
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

  function linkBadge(c) {
    const link = Cloud.linkFor(c.key);
    return link ? `<span class="shared" title="Shared. The link works for ${Cloud.left(link.expires)} more.">Link · ${Cloud.left(link.expires)}</span>` : "";
  }

  const card = (c) => `<button class="tile" data-play="${esc(c.path)}" title="${esc(c.name)}">
      <span class="thumb">${c.thumb ? `<img src="${esc(convertFileSrc(c.thumb))}" alt="" loading="lazy">` : ""}
        ${c.seconds != null ? `<span class="length">${clock(c.seconds)}</span>` : ""}
        ${c.kind === "recording" ? `<span class="kind">Recording</span>` : ""}
        ${linkBadge(c)}</span>
      <span class="title">${esc(c.game)}</span>
      <span class="meta">${when(c.modified)} · ${bytes(c.bytes)}</span>
    </button>`;

  function cloudChip() {
    const u = Cloud.usage();
    if (!u) return "";
    return `<button class="cloud-chip ${Cloud.tone(u)}" data-act="links" title="Cloud storage for shared links: ${bytes(u.used)} of the free ${bytes(u.free)}">${Cloud.ring(u, 18, false)}<span>${bytes(u.used)} / ${bytes(u.free)}</span></button>`;
  }

  function renderList() {
    if (!box) return;
    const list = shown();
    const empty = clips.length ? "Nothing here matches." : "Nothing saved yet. Clips show up here as soon as you save them.";
    box.innerHTML = `<header class="top gallery-bar" data-tauri-drag-region>
        <h1 data-tauri-drag-region>Clips<span class="count">${clips.length || ""}</span></h1>
        <div class="sp" data-tauri-drag-region></div>
        ${cloudChip()}
        <select class="game-pick" aria-label="Game">${gameOptions().map(([v, label]) => `<option value="${esc(v)}" ${v === game ? "selected" : ""}>${esc(label)}</option>`).join("")}</select>
        <div class="seg">${KINDS.map(([v, label]) => `<button data-kind="${v}" class="${v === kind ? "on" : ""}">${label}</button>`).join("")}</div>
      </header>
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
    if (current < 0) playerChanged(true);
    current = index;
    const v = video();
    $("player-note").hidden = true;
    v.hidden = false;
    v.src = convertFileSrc(clip.path);
    v.play().catch(() => {});
    $("player-title").textContent = clip.game;
    $("player-meta").textContent = `${clip.name} · ${when(clip.modified)} · ${bytes(clip.bytes)}`;
    renderShare();
    $("player-prev").disabled = index === 0;
    $("player-next").disabled = index === list.length - 1;
    player().hidden = false;
    $("player-scrim").hidden = false;
    document.body.classList.add("playing");
  }

  function renderShare() {
    const clip = shown()[current];
    const button = $("player-share");
    if (!clip || !button) return;
    button.textContent = Cloud.label(clip);
    button.title = Cloud.linkFor(clip.key) ? "Already shared: copies the same link" : "Uploads a smaller copy and copies a link that works for a few days";
  }

  Cloud.onChange(() => {
    renderShare();
    const chip = box?.querySelector(".cloud-chip");
    if (chip) chip.outerHTML = cloudChip() || "<span></span>";
    else if (box && Cloud.usage()) box.querySelector(".gallery-bar .cap")?.insertAdjacentHTML("afterend", cloudChip());
    // Badges only; a full redraw would restart thumbnails that are loading.
    if (box) for (const el of box.querySelectorAll(".tile")) {
      const c = clips.find((x) => x.path === el.dataset.play);
      const thumb = el.querySelector(".thumb");
      thumb?.querySelector(".shared")?.remove();
      if (c && thumb) thumb.insertAdjacentHTML("beforeend", linkBadge(c));
    }
  });

  function stop() {
    if (current < 0) return;
    current = -1;
    player().hidden = true;
    $("player-scrim").hidden = true;
    document.body.classList.remove("playing");
    const v = video();
    v.pause();
    v.removeAttribute("src");
    v.load();
    playerChanged(false);
    fillThumbs();
  }

  function play(path) {
    show(shown().findIndex((c) => c.path === path));
  }

  const isOpen = () => current >= 0;

  function mount(element, onPlayer) {
    box = element;
    if (onPlayer) playerChanged = onPlayer;
    // A popup over the dimmed page. Clicking the dimmed part closes it.
    document.body.insertAdjacentHTML("beforeend", `<div class="scrim" id="player-scrim" hidden data-player="close"></div>
    <div class="player" id="player" hidden role="dialog" aria-modal="true" aria-label="Player">
      <div class="player-bar">
        <div class="player-info"><div class="player-title" id="player-title"></div><div class="player-meta" id="player-meta"></div></div>
        <button class="btn sm press" id="player-prev" data-player="prev">Newer</button>
        <button class="btn sm press" id="player-next" data-player="next">Older</button>
        <button class="btn sm ghost press" data-player="reveal">Show in folder</button>
        <button class="btn sm ghost press" data-player="external">Open with default app</button>
        <button class="btn sm pri press" id="player-share" data-player="share">Copy link</button>
        <button class="btn sm ghost icon press" data-player="close" title="Close (Esc)" aria-label="Close"><svg viewBox="0 0 16 16"><path d="M4 4l8 8M12 4l-8 8"/></svg></button>
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
      case "share": return clip && Cloud.share(clip);
    }
  });

  return { mount, load, play, stop, isOpen, makeThumb };
})();
