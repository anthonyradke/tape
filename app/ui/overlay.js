// The side panel that opens over a game, without taking focus from it. Esc, the hotkey, the close
// button or Alt+Tab close it, and nothing here is ever left in the way.
"use strict";

// Windows can report a lost focus while a window is still being brought forward.
const IGNORE_BLUR_MS = 600;
let openedAt = Date.now();

function render() {
  if (document.activeElement?.matches?.("input[type=range]")) return; // don't move a slider in use
  const { settings: s, probe: p, status } = view;
  $("ov-settings").innerHTML = `<section class="sec"><h3>Video</h3><div class="card">
      ${row("Resolution", select("resolution", heightOptions(), s.resolution))}
      ${upscaleNote()}
      ${row("Frame rate", select("fps", FPS.map((f) => [f, `${f} fps`]), s.fps))}
      ${row("Bitrate", range("bitrateMbps", 5, 150, 5, " Mbps", "Bitrate"))}
      ${status.recording ? `<p class="note warn">Applies when the recording stops.</p>` : ""}
    </div></section>
    <section class="sec"><h3>Instant replay</h3><div class="card">
      ${row("Keep the last", select("replaySeconds", LENGTHS, s.replaySeconds))}
    </div></section>
    <section class="sec"><h3>Audio</h3><div class="card">
      ${row("Game audio", select("systemAudio", deviceOptions(p.outputs, s.systemAudio, "Off"), s.systemAudio))}
      ${row("Microphone", select("microphone", deviceOptions(p.inputs, s.microphone, "Off"), s.microphone))}
    </div></section>
    <div class="ov-more"><button class="btn sm ghost press" data-act="settings">All settings</button></div>`;
  $("k-clip").textContent = s.hotkeys.saveClip;
  $("k-rec").textContent = s.hotkeys.toggleRecording;
  $("k-short").textContent = s.hotkeys.shortClip;
  $("k-share").textContent = s.hotkeys.shareClip;
  $("b-share").hidden = !(s.shareServer && s.shareKey);
  renderReplaySwitch();
  renderStatus(status);
}

function renderStatus(s) {
  view.status = s;
  const light = view.health.light;
  const state = $("tb-state");
  const said = { idle: "Off", replay: "Instant replay on", recording: "Recording", trouble: view.health.problem || "Not capturing" }[light];
  state.className = `nav-status ${light}`;
  state.title = said;
  state.innerHTML = `<i class="dot"></i><span>${esc(said)}</span>`;
  $("ov-game").innerHTML = `Saving to <b>${esc(s.game || "Desktop")}</b>`;
  $("quality").textContent = qualityText(s);

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

  $("b-clip").disabled = !s.replayOn;
  $("b-clip-text").textContent = s.replayOn ? `Save last ${clock(Math.min(s.clipSeconds, s.replayTarget))}` : "Save clip";
  // What the short key saves right now: its length, or less right after another clip.
  const short = Math.min(view.settings.shortClipSeconds, s.replayTarget);
  $("b-short").disabled = !s.replayOn;
  $("b-share").disabled = !s.replayOn;
  $("b-short-text").textContent = s.replayOn ? `Save last ${span(Math.min(s.clipSeconds, short))}` : "Save short clip";
  $("b-rec").className = s.recording ? "btn big press stop" : "btn big press";
  $("b-rec-text").textContent = s.recording ? "Stop recording" : "Start recording";
}

function showPane(name) {
  document.body.dataset.pane = name;
  localStorage.setItem("pane", name);
  document.querySelectorAll("[data-pane-name]").forEach((b) => b.classList.toggle("on", b.dataset.paneName === name));
  // Only here, so opening the panel in a game doesn't start decoding thumbnails nobody asked for.
  if (name === "clips") Gallery.load();
}

function hide() {
  Gallery.stop();
  invoke("hide_overlay");
}

async function opened() {
  openedAt = Date.now();
  await refresh();
  showPane(localStorage.getItem("pane") || "settings");
}

document.addEventListener("click", (e) => {
  const target = e.target.closest("[data-act],[data-seg],[data-toggle],[data-pane-name]");
  if (!target) return;
  const { act, toggle: toggleKey, paneName } = target.dataset;
  if (paneName) return showPane(paneName);
  if (toggleKey) return save((s) => (s[toggleKey] = !s[toggleKey]));
  switch (act) {
    case "hide": return hide();
    case "clip": return invoke("save_clip");
    case "short-clip": return invoke("save_short_clip");
    case "share-clip": return invoke("save_and_share_clip");
    case "record": return invoke("toggle_recording");
    case "settings": return invoke("open_main");
    case "links": return invoke("open_main");
  }
});

// The webview's own menu only in text fields, for Paste.
document.addEventListener("contextmenu", (e) => e.target.matches?.("textarea, input:not([type=range])") || e.preventDefault());

function escape() {
  if (!Gallery.escape()) hide();
}

// Esc comes from the app as a global key, since the game keeps the keyboard. The keydown is for
// when a click gave the panel focus after all.
listen("overlay-escape", escape);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") escape();
});

// Only if a click gave it focus: clicking back into the game then closes it.
window.addEventListener("blur", () => {
  if (Date.now() - openedAt > IGNORE_BLUR_MS) hide();
});

listen("status", (e) => view && renderStatus(e.payload));
listen("changed", refresh);
listen("error", (e) => toast(e.payload, true));
listen("saved", (e) => {
  toast(`Saved ${e.payload.split(/[\\/]/).pop()}`);
  refresh();
  if (document.body.dataset.pane === "clips") Gallery.load();
});
listen("overlay-shown", opened);
listen("overlay-hidden", () => Gallery.stop());

// No renaming here: typing would need the keyboard the game is holding.
Gallery.mount($("gallery"), (open) => invoke("overlay_player", { open }), { rename: false, browse: false });
Cloud.load(false);
opened();
