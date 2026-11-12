// A fake backend so the UI can run in a normal browser. See ui-preview.sh.
(() => {
  const q = new URLSearchParams(location.search);
  let shown = 0;
  const scene = q.get("scene") || "replay";
  const settings = {
    folder: "", monitor: "", resolution: 0, fps: 60, codec: "h264", bitrateMbps: 50, preset: "balanced", capture: "auto",
    showCursor: true, replay: scene !== "off", replaySeconds: 120, shortClipSeconds: 30, systemAudio: "default", excludeTapeAudio: true, microphone: scene === "replay" ? "" : "mic-1",
    systemVolume: 100, micVolume: 100, separateTracks: false,
    shareServer: q.get("cloud") === "off" ? "" : "https://tape-clips.example.workers.dev", shareKey: q.get("cloud") === "off" ? "" : "not-a-real-key",
    hotkeys: { saveClip: "Alt+F10", toggleRecording: "Alt+F9", toggleReplay: "Alt+Shift+F10", overlay: "Alt+Z", marker: "Alt+F8", shortClip: "Alt+F11" }, sounds: true,
  };
  const recording = scene === "recording";
  const trouble = scene === "trouble";
  const status = {
    video: { running: !trouble && scene !== "off" || recording, width: 1920, height: 1080, fps: 60, method: "Desktop Duplication", encodeMs: 4.1, dropped: 0, bitrateKbps: 41800, problem: null },
    system: { name: "Headphones (Arctis Nova 5)", running: true, peak: 0.42, problem: null, excludesTape: true },
    mic: settings.microphone ? { name: "Headset Microphone", running: true, peak: 0.18, problem: null } : null,
    replayOn: settings.replay, replaySeconds: 87.4, clipSeconds: 87.4, replayTarget: 120, replayBytes: 480e6,
    recording, recordingSeconds: recording ? 754 : 0, recordingBytes: recording ? 3.9e9 : 0, recordingPath: null, restarts: 0, game: "Counter-Strike 2",
    quality: { total: { droppedFrames: 0, lateFrames: 0, systemGlitches: 0, micGlitches: 0, slowDisk: 0 }, seconds: 900, recent: { droppedFrames: 0, lateFrames: 0, systemGlitches: 0, micGlitches: 0, slowDisk: 0 }, recentSeconds: 300 },
  };
  // ?quality=1 has something to report from the last few minutes.
  if (q.get("quality")) Object.assign(status.quality.recent, { droppedFrames: 12, systemGlitches: 1 });
  const now = Date.now();
  // Clips use sample.mp4 from next to this file when there is one. See ui-preview.sh.
  const pad = (n) => String(n).padStart(2, "0");
  const clip = (game, ago, size, kind = "clip") => {
    const d = new Date(now - ago * 60000);
    const name = `${game} ${d.getFullYear()}.${pad(d.getMonth() + 1)}.${pad(d.getDate())} - ${pad(d.getHours())}.${pad(d.getMinutes())}.${pad(d.getSeconds())}.11${kind === "clip" ? ".DVR" : ""}.mp4`;
    return { path: `sample.mp4?${game}${ago}`, name, game, kind, bytes: size, modified: d.getTime(), key: `${game}${ago}`.replace(/[^a-f0-9]/gi, "a"), thumb: q.get("thumbs") ? `thumb-${shown++}.jpg` : null, seconds: q.get("thumbs") ? 120 : null };
  };
  let view = {
    settings, status,
    probe: {
      gpu: "NVIDIA GeForce RTX 4070", driver: "32.0.15.6614", codecs: ["h264", "hevc", "av1"], nvencError: null,
      monitors: [
        { id: "\\\\.\\DISPLAY1", name: "Monitor 1", width: 1920, height: 1080, hz: 240, primary: true },
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
    folder: "C:\\Users\\me\\Videos\\Tape",
    clips: q.get("empty") ? [] : [
      clip("Counter-Strike 2", 12, 790e6), clip("Counter-Strike 2", 28, 760e6), clip("Fortnite", 95, 1.1e9),
      clip("Minecraft", 1400, 4.1e9, "recording"), clip("Desktop", 1600, 712e6), clip("Lethal Company", 2800, 803e6),
      clip("Lethal Company", 2830, 788e6), clip("Grand Theft Auto V", 4300, 790e6), clip("Fortnite", 5800, 760e6),
    ],
    autostart: true, overlayRunning: scene === "conflict", replayRam: 1.13e9, version: "0.2.0", logPath: "", selfTest: null, uiTest: false,
  };
  // ?cloud=0.85 fills the share server's storage that far; ?cloud=off leaves sharing unset.
  const full = Number(q.get("cloud") || 0.18);
  const link = (c, days, bytes) => ({ id: c.key.slice(0, 10), url: `https://tape-clips.example.workers.dev/${c.key.slice(0, 10)}`, name: c.name, source: c.key, bytes, seconds: 120, uploaded: now - (7 - days) * 864e5, expires: now + days * 864e5 });
  const usage = { used: 0, free: 1e10, cap: 9.5e9, linkDays: 7, clips: view.clips.length ? [link(view.clips[0], 6.8, 92e6), link(view.clips[2], 3.2, 140e6), link(view.clips[5], 0.4, 75e6)] : [] };
  usage.used = Math.max(full * 1e10, usage.clips.reduce((a, l) => a + l.bytes, 0));
  const listeners = {};

  const emit = (name, payload) => (listeners[name] || []).forEach((f) => f({ payload }));
  // ?update=1 has a new version waiting.
  const upd = { current: "0.3.0", available: q.get("update") ? "0.3.1" : null, notes: q.get("update") ? "- Better things." : null, checking: false, checked: true, progress: null, error: null };
  const calls = [];
  window.__mock = { emit, calls };
  window.__TAURI__ = {
    core: {
      convertFileSrc: (path) => path,
      invoke: async (cmd, args) => {
        calls.push(cmd);
        if (cmd === "view") return structuredClone(view);
        if (cmd === "save_settings") {
          view.settings = args.settings;
          view.replayRam = args.settings.bitrateMbps * 1e6 * 1.5 / 8 * (args.settings.replaySeconds + 3);
          return structuredClone(view);
        }
        if (cmd === "set_autostart") return args.enabled;
        if (cmd === "update_state") return { ...upd };
        if (cmd === "check_update") return { ...upd };
        if (cmd === "install_update") {
          if (scene === "recording") throw "Stop the recording first.";
          for (const progress of [0, 40, 100]) {
            await new Promise((r) => setTimeout(r, 60));
            emit("update", { ...upd, progress });
          }
          return;
        }
        if (cmd === "clips") return structuredClone(view.clips);
        if (cmd === "save_thumb") {
          const jpeg = new Blob([new Uint8Array(args.jpeg)], { type: "image/jpeg" });
          const thumb = URL.createObjectURL(jpeg);
          Object.assign(view.clips.find((c) => c.key === args.key), { thumb, seconds: args.seconds });
          return thumb;
        }
        if (cmd === "clip_info") return { duration: 20, keyframes: Array.from({ length: 10 }, (_, i) => i * 2) };
        if (cmd === "trim_clip") {
          const c = view.clips.find((x) => x.path === args.path);
          const name = c.name.replace(/(\.DVR)?\.mp4$/i, (end) => ` trim${end}`);
          const made = { ...c, path: `${c.path}&trim${calls.length}`, name, key: `${c.key}b${calls.length}`, modified: Date.now(), seconds: args.end - args.start, thumb: null, starred: false };
          view.clips.unshift(made);
          return made.path;
        }
        if (cmd === "delete_clip") {
          view.clips = view.clips.filter((c) => c.path !== args.path);
          return;
        }
        if (cmd === "rename_clip") {
          const c = view.clips.find((x) => x.path === args.path);
          if (!args.name.trim()) throw "That name has nothing a file name can use.";
          c.name = `${args.name}${c.kind === "clip" ? ".DVR" : ""}.mp4`;
          c.path = `${c.path.split("#")[0]}#${encodeURIComponent(args.name)}`;
          return c.path;
        }
        if (cmd === "star_clip") {
          view.clips.find((x) => x.path === args.path).starred = args.on;
          return;
        }
        if (cmd === "share_usage") return settings.shareServer ? structuredClone(usage) : null;
        if (cmd === "share_jobs") return [];
        if (cmd === "copy_text" || cmd === "cancel_share") return;
        if (cmd === "unshare") {
          const gone = usage.clips.find((l) => l.id === args.id);
          usage.clips = usage.clips.filter((l) => l !== gone);
          usage.used -= gone ? gone.bytes : 0;
          return structuredClone(usage);
        }
        if (cmd === "share_clip") {
          const c = view.clips.find((x) => x.path === args.path);
          for (const [stage, progress] of [["converting", 0.3], ["converting", 0.9], ["uploading", 0.5], ["uploading", 1]]) {
            await new Promise((r) => setTimeout(r, 250));
            emit("share", { path: args.path, stage, progress });
          }
          const l = link(c, 7, 80e6);
          usage.clips.unshift(l);
          usage.used += l.bytes;
          emit("share-usage", structuredClone(usage));
          emit("share", { path: args.path, stage: "done", progress: 1, url: l.url }); // as the app does, to every window
          return l.url;
        }
        if (cmd === "save_clip" || cmd === "save_short_clip") return emit("saved", "C:\\Users\\me\\Videos\\Tape\\Counter-Strike 2\\Counter-Strike 2 2026.11.01 - 21.30.00.11.DVR.mp4");
        if (cmd === "self_test") return setTimeout(() => emit("selftest", "tape 0.1.0 self-test\nWindows 10.0.26100\n\nCapture\n  Desktop Duplication: OK"), 300);
      },
    },
    event: { listen: async (name, f) => { (listeners[name] ||= []).push(f); } },
    window: { getCurrentWindow: () => ({ minimize() {}, close() {} }) },
  };
})();
