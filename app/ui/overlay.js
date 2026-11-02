// The side panel that opens over a game, without taking focus from it. Esc, the hotkey, the close
// button or Alt+Tab close it, and nothing here is ever left in the way.
"use strict";

// Windows can report a lost focus while a window is still being brought forward.
const IGNORE_BLUR_MS = 600;
let openedAt = Date.now();

function render() {
  if (document.activeElement?.matches?.("input[type=range]")) return; // don't move a slider in use
  const { settings: s, probe: p, status } = view;
  $("ov-settings").innerHTML = `<div class="group"><span class="cap">Video</span>
      ${row("Resolution", select("resolution", heightOptions(), s.resolution))}
      ${upscaleNote()}
      ${row("Frame rate", select("fps", FPS.map((f) => [f, `${f} fps`]), s.fps))}
      ${row("Bitrate", range("bitrateMbps", 5, 150, 5, " Mbps", "Bitrate"))}
      ${status.recording ? `<p class="note warn">Applies when the recording stops.</p>` : ""}
    </div>
    <div class="group"><span class="cap">Instant replay</span>
      ${row("Keep the last", select("replaySeconds", LENGTHS, s.replaySeconds))}
    </div>
    <div class="group"><span class="cap">Audio</span>
      ${row("Game audio", select("systemAudio", deviceOptions(p.outputs, s.systemAudio, "Off"), s.systemAudio))}
      ${row("Microphone", select("microphone", deviceOptions(p.inputs, s.microphone, "Off"), s.microphone))}
    </div>
    <div class="ov-more"><button class="link-btn" data-act="settings">All settings</button></div>`;
  $("k-clip").textContent = s.hotkeys.saveClip;
  $("k-rec").textContent = s.hotkeys.toggleRecording;
  renderReplaySwitch();
  renderStatus(status);
}

function renderStatus(s) {
  view.status = s;
  const light = view.health.light;
  const state = $("tb-state");
  state.className = `tb-state ${light}`;
  state.textContent = { idle: "Off", replay: "Instant replay on", recording: "Recording", trouble: view.health.problem || "Not capturing" }[light];
  $("ov-game").innerHTML = `Saving to <b>${esc(s.game || "Desktop")}</b>`;

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
  $("b-rec").className = s.recording ? "btn big quiet stop" : "btn big quiet";
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
    case "record": return invoke("toggle_recording");
    case "settings": return invoke("open_main");
  }
});

document.addEventListener("contextmenu", (e) => e.preventDefault());

function escape() {
  if (Gallery.isOpen()) Gallery.stop();
  else hide();
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

Gallery.mount($("gallery"), (open) => invoke("overlay_player", { open }));
opened();
