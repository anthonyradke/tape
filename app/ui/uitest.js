// Runs in the real app only when CI sets TAPE_UI_TEST, with the engine on its fake backend. It saves
// a clip and a recording, then checks they play in the webview, which nothing short of a real
// WebView2 can show: real H.264 and AAC decoders, the asset protocol's range requests, seeking in
// both MP4 layouts, and thumbnails from a cross-origin video.
"use strict";

(async () => {
  const results = {};
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const nextEvent = (name, ms) =>
    new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`no ${name} event`)), ms);
      listen(name, (e) => {
        clearTimeout(timer);
        resolve(e.payload);
      });
    });

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

  try {
    for (let i = 0; i < 100 && !(view.status.replaySeconds > 3); i++) {
      await sleep(200);
      view = await invoke("view");
    }
    results.replaySeconds = view.status.replaySeconds;
    const clipSaved = nextEvent("saved", 15000);
    await invoke("save_clip");
    results.clipPath = await clipSaved;

    await invoke("toggle_recording");
    await sleep(3000);
    const recordingSaved = nextEvent("saved", 15000);
    await invoke("toggle_recording");
    results.recordingPath = await recordingSaved;

    const clips = await invoke("clips");
    results.count = clips.length;
    results.clips = [];
    for (const clip of clips) results.clips.push(await checkVideo(clip));
    const again = await invoke("clips");
    results.thumbsListed = again.filter((c) => c.thumb && c.seconds > 0).length;

    results.overlayOpen = await invoke("ui_test_overlay");
    results.overlayClosed = await invoke("ui_test_overlay");
  } catch (e) {
    results.failure = String(e);
  }
  await invoke("ui_test_done", { report: JSON.stringify(results, null, 2) });
})();
