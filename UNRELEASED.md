# Unreleased

- The engine no longer crashes on every start when tape's own sounds are left out of game audio. It restarted over
  and over showing "the engine crashed (0xC0000374)" and recorded nothing.
- A chosen microphone or output that disappears, as when Windows adds a headset back under a new id, is replaced by
  the default device until it returns, instead of leaving that sound out of clips. The status says so.
- Saving while the engine restarts, or just before it stops, plays the failure sound and says the clip wasn't saved,
  instead of doing nothing.
- An image the video processor rejects, as can happen while a game changes display mode, no longer restarts the whole
  pipeline, and waking or unlocking the PC no longer moves Auto capture to Windows Graphics Capture for good.
- An engine that keeps crashing shows "The engine keeps crashing" instead of "Starting capture", and the counter says
  restarted rather than recovered.
- Giving a hotkey to a second action takes it off the first, instead of showing it as taken by another app.
- Cancelling a share right after starting it stops the conversion, and sharing a part of a clip from after its sound
  ended no longer hangs.
- A shared link is no longer deleted when the answer to its last upload step is lost on the way back. This needs the
  share server deployed again.
- An audio device without a name can no longer crash the engine.
