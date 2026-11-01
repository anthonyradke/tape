// The panel that opens over a game. Clicking away, Esc, Alt+Tab or the hotkey all close it, and
// nothing here is ever left covering the screen.
"use strict";

// Windows can report a lost focus while a window is still being brought forward.
const IGNORE_BLUR_MS = 600;
let openedAt = Date.now();

function render() {
  if (document.activeElement?.matches?.("input[type=range]")) return; // don't move a slider in use
  const { settings: s, probe: p, status } = view;
  $("ov-settings").innerHTML = `<div class="group"><span class="cap">Video</span>
      ${row("Resolution", select("resolution", heightOptions(), s.resolution))}
      ${row("Frame rate", select("fps", FPS.map((f) => [f, `${f} fps`]), s.fps))}
      ${row("Bitrate", range("bitrateMbps", 5, 150, 5, " Mbps", "Bitrate"))}
      ${status.recording ? `<p class="note warn">Applies when the recording stops.</p>` : ""}
    </div>
    <div class="group"><span class="cap">Instant replay</span>
      ${row("On", toggle("replay", s.replay, "Instant replay"))}
      ${row("Keep the last", select("replaySeconds", LENGTHS, s.replaySeconds))}
    </div>
    <div class="group"><span class="cap">Audio</span>
      ${row("Game audio", select("systemAudio", deviceOptions(p.outputs, s.systemAudio, "Off"), s.systemAudio))}
      ${row("Microphone", select("microphone", deviceOptions(p.inputs, s.microphone, "Off"), s.microphone))}
    </div>`;
  $("k-clip").textContent = s.hotkeys.saveClip;
  $("k-rec").textContent = s.hotkeys.toggleRecording;
  renderStatus(status);
}

function renderStatus(s) {
  view.status = s;
  const light = view.health.light;
  const state = $("tb-state");
  state.className = `tb-state ${light}`;
  state.textContent = { idle: "Off", replay: "Instant replay on", recording: "Recording", trouble: view.health.problem || "Not capturing" }[light];
  $("ov-game").textContent = s.game;

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
  $("b-clip-text").textContent = s.replayOn ? `Save last ${clock(Math.min(s.replaySeconds, s.replayTarget))}` : "Save clip";
  $("b-rec").className = s.recording ? "btn big quiet stop" : "btn big quiet";
  $("b-rec-text").textContent = s.recording ? "Stop recording" : "Start recording";
}

function hide() {
  Gallery.stop();
  invoke("hide_overlay");
}

async function opened() {
  openedAt = Date.now();
  await refresh();
  Gallery.load();
}

document.addEventListener("click", (e) => {
  const target = e.target.closest("[data-act],[data-seg],[data-toggle]");
  if (!target) return;
  const { act, toggle: toggleKey } = target.dataset;
  if (toggleKey) return save((s) => (s[toggleKey] = !s[toggleKey]));
  switch (act) {
    case "hide": return hide();
    case "clip": return invoke("save_clip");
    case "record": return invoke("toggle_recording");
    case "settings": return invoke("open_main");
  }
});

document.addEventListener("contextmenu", (e) => e.preventDefault());

document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (Gallery.isOpen()) Gallery.stop();
  else hide();
});

window.addEventListener("blur", () => {
  if (Date.now() - openedAt > IGNORE_BLUR_MS) hide();
});

listen("status", (e) => view && renderStatus(e.payload));
listen("changed", refresh);
listen("error", (e) => toast(e.payload, true));
listen("saved", (e) => {
  toast(`Saved ${e.payload.split(/[\\/]/).pop()}`);
  refresh();
  Gallery.load();
});
listen("overlay-shown", opened);
listen("overlay-hidden", () => Gallery.stop());

Gallery.mount($("gallery"));
opened();
