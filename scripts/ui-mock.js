// A fake backend so the UI can run in a normal browser. See ui-preview.sh.
(() => {
  const q = new URLSearchParams(location.search);
  const scene = q.get("scene") || "replay";
  const settings = {
    folder: "", monitor: "", resolution: 0, fps: 60, codec: "h264", bitrateMbps: 50, preset: "balanced", capture: "auto",
    showCursor: true, replay: scene !== "off", replaySeconds: 120, systemAudio: "default", microphone: scene === "replay" ? "" : "mic-1",
    systemVolume: 100, micVolume: 100, separateTracks: false,
    hotkeys: { saveClip: "Alt+F10", toggleRecording: "Alt+F9", toggleReplay: "Alt+Shift+F10" }, sounds: true,
  };
  const recording = scene === "recording";
  const trouble = scene === "trouble";
  const status = {
    video: { running: !trouble && scene !== "off" || recording, width: 2560, height: 1440, fps: 60, method: "Desktop Duplication", encodeMs: 4.1, dropped: 0, bitrateKbps: 41800, problem: null },
    system: { name: "Headphones (Arctis Nova 5)", running: true, peak: 0.42, problem: null },
    mic: settings.microphone ? { name: "Headset Microphone", running: true, peak: 0.18, problem: null } : null,
    replayOn: settings.replay, replaySeconds: 87.4, replayTarget: 120, replayBytes: 480e6,
    recording, recordingSeconds: recording ? 754 : 0, recordingBytes: recording ? 3.9e9 : 0, recordingPath: null, restarts: 0,
  };
  const now = Date.now();
  const clip = (name, ago, size) => ({ path: `C:\\Users\\me\\Videos\\tape\\${name}.mp4`, name: `${name}.mp4`, bytes: size, modified: now - ago * 60000 });
  let view = {
    settings, status,
    probe: {
      gpu: "NVIDIA GeForce RTX 4070", driver: "32.0.15.6614", codecs: ["h264", "hevc", "av1"], nvencError: null,
      monitors: [
        { id: "\\\\.\\DISPLAY1", name: "Monitor 1", width: 2560, height: 1440, hz: 165, primary: true },
        { id: "\\\\.\\DISPLAY2", name: "Monitor 2", width: 1920, height: 1080, hz: 60, primary: false },
      ],
      outputs: [{ id: "out-1", name: "Headphones (Arctis Nova 5)" }, { id: "out-2", name: "Speakers (Realtek Audio)" }],
      inputs: [{ id: "mic-1", name: "Headset Microphone (Arctis Nova 5)" }],
    },
    health: {
      light: trouble ? "trouble" : recording ? "recording" : scene === "off" ? "idle" : "replay",
      problem: trouble ? "Desktop Duplication: access denied" : null,
      engineRestarts: trouble ? 2 : 0, lastRestart: trouble ? "engine stopped responding" : null,
    },
    hotkeyErrors: scene === "conflict" ? ["Alt+F10"] : [],
    folder: "C:\\Users\\me\\Videos\\tape",
    clips: q.get("empty") ? [] : [
      clip("Valheim 2026-10-31 21-14-03", 12, 790e6), clip("Valheim 2026-10-31 20-58-41", 28, 760e6),
      clip("Minecraft 2026-10-30 22-10-17", 1400, 4.1e9), clip("2026-10-30 19-02-55", 1600, 712e6),
      clip("Counter-Strike 2 2026-10-29 23-41-09", 2800, 803e6), clip("Counter-Strike 2 2026-10-29 23-12-30", 2830, 788e6),
    ],
    autostart: true, overlayRunning: scene === "conflict", replayRam: 1.13e9, version: "0.1.0", logPath: "", selfTest: null,
  };
  const listeners = {};
  const emit = (name, payload) => (listeners[name] || []).forEach((f) => f({ payload }));
  window.__mock = { emit };
  window.__TAURI__ = {
    core: {
      invoke: async (cmd, args) => {
        if (cmd === "view") return structuredClone(view);
        if (cmd === "save_settings") {
          view.settings = args.settings;
          view.replayRam = args.settings.bitrateMbps * 1e6 * 1.5 / 8 * (args.settings.replaySeconds + 3);
          return structuredClone(view);
        }
        if (cmd === "set_autostart") return args.enabled;
        if (cmd === "save_clip") return emit("saved", "C:\\Users\\me\\Videos\\tape\\Valheim 2026-10-31 21-30-00.mp4");
        if (cmd === "self_test") return setTimeout(() => emit("selftest", "tape 0.1.0 self-test\nWindows 10.0.26100\n\nCapture\n  Desktop Duplication: OK"), 300);
      },
    },
    event: { listen: async (name, f) => { (listeners[name] ||= []).push(f); } },
    window: { getCurrentWindow: () => ({ minimize() {}, close() {} }) },
  };
})();
