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
  Cloud.follow(view.settings);
  render();
}

async function save(change) {
  const settings = structuredClone(view.settings);
  change(settings);
  view.settings = settings; // shown right away, confirmed by the reply
  view = await invoke("save_settings", { settings });
  Cloud.follow(view.settings);
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

  let asked = 0;

  // Only the newest answer counts, so one from a server just switched away from can't land last.
  async function load(fresh) {
    const mine = ++asked;
    try {
      const u = await invoke("share_usage", { fresh: !!fresh });
      if (mine === asked) set(u);
    } catch (err) {
      if (fresh && mine === asked) toast(String(err), true);
    }
  }

  // A different share server or key makes the usage and links shown so far someone else's. The app
  // drops its copy on saving; this drops the page's and asks again. Either window may have changed it.
  let server;
  function follow(settings) {
    const now = `${settings.shareServer}\n${settings.shareKey}`;
    if (server !== undefined && now !== server) {
      usage = null;
      changed();
      load(false);
    }
    server = now;
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

  const active = (job) => job.stage === "converting" || job.stage === "uploading";

  // Every window hears these, not only the one that started the share, so each drops the job once
  // it's done or failed (a cancel ends as "failed" too).
  listen("share", (e) => {
    const job = e.payload;
    if (active(job)) jobs.set(job.path, job);
    else jobs.delete(job.path);
    changed();
  });
  listen("share-usage", (e) => set(e.payload));
  invoke("share_jobs").then((list) => {
    list.filter(active).forEach((j) => jobs.set(j.path, j));
    changed();
  });

  return { load, follow, ring, bars, linkRows, linkFor, share, label, onChange, left, usage: () => usage, tone };
})();

// ---------- gallery ----------

const Gallery = (() => {
  const THUMB_WIDTH = 320;
  const THUMB_HEIGHT = 180;
  const GIVE_UP_MS = 8000;
  const KINDS = [["all", "All"], ["clip", "Clips"], ["recording", "Recordings"], ["starred", "Starred"]];

  const STAR = `<svg viewBox="0 0 16 16"><path d="M8 1.8l1.85 3.86 4.2.53-3.08 2.92.78 4.17L8 11.25l-3.75 2.03.78-4.17L1.95 6.19l4.2-.53z"/></svg>`;

  let box = null;
  let options = { rename: true };
  let playerChanged = () => {};
  let clips = [];
  let game = "";
  let kind = "all";
  // The clip in the player, by path: the list reloads under it (a save puts a new clip on top), so
  // an index would end up pointing at a neighbour.
  let currentPath = null;
  let making = false;
  const unreadable = new Set(); // no point retrying these until the next run

  const shown = () => clips.filter((c) => (!game || c.game === game) && (kind === "all" || c.kind === kind || (kind === "starred" && c.starred)));

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

  // A name the user gave it, or null for the automatic "<game> <date> - <time>" (and its trims).
  const AUTO_NAME = /\d{4}\.\d\d\.\d\d - \d\d\.\d\d\.\d\d\.\d\d( trim( \d+)?)?( \(\d+\))?$/;
  const stem = (c) => c.name.replace(/(\.DVR)?\.mp4$/i, "");
  const customName = (c) => (AUTO_NAME.test(stem(c)) ? null : stem(c));

  const card = (c) => `<button class="tile" data-play="${esc(c.path)}" title="${esc(c.name)}">
      <span class="thumb">${c.thumb ? `<img src="${esc(convertFileSrc(c.thumb))}" alt="" loading="lazy">` : ""}
        ${c.seconds != null ? `<span class="length">${clock(c.seconds)}</span>` : ""}
        ${c.kind === "recording" ? `<span class="kind">Recording</span>` : ""}
        ${c.starred ? `<span class="star-badge" title="Starred">${STAR}</span>` : ""}
        ${linkBadge(c)}</span>
      <span class="title">${esc(customName(c) || c.game)}</span>
      <span class="meta">${customName(c) ? `${esc(c.game)} · ` : ""}${when(c.modified)} · ${bytes(c.bytes)}</span>
    </button>`;

  function cloudChip() {
    const u = Cloud.usage();
    if (!u) return "";
    return `<button class="cloud-chip ${Cloud.tone(u)}" data-act="links" title="Cloud storage for shared links: ${bytes(u.used)} of the free ${bytes(u.free)}">${Cloud.ring(u, 18, false)}<span>${bytes(u.used)} / ${bytes(u.free)}</span></button>`;
  }

  function renderList() {
    if (!box) return;
    const list = shown();
    const empty = !clips.length
      ? "Nothing saved yet. Clips show up here as soon as you save them."
      : kind === "starred" && !clips.some((c) => c.starred)
        ? "Nothing starred yet. Star a clip in the player to keep it handy here."
        : "Nothing here matches.";
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
      if (!next || isOpen()) break;
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
    resync();
    fillThumbs();
  }

  // After the list or the filter changes: the player keeps its clip, or closes if it's gone.
  function resync() {
    if (!isOpen()) return;
    if (!playing()) return stop();
    // Redrawing the bar would drop a rename being typed, so then only the parts around it.
    if ($("player-rename")) {
      renderNav();
      renderShare();
    } else {
      renderBar();
    }
  }

  // ---------- player ----------

  const player = () => $("player");
  const video = () => $("player-video");
  const playing = () => (currentPath == null ? undefined : shown().find((c) => c.path === currentPath));
  const index = () => shown().findIndex((c) => c.path === currentPath);

  function show(at, time) {
    const clip = shown()[at];
    if (!clip) return;
    if (!isOpen()) playerChanged(true);
    closeTrim();
    closeMenu();
    currentPath = clip.path;
    const v = video();
    $("player-note").hidden = true;
    v.hidden = false;
    v.src = convertFileSrc(clip.path);
    if (time) v.currentTime = time;
    v.play().catch(() => {});
    renderBar();
    player().hidden = false;
    $("player-scrim").hidden = false;
    document.body.classList.add("playing");
  }

  function renderBar() {
    const clip = playing();
    if (!clip) return;
    const custom = customName(clip);
    $("player-info").innerHTML = `<div class="player-title" title="${esc(clip.name)}">${esc(custom || clip.game)}<span class="player-name">${esc(custom ? clip.game : stem(clip).replace(clip.game, "").trim())}</span></div>
      <div class="player-meta">${when(clip.modified)} · ${bytes(clip.bytes)}${clip.seconds != null ? ` · ${clock(clip.seconds)}` : ""}</div>`;
    const star = $("player-star");
    star.classList.toggle("on", !!clip.starred);
    star.title = clip.starred ? "Starred. Click to unstar." : "Star: keeps it under Starred";
    star.setAttribute("aria-pressed", !!clip.starred);
    renderNav();
    renderShare();
  }

  function renderNav() {
    const i = index();
    $("player-prev").disabled = i <= 0;
    $("player-next").disabled = i < 0 || i === shown().length - 1;
  }

  function renderShare() {
    const clip = playing();
    const button = $("player-share");
    if (!clip || !button) return;
    button.textContent = Cloud.label(clip);
    button.title = Cloud.linkFor(clip.key) ? "Already shared: copies the same link" : "Uploads a smaller copy and copies a link that works for a few days";
  }

  Cloud.onChange(() => {
    renderShare();
    const chip = box?.querySelector(".cloud-chip");
    const html = cloudChip();
    if (chip) {
      if (html) chip.outerHTML = html;
      else chip.remove();
    } else if (html) {
      box?.querySelector(".gallery-bar .game-pick")?.insertAdjacentHTML("beforebegin", html); // where renderList puts it
    }
    // Badges only; a full redraw would restart thumbnails that are loading.
    if (box) for (const el of box.querySelectorAll(".tile")) {
      const c = clips.find((x) => x.path === el.dataset.play);
      const thumb = el.querySelector(".thumb");
      thumb?.querySelector(".shared")?.remove();
      if (c && thumb) thumb.insertAdjacentHTML("beforeend", linkBadge(c));
    }
  });

  // Lets go of the file, which Windows won't rename or delete while it's being read.
  function release() {
    const v = video();
    v.pause();
    v.removeAttribute("src");
    v.load();
  }

  function stop() {
    if (!isOpen()) return;
    closeTrim();
    closeMenu();
    currentPath = null;
    player().hidden = true;
    $("player-scrim").hidden = true;
    document.body.classList.remove("playing");
    release();
    playerChanged(false);
    fillThumbs();
  }

  function play(path, time) {
    show(shown().findIndex((c) => c.path === path), time);
  }

  const isOpen = () => currentPath != null;

  // Esc closes the innermost thing open. False if there was nothing to close.
  function escape() {
    if ($("player-rename")) return cancelRename(), true;
    if (!$("player-menu").hidden) return closeMenu(), true;
    if (trim) return closeTrim(), true;
    if (isOpen()) return stop(), true;
    return false;
  }

  // ---------- star, rename, delete ----------

  async function toggleStar() {
    const clip = playing();
    if (!clip) return;
    const on = !clip.starred;
    try {
      await invoke("star_clip", { path: clip.path, on });
      clip.starred = on;
      renderBar();
      renderList();
    } catch (err) {
      toast(String(err), true);
    }
  }

  function openMenu() {
    const menu = $("player-menu");
    if (!menu.hidden) return closeMenu();
    menu.innerHTML = `${options.rename ? `<button class="menu-item" data-player="rename">Rename</button>` : ""}
      <button class="menu-item" data-player="reveal">Show in folder</button>
      <button class="menu-item" data-player="external">Open with default app</button>
      <div class="menu-sep"></div>
      <button class="menu-item danger" data-player="delete" id="player-delete">Delete</button>`;
    menu.hidden = false;
  }

  function closeMenu() {
    const menu = $("player-menu");
    if (menu) menu.hidden = true;
  }

  function startRename() {
    const clip = playing();
    if (!clip) return;
    closeMenu();
    const title = $("player-info").querySelector(".player-title");
    const current = stem(clip);
    title.innerHTML = `<input class="text rename" id="player-rename" value="${esc(current)}" spellcheck="false" aria-label="New name">`;
    const input = $("player-rename");
    input.focus();
    input.select();
    input.addEventListener("keydown", (e) => {
      e.stopPropagation(); // Esc here cancels the rename, not the player
      if (e.key === "Enter") finishRename(input.value);
      if (e.key === "Escape") cancelRename();
    });
    input.addEventListener("blur", () => $("player-rename") && finishRename(input.value));
  }

  function cancelRename() {
    renderBar();
  }

  async function finishRename(name) {
    const clip = playing();
    const input = $("player-rename");
    if (!clip || !input || input.dataset.done) return;
    input.dataset.done = "1"; // removing it fires blur, which would come back here
    input.remove();
    if (!name.trim() || name.trim() === stem(clip)) return renderBar();
    const v = video();
    const at = v.currentTime;
    const paused = v.paused;
    release();
    try {
      const path = await invoke("rename_clip", { path: clip.path, name });
      currentPath = path; // so the reload below keeps the player on it
      await load();
      play(path, at);
      if (paused) video().pause();
      toast("Renamed");
    } catch (err) {
      toast(String(err), true);
      play(clip.path, at);
    }
  }

  let deleteArmed = 0;

  // Asks once by changing the button, so it works without a keyboard over a game too.
  async function remove() {
    const clip = playing();
    const button = $("player-delete");
    if (!clip || !button) return;
    if (Date.now() - deleteArmed > 4000) {
      deleteArmed = Date.now();
      button.textContent = "Move to Recycle Bin?";
      button.classList.add("armed");
      return;
    }
    deleteArmed = 0;
    // The next older one takes its place, or the newer one if it was the last.
    const list = shown();
    const i = list.indexOf(clip);
    const next = list[i + 1] || list[i - 1];
    release();
    try {
      await invoke("delete_clip", { path: clip.path });
      toast("Moved to the Recycle Bin");
      if (!isOpen()) return load(); // a reload in the meantime already closed it
      if (!next) {
        stop();
        return load();
      }
      currentPath = next.path;
      await load();
      if (isOpen()) show(index());
    } catch (err) {
      toast(String(err), true);
      play(clip.path);
    }
  }

  // ---------- trim ----------

  // While open: { path, keyframes, duration, start, end, saving }.
  let trim = null;
  let frame = 0;

  // To a tenth of a second, which is as close as anyone drags.
  const fine = (t) => `${clock(t)}.${Math.floor((t % 1) * 10)}`;

  // A cut can only start where a keyframe is, so the start handle jumps between them.
  const snap = (t) => trim.keyframes.reduce((best, k) => (k <= t + 0.001 ? Math.max(best, k) : best), trim.keyframes[0] || 0);

  async function openTrim() {
    const clip = playing();
    if (!clip) return;
    if (trim) return closeTrim();
    closeMenu();
    let info;
    try {
      info = await invoke("clip_info", { path: clip.path });
    } catch (err) {
      return toast(String(err), true);
    }
    if (playing()?.path !== clip.path) return;
    trim = { path: clip.path, keyframes: info.keyframes, duration: info.duration, start: 0, end: info.duration, saving: false };
    const ticks = info.keyframes.length <= 400 ? info.keyframes.map((k) => `<i style="left:${(k / info.duration) * 100}%"></i>`).join("") : "";
    $("player-trim").innerHTML = `<div class="trim-track" id="trim-track">
        <div class="trim-keys">${ticks}</div>
        <div class="trim-sel" id="trim-sel"></div>
        <div class="trim-head" id="trim-head"></div>
        <div class="trim-handle" id="trim-start" data-edge="start" role="slider" aria-label="Start" title="Start. Arrow keys move it a keyframe at a time." tabindex="0"></div>
        <div class="trim-handle" id="trim-end" data-edge="end" role="slider" aria-label="End" title="End. Arrow keys move it a tenth of a second, Shift for a whole second." tabindex="0"></div>
      </div>
      <div class="trim-row">
        <button class="btn sm press" data-player="trim-start" title="Start at the keyframe nearest before the playhead">Start here</button>
        <button class="btn sm press" data-player="trim-end">End here</button>
        <div class="trim-times" id="trim-times"></div>
        <div class="sp"></div>
        <button class="btn sm ghost press" data-player="trim-cancel">Cancel</button>
        <button class="btn sm pri press" id="trim-save" data-player="trim-save">Save trimmed copy</button>
      </div>`;
    $("player-trim").hidden = false;
    $("player-trim-btn").classList.add("on");
    renderTrim();
    const tick = () => {
      if (!trim) return;
      followPlayhead();
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
  }

  function closeTrim() {
    if (!trim) return;
    trim = null;
    cancelAnimationFrame(frame);
    $("player-trim").hidden = true;
    $("player-trim").innerHTML = "";
    $("player-trim-btn").classList.remove("on");
  }

  function renderTrim() {
    if (!trim) return;
    const pct = (t) => `${(t / trim.duration) * 100}%`;
    $("trim-start").style.left = pct(trim.start);
    $("trim-end").style.left = pct(trim.end);
    // Screen readers hear the handles as sliders over the clip's length, in seconds.
    for (const [id, value] of [["trim-start", trim.start], ["trim-end", trim.end]]) {
      const handle = $(id);
      handle.setAttribute("aria-valuemin", 0);
      handle.setAttribute("aria-valuemax", trim.duration.toFixed(1));
      handle.setAttribute("aria-valuenow", value.toFixed(1));
      handle.setAttribute("aria-valuetext", fine(value));
    }
    $("trim-sel").style.left = pct(trim.start);
    $("trim-sel").style.width = `${((trim.end - trim.start) / trim.duration) * 100}%`;
    const length = trim.end - trim.start;
    $("trim-times").innerHTML = `<b>${fine(trim.start)}</b> to <b>${fine(trim.end)}</b> · ${fine(length)} of ${clock(trim.duration)}`;
    const save = $("trim-save");
    save.disabled = trim.saving || length < 0.5 || (trim.start <= 0.001 && trim.end >= trim.duration - 0.05);
    save.textContent = trim.saving ? "Saving…" : "Save trimmed copy";
  }

  // Keeps the playhead marker in place and playback inside the cut.
  function followPlayhead() {
    const v = video();
    const head = $("trim-head");
    if (!head) return;
    head.style.left = `${(v.currentTime / trim.duration) * 100}%`;
    if (!dragging && !v.paused && (v.currentTime >= trim.end || v.currentTime < trim.start - 0.25)) v.currentTime = trim.start;
  }

  function setEdge(edge, t) {
    t = Math.max(0, Math.min(trim.duration, t));
    if (edge === "start") trim.start = Math.min(snap(t), snap(Math.max(0, trim.end - 0.5)));
    else trim.end = Math.max(t, trim.start + 0.5);
    trim.end = Math.min(trim.end, trim.duration);
    renderTrim();
    // Show the frame the cut lands on.
    video().currentTime = edge === "start" ? trim.start : Math.max(trim.start, trim.end - 0.05);
  }

  let dragging = null;

  function timeAt(e) {
    const r = $("trim-track").getBoundingClientRect();
    return ((e.clientX - r.left) / r.width) * trim.duration;
  }

  document.addEventListener("pointerdown", (e) => {
    if (!trim || !e.target.closest("#trim-track")) return;
    const handle = e.target.closest(".trim-handle");
    video().pause();
    if (handle) {
      dragging = handle.dataset.edge;
      handle.setPointerCapture(e.pointerId);
    } else {
      video().currentTime = Math.max(trim.start, Math.min(trim.end, timeAt(e)));
    }
    e.preventDefault();
  });
  document.addEventListener("pointermove", (e) => dragging && trim && setEdge(dragging, timeAt(e)));
  document.addEventListener("pointerup", () => (dragging = null));
  document.addEventListener("keydown", (e) => {
    const handle = e.target.closest?.(".trim-handle");
    const steps = { ArrowLeft: -1, ArrowDown: -1, ArrowRight: 1, ArrowUp: 1 };
    if (!trim || !handle || !(e.key in steps || e.key === "Home" || e.key === "End")) return;
    e.preventDefault();
    const edge = handle.dataset.edge;
    if (e.key === "Home" || e.key === "End") return setEdge(edge, e.key === "Home" ? 0 : trim.duration);
    const back = steps[e.key] < 0;
    if (edge === "start") {
      // Keyframe by keyframe.
      const keys = trim.keyframes;
      const i = keys.findIndex((k) => Math.abs(k - trim.start) < 0.001);
      const next = keys[Math.max(0, Math.min(keys.length - 1, i + (back ? -1 : 1)))];
      if (next != null) setEdge("start", next);
    } else {
      setEdge("end", trim.end + (back ? -1 : 1) * (e.shiftKey ? 1 : 0.1));
    }
  });

  async function saveTrim() {
    if (!trim || trim.saving) return;
    trim.saving = true;
    renderTrim();
    const { path, start, end } = trim;
    try {
      const made = await invoke("trim_clip", { path, start, end });
      await load();
      // A new file sorts first, so "All" might not be where it shows; make sure it's in the list.
      if (!shown().some((c) => c.path === made)) {
        kind = "all";
        game = "";
        renderList();
      }
      play(made);
      toast("Saved a trimmed copy. The original is unchanged.");
    } catch (err) {
      if (trim) {
        trim.saving = false;
        renderTrim();
      }
      toast(String(err), true);
    }
  }

  function mount(element, onPlayer, opts) {
    box = element;
    if (onPlayer) playerChanged = onPlayer;
    options = { ...options, ...opts };
    // A popup over the dimmed page. Clicking the dimmed part closes it.
    document.body.insertAdjacentHTML("beforeend", `<div class="scrim" id="player-scrim" hidden data-player="close"></div>
    <div class="player" id="player" hidden role="dialog" aria-modal="true" aria-label="Player">
      <div class="player-bar">
        <div class="player-info" id="player-info"></div>
        <button class="btn sm press" id="player-prev" data-player="prev">Newer</button>
        <button class="btn sm press" id="player-next" data-player="next">Older</button>
        <button class="btn sm ghost icon press star-btn" id="player-star" data-player="star" aria-label="Star">${STAR}</button>
        <button class="btn sm press" id="player-trim-btn" data-player="trim" title="Cut it down and save the result as a new clip"><svg viewBox="0 0 16 16"><circle cx="4" cy="4.5" r="2"/><circle cx="4" cy="11.5" r="2"/><path d="M5.6 5.6L13 12M5.6 10.4L13 4"/></svg>Trim</button>
        <button class="btn sm pri press" id="player-share" data-player="share">Copy link</button>
        <div class="menu-wrap">
          <button class="btn sm ghost icon press" data-player="menu" title="More" aria-label="More"><svg viewBox="0 0 16 16"><circle cx="3.5" cy="8" r=".6"/><circle cx="8" cy="8" r=".6"/><circle cx="12.5" cy="8" r=".6"/></svg></button>
          <div class="menu" id="player-menu" hidden></div>
        </div>
        <button class="btn sm ghost icon press" data-player="close" title="Close (Esc)" aria-label="Close"><svg viewBox="0 0 16 16"><path d="M4 4l8 8M12 4l-8 8"/></svg></button>
      </div>
      <video id="player-video" controls></video>
      <div class="player-note" id="player-note" hidden>This video can't play here, usually because Windows is missing the format's codec. It should still open in your default app or in a browser.</div>
      <div class="trim" id="player-trim" hidden></div>
    </div>`);
    const v = video();
    v.volume = Number(localStorage.getItem("volume") ?? 1);
    v.addEventListener("volumechange", () => localStorage.setItem("volume", v.volume));
    v.addEventListener("error", () => {
      if (!isOpen() || !v.getAttribute("src")) return;
      v.hidden = true;
      $("player-note").hidden = false;
    });
  }

  document.addEventListener("change", (e) => {
    if (!e.target.matches(".game-pick")) return;
    game = e.target.value;
    renderList();
    resync();
  });

  document.addEventListener("click", (e) => {
    if (!e.target.closest(".menu-wrap")) closeMenu();
    const target = e.target.closest("[data-play],[data-kind],[data-player]");
    if (!target) return;
    const { play: path, kind: wanted, player: action } = target.dataset;
    if (path) return play(path);
    if (wanted) {
      kind = wanted;
      renderList();
      return resync();
    }
    const clip = playing();
    if (action !== "delete") deleteArmed = 0;
    switch (action) {
      case "prev": return show(index() - 1);
      case "next": return show(index() + 1);
      case "close": return stop();
      case "star": return toggleStar();
      case "menu": return openMenu();
      case "rename": return startRename();
      case "delete": return remove();
      case "trim": return openTrim();
      case "trim-start": return trim && setEdge("start", video().currentTime);
      case "trim-end": return trim && setEdge("end", video().currentTime);
      case "trim-cancel": return closeTrim();
      case "trim-save": return saveTrim();
      case "reveal": closeMenu(); return clip && invoke("reveal_file", { path: clip.path });
      case "external": closeMenu(); return clip && invoke("open_file", { path: clip.path });
      case "share": return clip && Cloud.share(clip);
    }
  });

  return { mount, load, play, stop, isOpen, escape, makeThumb };
})();
