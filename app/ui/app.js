// tape's window. Everything shown comes from the app's view; a setting takes effect the moment
// it changes, so there is no Save button.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

const FPS = [30, 60, 120, 144, 165, 240];
const HEIGHTS = [2160, 1440, 1080, 720];
const LENGTHS = [[30, "30 seconds"], [60, "1 minute"], [120, "2 minutes"], [300, "5 minutes"], [600, "10 minutes"], [900, "15 minutes"], [1200, "20 minutes"]];
const CODECS = [["h264", "H.264", "plays everywhere"], ["hevc", "HEVC", "smaller files, less widely playable"], ["av1", "AV1", "smallest files, newest GPUs only"]];
const PRESETS = [["fast", "Fast"], ["balanced", "Balanced"], ["quality", "Quality"]];
const METHODS = [["auto", "Automatic"], ["duplication", "Desktop Duplication"], ["graphics", "Windows Graphics Capture"]];
const KEYS = [["saveClip", "Save clip"], ["toggleRecording", "Record"], ["toggleReplay", "Instant replay"]];

let view = null;
let listening = null;
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

// ---------- settings form ----------

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

function deviceOptions(devices, value, offLabel) {
  const options = [["default", "Windows default"], ...devices.map((d) => [d.id, d.name]), ["", offLabel]];
  // Keep a saved device on the list while it's unplugged, so opening this doesn't change it.
  if (value && value !== "default" && !devices.some((d) => d.id === value)) options.splice(1, 0, [value, "Saved device (not connected)"]);
  return options;
}

function renderSettings() {
  const s = view.settings;
  const p = view.probe;
  const recording = view.status.recording;
  const monitors = p.monitors.map((m) => [m.primary ? "" : m.id, `${m.name}, ${m.width}×${m.height} ${m.hz ? m.hz + " Hz" : ""}${m.primary ? " (primary)" : ""}`]);
  const current = p.monitors.find((m) => m.id === s.monitor) || p.monitors.find((m) => m.primary) || p.monitors[0];
  const native = current ? `Monitor, ${current.width}×${current.height}` : "Monitor";
  const heights = [[0, native], ...HEIGHTS.filter((h) => !current || h < current.height).map((h) => [h, `${h}p`])];
  if (s.resolution && !heights.some(([h]) => h === s.resolution)) heights.push([s.resolution, `${s.resolution}p`]);
  const codecs = CODECS.map(([v, label]) => [v, label, p.codecs.length > 0 && !p.codecs.includes(v)]);
  const codecNote = (CODECS.find(([v]) => v === s.codec) || [])[2];
  const taken = view.hotkeyErrors;

  const html = [];
  html.push(`<div class="group"><span class="cap">Video</span>
    ${monitors.length > 1 ? row("Monitor", select("monitor", monitors, s.monitor)) : ""}
    ${row("Resolution", select("resolution", heights, s.resolution))}
    ${row("Frame rate", select("fps", FPS.map((f) => [f, `${f} fps`]), s.fps))}
    ${row("Format", select("codec", codecs, s.codec))}
    <p class="note">${esc(codecNote || "")}</p>
    ${row("Bitrate", `<input type="range" data-range="bitrateMbps" min="5" max="150" step="5" value="${s.bitrateMbps}" aria-label="Bitrate"><span class="num" id="n-bitrateMbps">${s.bitrateMbps} Mbps</span>`)}
    ${row("Encoder quality", seg("preset", PRESETS, s.preset))}
    ${recording ? `<p class="note warn">Video and audio device changes apply when the recording stops.</p>` : ""}
  </div>`);

  html.push(`<div class="group"><span class="cap">Instant replay</span>
    ${row("Keep the last", select("replaySeconds", LENGTHS, s.replaySeconds))}
    <p class="note">Held in memory, never written to disk until you save. Uses up to ${bytes(view.replayRam)} of RAM at this bitrate.</p>
  </div>`);

  html.push(`<div class="group"><span class="cap">Audio</span>
    ${row("Game audio", select("systemAudio", deviceOptions(p.outputs, s.systemAudio, "Off"), s.systemAudio))}
    <div class="level"><i id="l-system"></i></div>
    ${s.systemAudio ? row("Volume", `<input type="range" data-range="systemVolume" min="0" max="200" step="5" value="${s.systemVolume}" aria-label="Game audio volume"><span class="num" id="n-systemVolume">${s.systemVolume}%</span>`) : ""}
    ${row("Microphone", select("microphone", deviceOptions(p.inputs, s.microphone, "Off"), s.microphone))}
    <div class="level"><i id="l-mic"></i></div>
    ${s.microphone ? row("Volume", `<input type="range" data-range="micVolume" min="0" max="200" step="5" value="${s.micVolume}" aria-label="Microphone volume"><span class="num" id="n-micVolume">${s.micVolume}%</span>`) : ""}
    ${s.systemAudio && s.microphone ? row("Separate tracks", toggle("separateTracks", s.separateTracks, "Separate tracks")) : ""}
    ${s.systemAudio && s.microphone && s.separateTracks ? `<p class="note">Good for editing. Most players and Discord only play the first track, so clips sound like game audio alone.</p>` : ""}
  </div>`);

  html.push(`<div class="group"><span class="cap">Keys</span>
    ${KEYS.map(([k, label]) => row(label, `<button class="keycap ${taken.includes(s.hotkeys[k]) ? "taken" : ""}" data-hotkey="${k}">${esc(s.hotkeys[k] || "None")}</button>`)).join("")}
    ${taken.length ? `<p class="note warn">${esc(taken.join(", "))} ${taken.length > 1 ? "are" : "is"} in use by another app.</p>` : ""}
  </div>`);

  html.push(`<div class="group"><span class="cap">Files</span>
    ${row("Save to", `<div class="path" title="${esc(view.folder)}">${esc(view.folder)}</div><button class="btn quiet small" data-act="pick-folder">Change</button>`)}
  </div>`);

  html.push(`<div class="group"><span class="cap">General</span>
    ${row("Start with Windows", toggle("autostart", view.autostart, "Start with Windows"))}
    ${row("Sounds", toggle("sounds", s.sounds, "Sounds"))}
    <p class="note">Short tones when a clip saves or something goes wrong, for when a game is covering the screen.</p>
    ${row("Show mouse pointer", toggle("showCursor", s.showCursor, "Show mouse pointer"))}
    ${row("Capture method", select("capture", METHODS, s.capture))}
    ${row("Problems", `<button class="btn quiet small" data-act="self-test">Run self-test</button><button class="link-btn" data-act="open-log">Open log</button>`)}
    <p class="note">tape ${esc(view.version)}${p.gpu ? `, ${esc(p.gpu)}` : ""}</p>
  </div>`);

  const scroller = $("settings");
  const top = scroller.scrollTop;
  scroller.innerHTML = html.join("");
  scroller.scrollTop = top;
}

function renderClips() {
  const list = $("clips");
  if (!view.clips.length) {
    const key = view.settings.hotkeys.saveClip || "the save key";
    list.innerHTML = `<div class="empty">Nothing saved yet. Press ${esc(key)} during a game to keep the last ${esc((LENGTHS.find(([v]) => v === view.settings.replaySeconds) || [0, "few minutes"])[1])}.</div>`;
    return;
  }
  list.innerHTML = view.clips
    .map((c) => `<button class="clip" data-open="${esc(c.path)}" title="Open. Right-click to show in folder."><span class="name">${esc(c.name.replace(/\.mp4$/i, ""))}</span><span class="size">${bytes(c.bytes)}</span><span class="when">${when(c.modified)}</span></button>`)
    .join("");
}

function renderAlerts() {
  const out = [];
  const h = view.health;
  if (h.problem) out.push(`<div class="alert"><b>${esc(h.problem)}</b>${h.light === "trouble" ? " tape keeps retrying on its own." : ""}</div>`);
  if (view.probe.nvencError) out.push(`<div class="alert"><b>Can't use the GPU encoder.</b> ${esc(view.probe.nvencError)}</div>`);
  if (view.overlayRunning) out.push(`<div class="alert"><b>The NVIDIA overlay is running.</b> Turn it off in the NVIDIA app so it stops recording in the background and lets go of the same hotkeys.</div>`);
  if (h.engineRestarts > 0) out.push(`<div class="alert">Recovered ${h.engineRestarts === 1 ? "once" : `${h.engineRestarts} times`} since starting. Last: ${esc(h.lastRestart || "unknown")}.</div>`);
  $("alerts").innerHTML = out.join("");
}

function renderKeys() {
  const k = view.settings.hotkeys;
  $("k-clip").textContent = k.saveClip;
  $("k-rec").textContent = k.toggleRecording;
}

function renderAll() {
  if (document.activeElement?.matches?.("input[type=range]")) return; // don't move a slider in use
  renderSettings();
  renderClips();
  renderAlerts();
  renderKeys();
  renderStatus(view.status);
}

function renderStatus(s) {
  view.status = s;
  const light = view.health.light;
  const state = $("tb-state");
  state.className = `tb-state ${light}`;
  state.textContent = { idle: "Off", replay: "Instant replay on", recording: "Recording", trouble: view.health.problem || "Not capturing" }[light];

  const replay = $("p-replay");
  if (!s.replayOn) {
    replay.className = "val dim";
    replay.textContent = "Off";
    $("p-replay-fill").style.width = "0";
  } else {
    replay.className = s.video.running ? "val live" : "val warn";
    replay.innerHTML = `${clock(s.replaySeconds)} <small>of ${clock(s.replayTarget)}</small>`;
    $("p-replay-fill").style.width = `${Math.min(100, (s.replaySeconds / Math.max(1, s.replayTarget)) * 100)}%`;
  }

  const rec = $("p-rec");
  rec.className = s.recording ? "val rec" : "val dim";
  rec.textContent = s.recording ? clock(s.recordingSeconds) : "Off";
  $("p-rec-sub").textContent = s.recording ? bytes(s.recordingBytes) : "";

  const running = s.video.running;
  const fps = $("p-fps");
  fps.className = running ? "val" : "val dim";
  fps.innerHTML = running ? `${Math.round(s.video.fps)} <small>fps</small>` : "—";
  $("p-fps-sub").textContent = running ? `${s.video.width}×${s.video.height}${s.video.dropped ? `, ${s.video.dropped} dropped` : ""}` : "";
  const rate = $("p-rate");
  rate.className = running ? "val" : "val dim";
  rate.innerHTML = running ? `${(s.video.bitrateKbps / 1000).toFixed(1)} <small>Mbps</small>` : "—";
  $("p-rate-sub").textContent = running ? s.video.method : "";

  $("b-clip").disabled = !s.replayOn;
  const button = $("b-rec");
  button.className = s.recording ? "btn big quiet stop" : "btn big quiet";
  $("b-rec-text").textContent = s.recording ? "Stop recording" : "Start recording";

  const level = (id, source) => {
    const bar = $(id);
    if (bar) bar.style.width = `${Math.min(100, (source?.peak || 0) * 100)}%`;
  };
  level("l-system", s.system);
  level("l-mic", s.mic);
}

async function refresh() {
  view = await invoke("view");
  renderAll();
}

async function save(change) {
  const settings = structuredClone(view.settings);
  change(settings);
  view.settings = settings; // shown right away, confirmed by the reply
  view = await invoke("save_settings", { settings });
  renderAll();
}

// ---------- input ----------

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

document.addEventListener("click", async (e) => {
  const target = e.target.closest("[data-act],[data-seg],[data-toggle],[data-hotkey],[data-open]");
  if (!target) return;
  const { act, seg: segKey, toggle: toggleKey, hotkey, open } = target.dataset;
  if (open) return invoke("open_file", { path: open });
  if (segKey) return save((s) => (s[segKey] = target.dataset.value));
  if (hotkey) return listenForKey(target, hotkey);
  if (toggleKey === "autostart") {
    view.autostart = await invoke("set_autostart", { enabled: !view.autostart });
    return renderAll();
  }
  if (toggleKey) return save((s) => (s[toggleKey] = !s[toggleKey]));
  const w = window.__TAURI__.window.getCurrentWindow();
  switch (act) {
    case "win-min": return w.minimize();
    case "win-close": return w.close();
    case "clip": return invoke("save_clip");
    case "record": return invoke("toggle_recording");
    case "open-folder": return invoke("open_folder");
    case "open-log": return invoke("open_log");
    case "pick-folder": {
      const folder = await invoke("pick_folder");
      if (folder) save((s) => (s.folder = folder));
      return;
    }
    case "self-test":
      $("report-text").textContent = "Running. This takes about 20 seconds, and replay keeps running.";
      $("report").hidden = false;
      return invoke("self_test");
    case "close-report":
      $("report").hidden = true;
      return;
    case "copy-report":
      return navigator.clipboard.writeText($("report-text").textContent);
  }
});

document.addEventListener("contextmenu", (e) => {
  const item = e.target.closest("[data-open]");
  e.preventDefault();
  if (item) invoke("reveal_file", { path: item.dataset.open });
});

// ---------- rebinding keys ----------

function accelerator(e) {
  const code = e.code;
  let key = null;
  if (/^F\d+$/.test(code)) key = code;
  else if (/^Key[A-Z]$/.test(code)) key = code.slice(3);
  else if (/^Digit\d$/.test(code)) key = code.slice(5);
  else if (/^Numpad\d$/.test(code)) key = code;
  else if (["Home", "End", "Insert", "Delete", "PageUp", "PageDown", "Pause", "ScrollLock", "PrintScreen"].includes(code)) key = code;
  if (!key) return null;
  const parts = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  // A letter alone would fire while typing anywhere; function keys and the like are fine alone.
  if (!parts.length && !/^(F\d+|Home|End|Insert|Delete|PageUp|PageDown|Pause|ScrollLock|PrintScreen)$/.test(key)) return null;
  return [...parts, key].join("+");
}

async function listenForKey(button, name) {
  if (listening) return;
  listening = name;
  button.classList.add("listening");
  button.textContent = "Press keys. Esc cancels, Backspace clears.";
  await invoke("pause_hotkeys");
  const done = async (value) => {
    document.removeEventListener("keydown", onKey, true);
    listening = null;
    await invoke("resume_hotkeys");
    if (value === undefined) await refresh();
    else await save((s) => (s.hotkeys[name] = value));
  };
  const onKey = (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (e.code === "Escape") return done();
    if (e.code === "Backspace") return done("");
    const combo = accelerator(e);
    if (combo) done(combo);
  };
  document.addEventListener("keydown", onKey, true);
}

// ---------- engine events ----------

listen("status", (e) => view && renderStatus(e.payload));
listen("changed", () => !listening && refresh());
listen("saved", (e) => {
  toast(`Saved ${e.payload.split(/[\\/]/).pop()}`);
  refresh();
});
listen("error", (e) => toast(e.payload, true));
listen("selftest", (e) => {
  $("report-text").textContent = e.payload;
  $("report").hidden = false;
});

refresh();
