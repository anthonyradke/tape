// Runs in the real app only when CI sets TAPE_UI_TEST, with the engine on its fake backend. It saves
// a clip and a recording, then checks they play in the webview, which nothing short of a real
// WebView2 can show: real H.264 and AAC decoders, the asset protocol's range requests, seeking in
// both MP4 layouts, and thumbnails from a cross-origin video.
"use strict";

(async () => {
  const results = {};
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  // Listening starts here, once, so an event can't slip past between two steps.
  const saved = [];
  const errors = [];
  await listen("saved", (e) => saved.push(e.payload));
  await listen("error", (e) => errors.push(e.payload));
  async function nthSaved(n, ms) {
    for (const start = Date.now(); saved.length < n; await sleep(50)) {
      if (Date.now() - start > ms) throw new Error(`only ${saved.length} of ${n} saved events`);
    }
    return saved[n - 1];
  }

  async function checkVideo(clip) {
    const out = { game: clip.game, kind: clip.kind, name: clip.name };
    try {
      const made = await Gallery.makeThumb(clip);
      out.thumb = !!made.thumb;
      out.seconds = made.seconds;
    } catch (e) {
      out.thumb = false;
      out.thumbError = String(e);
    }
    await new Promise((resolve) => {
      const video = document.createElement("video");
      video.muted = true;
      video.addEventListener("error", () => {
        out.playError = video.error?.message || "error";
        resolve();
      });
      video.addEventListener("canplay", async () => {
        out.width = video.videoWidth;
        out.duration = video.duration;
        await video.play();
        await sleep(1500);
        out.playedTo = video.currentTime;
        out.frames = video.getVideoPlaybackQuality?.().totalVideoFrames;
        out.audioBytes = video.webkitAudioDecodedByteCount;
        video.addEventListener("seeked", () => {
          out.seekedTo = video.currentTime;
          video.pause();
          resolve();
        }, { once: true });
        video.currentTime = video.duration * 0.75;
      }, { once: true });
      document.body.append(video);
      video.src = convertFileSrc(clip.path);
    });
    return out;
  }

  // Trims the recording, then stars, renames and deletes the copy, on real files.
  async function library(source) {
    const out = {};
    const find = async (path) => (await invoke("clips")).find((c) => c.path === path);
    const info = await invoke("clip_info", { path: source });
    out.keyframes = info.keyframes.length;
    out.duration = info.duration;
    // From just after the second keyframe, so the start has to snap back to it.
    const start = info.keyframes[1] + 0.1;
    const trimmed = await invoke("trim_clip", { path: source, start, end: info.duration - 0.2 });
    out.trimmedName = trimmed.split(/[\\/]/).pop();
    const copy = await find(trimmed);
    out.trimmedKind = copy?.kind;
    out.trimmed = copy ? await checkVideo(copy) : null;
    out.expectedSeconds = info.duration - 0.2 - info.keyframes[1];

    await invoke("star_clip", { path: trimmed, on: true });
    out.starred = !!(await find(trimmed))?.starred;
    const renamed = await invoke("rename_clip", { path: trimmed, name: "renamed by the test" });
    out.renamedName = renamed.split(/[\\/]/).pop();
    const after = await find(renamed);
    out.renamedStarred = !!after?.starred;
    out.renamedThumb = !!after?.thumb;
    out.oldNameGone = !(await find(trimmed));
    // The copy and the recording share an ending, so taking the recording's name has to fail.
    try {
      await invoke("rename_clip", { path: renamed, name: source.split(/[\\/]/).pop().replace(/\.mp4$/i, "") });
      out.clashRefused = false;
    } catch {
      out.clashRefused = true;
    }
    await invoke("delete_clip", { path: renamed });
    out.afterDelete = (await invoke("clips")).length;
    try {
      await invoke("delete_clip", { path: "C:\\Windows\\win.ini" });
      out.outsideRefused = false;
    } catch {
      out.outsideRefused = true;
    }
    return out;
  }

  try {
    for (let i = 0; i < 100 && !(view.status.replaySeconds > 3); i++) {
      await sleep(200);
      view = await invoke("view");
    }
    results.replaySeconds = view.status.replaySeconds;
    await invoke("save_clip");
    results.clipPath = await nthSaved(1, 15000);

    await invoke("toggle_recording");
    await sleep(3000);
    results.recordingStatus = (await invoke("view")).status;
    await invoke("toggle_recording");
    results.recordingPath = await nthSaved(2, 15000);

    const clips = await invoke("clips");
    results.count = clips.length;
    results.clips = [];
    for (const clip of clips) results.clips.push(await checkVideo(clip));
    const again = await invoke("clips");
    results.thumbsListed = again.filter((c) => c.thumb && c.seconds > 0).length;

    results.library = await library(results.recordingPath);

    results.overlayOpen = await invoke("ui_test_overlay");
    results.overlayClosed = await invoke("ui_test_overlay");
  } catch (e) {
    results.failure = String(e);
    const end = await invoke("view");
    results.endStatus = end.status;
    results.endHealth = end.health;
    results.onDisk = (await invoke("clips")).map((c) => `${c.name} ${c.bytes}`);
  }
  results.errors = errors;
  await invoke("ui_test_done", { report: JSON.stringify(results, null, 2) });
})();
