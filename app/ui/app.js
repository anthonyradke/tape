// tape's window. Everything shown comes from the app's view; a setting takes effect the moment
// it changes, so there is no Save button.
"use strict";

const ICON_UPDATE = '<svg viewBox="0 0 24 24"><path d="M19.8 12A7.8 7.8 0 1 1 17.4 6.4"/><path d="M18.3 2.8V7h-4.2"/></svg>';
const ICON_DOWNLOAD = '<svg viewBox="0 0 24 24"><path d="M12 4v11M7 10.5l5 5 5-5M5 20h14"/></svg>';

let listening = null;
let upd = {};

function renderSettings() {
  const s = view.settings;
  const p = view.probe;
  const recording = view.status.recording;
  const monitors = p.monitors.map((m) => [m.primary ? "" : m.id, `${m.name}, ${m.width}×${m.height} ${m.hz ? m.hz + " Hz" : ""}${m.primary ? " (primary)" : ""}`]);
  const heights = heightOptions();
  const codecs = CODECS.map(([v, label]) => [v, label, p.codecs.length > 0 && !p.codecs.includes(v)]);
  const codecNote = (CODECS.find(([v]) => v === s.codec) || [])[2];
  const taken = view.hotkeyErrors;

  const html = [];
  html.push(`<div class="group"><span class="cap">Video</span>
    ${monitors.length > 1 ? row("Monitor", select("monitor", monitors, s.monitor)) : ""}
    ${row("Resolution", select("resolution", heights, s.resolution))}
    ${upscaleNote()}
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

  html.push(renderSharing());

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

function renderSharing() {
  const s = view.settings;
  const u = Cloud.usage();
  const ready = s.shareServer && s.shareKey;
  const storage = !ready ? "" : u
    ? row("Cloud storage", `<button class="cloud-chip" data-act="links">${Cloud.ring(u, 22, false)}<span>${bytes(u.used)} of ${bytes(u.free)}</span></button><button class="link-btn" data-act="links">Manage links</button>`)
    : row("Cloud storage", `<span class="dim-text">Not reached yet</span><button class="link-btn" data-act="links-retry">Try again</button>`);
  return `<div class="group"><span class="cap">Sharing</span>
    ${row("Share server", `<input type="text" class="text" data-key="shareServer" value="${esc(s.shareServer)}" placeholder="https://tape-clips.example.workers.dev" spellcheck="false" aria-label="Share server">`)}
    ${row("Upload key", `<input type="password" class="text" data-key="shareKey" value="${esc(s.shareKey)}" placeholder="From your share server" aria-label="Upload key">`)}
    ${storage}
    <p class="note">Copy link uploads a smaller 1080p copy to your own Cloudflare server. ${u ? `Links work for ${u.linkDays} days, then delete themselves.` : "Links delete themselves after a few days."} Setting one up: share/README.md in tape's repository.</p>
  </div>`;
}

function renderLinks() {
  const box = $("links-body");
  if (!box || $("links").hidden) return;
  const u = Cloud.usage();
  if (!u) {
    box.innerHTML = `<div class="empty">${view.settings.shareServer ? "Couldn't reach the share server." : "Sharing isn't set up. Add a share server and upload key in Settings."}</div>`;
    return;
  }
  box.innerHTML = `<div class="links-top">${Cloud.ring(u, 112)}${Cloud.bars(u)}</div><div class="links-list">${Cloud.linkRows(u)}</div>`;
}

function renderClips() {
  const list = $("clips");
  if (!view.clips.length) {
    const key = view.settings.hotkeys.saveClip || "the save key";
    list.innerHTML = `<div class="empty">Nothing saved yet. Press ${esc(key)} during a game to keep the last ${esc((LENGTHS.find(([v]) => v === view.settings.replaySeconds) || [0, "few minutes"])[1])}.</div>`;
    return;
  }
  list.innerHTML = view.clips
    .map((c) => `<button class="clip" data-open="${esc(c.path)}" title="Play. Right-click to show in folder."><span class="name">${esc(c.game)}${c.kind === "recording" ? " recording" : ""}</span><span class="size">${bytes(c.bytes)}</span><span class="when">${when(c.modified)}</span></button>`)
    .join("");
}

function renderAlerts() {
  const out = [];
  const h = view.health;
  if (h.problem) out.push(`<div class="alert"><b>${esc(h.problem)}</b>${h.light === "trouble" ? " tape keeps retrying on its own." : ""}</div>`);
  if (view.probe.nvencError) out.push(`<div class="alert"><b>Can't use the GPU encoder.</b> ${esc(view.probe.nvencError)}</div>`);
  if (view.overlayRunning) out.push(`<div class="alert"><b>The NVIDIA overlay is running.</b> Turn it off in the NVIDIA app so it stops recording in the background and lets go of the same hotkeys, Alt+Z included.</div>`);
  const u = Cloud.usage();
  if (u && u.used / u.free >= 0.8) {
    out.push(`<div class="alert"><b>Cloud storage is ${Math.round((u.used / u.free) * 100)}% full.</b> ${bytes(u.used)} of the free ${bytes(u.free)}. ${u.used >= u.cap ? "New links are refused until some expire or you delete them." : "Delete links you're done with to make room."} <button class="link-btn" data-act="links">Manage links</button></div>`);
  }
  if (h.engineRestarts > 0) out.push(`<div class="alert">Recovered ${h.engineRestarts === 1 ? "once" : `${h.engineRestarts} times`} since starting. Last: ${esc(h.lastRestart || "unknown")}.</div>`);
  $("alerts").innerHTML = out.join("");
}

function renderKeys() {
  const k = view.settings.hotkeys;
  $("k-clip").textContent = k.saveClip;
  $("k-rec").textContent = k.toggleRecording;
}

// Always in the title bar: a quiet icon that checks for updates, which turns into a labelled
// button when a new version is waiting, and shows the download while it installs.
function renderUpdate() {
  const el = $("upd");
  if (!upd.current) {
    el.hidden = true;
    return;
  }
  el.hidden = false;
  let cls = "", title, html = ICON_UPDATE;
  if (upd.progress != null) {
    cls = "busy";
    title = "tape restarts on its own when it's done.";
    html = `${ICON_UPDATE}<span>Updating ${upd.progress}%</span>`;
  } else if (upd.available) {
    cls = "ready";
    title = `Update to tape ${upd.available} and restart. Your settings and clips stay. Instant replay starts over.${upd.notes ? `\n\n${upd.notes}` : ""}`;
    html = `${ICON_DOWNLOAD}<span>Update</span>`;
  } else if (upd.checking) {
    cls = "spin";
    title = "Checking for updates…";
  } else {
    title = `Check for updates (tape ${upd.current})`;
  }
  el.className = `tb-upd ${cls}`;
  el.disabled = upd.progress != null;
  el.title = title;
  el.setAttribute("aria-label", title);
  el.innerHTML = html;
}

// Installs a waiting update, otherwise checks now.
async function updateClicked() {
  if (upd.progress != null || upd.checking) return;
  if (upd.available) {
    try {
      await invoke("install_update");
    } catch (err) {
      toast(`Update failed: ${err}`, true);
    }
    return;
  }
  upd = { ...upd, checking: true };
  renderUpdate();
  upd = await invoke("check_update");
  renderUpdate();
  if (upd.available) toast(`tape ${upd.available} is available`);
  else if (upd.error) toast(`Couldn't check for updates: ${upd.error}`, true);
  else toast("tape is up to date");
}

function render() {
  // Don't move a slider in use or retype a field being typed in.
  if (document.activeElement?.matches?.("input[type=range], input.text")) return;
  renderSettings();
  renderClips();
  renderAlerts();
  renderKeys();
  renderUpdate();
  renderReplaySwitch();
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

// ---------- tabs ----------

function showTab(name) {
  document.body.dataset.tab = name;
  document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("on", t.dataset.tabName === name));
  if (name === "gallery") return Gallery.load();
}

// ---------- input ----------

document.addEventListener("click", async (e) => {
  const target = e.target.closest("[data-act],[data-seg],[data-toggle],[data-hotkey],[data-open],[data-tab-name]");
  if (!target) return;
  const { act, seg: segKey, toggle: toggleKey, hotkey, open, tabName } = target.dataset;
  if (tabName) return showTab(tabName);
  if (open) {
    await showTab("gallery");
    return Gallery.play(open);
  }
  if (segKey) return save((s) => (s[segKey] = target.dataset.value));
  if (hotkey) return listenForKey(target, hotkey);
  if (toggleKey === "autostart") {
    view.autostart = await invoke("set_autostart", { enabled: !view.autostart });
    return render();
  }
  if (toggleKey) return save((s) => (s[toggleKey] = !s[toggleKey]));
  const w = window.__TAURI__.window.getCurrentWindow();
  switch (act) {
    case "win-min": return w.minimize();
    case "win-close": return w.close();
    case "update": return updateClicked();
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
    case "links":
      $("links").hidden = false;
      renderLinks();
      return Cloud.load(true);
    case "links-retry":
      return Cloud.load(true);
    case "close-links":
      $("links").hidden = true;
      return;
    case "close-report":
      $("report").hidden = true;
      return;
    case "copy-report":
      return navigator.clipboard.writeText($("report-text").textContent);
  }
});

document.addEventListener("contextmenu", (e) => {
  const item = e.target.closest("[data-open],[data-play]");
  e.preventDefault();
  if (item) invoke("reveal_file", { path: item.dataset.open || item.dataset.play });
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
listen("update", (e) => {
  upd = e.payload;
  renderUpdate();
});
listen("changed", () => !listening && refresh());
listen("saved", (e) => {
  toast(`Saved ${e.payload.split(/[\\/]/).pop()}`);
  refresh();
  if (document.body.dataset.tab === "gallery") Gallery.load();
});
listen("error", (e) => toast(e.payload, true));
listen("selftest", (e) => {
  $("report-text").textContent = e.payload;
  $("report").hidden = false;
});

Cloud.onChange(() => {
  if (!view) return;
  renderLinks();
  renderAlerts();
  if (!document.activeElement?.matches?.("input[type=range], input.text")) renderSettings();
});

Gallery.mount($("gallery"));
Cloud.load(false);
invoke("update_state").then((state) => {
  upd = state;
  renderUpdate();
});
refresh().then(() => {
  if (!view.uiTest) return;
  const test = document.createElement("script");
  test.src = "uitest.js";
  document.body.append(test);
});
