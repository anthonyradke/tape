# Unreleased

- Alt+F11 saves a short clip, the last 30 seconds by default, and the Alt+Z panel has a button for it. The length
  is in Settings.
- A new hotkey, unset by default, saves a clip and copies a link to it, with a tone once the link is on the
  clipboard. The Alt+Z panel has a "Save and copy link" button when sharing is set up.
- Alt+F8 adds a marker during a recording. Markers are saved as chapters, which players show on the seek bar, and
  the trim handles snap to them.
- Replay length and bitrate can be set per game in Settings. They apply while that game is in front, and changing
  the bitrate no longer empties the replay buffer.
- With the Windows default output device, tape's own sounds and clips played in it are left out of game audio.
- Recordings left unfinished by a crash or power cut are marked in the gallery and can be repaired in place.
- Copy link can share just the trim selection, and shared copies now include the mic when it's on its own track.
  Link previews name the game and the length.
- The clip player has keyboard shortcuts: Space, J/K/L, arrows, comma and period, I and O for the trim points, N and
  P for the next and previous clip, and Delete.
- A quiet line under the status shows dropped frames, audio glitches or a slow disk from the last few minutes.
- Settings warns when the replay buffer won't fit in memory and greys out output sizes the encoder can't make.
  The buffer is held to 40% of the PC's memory.
- A crash during a recording now loses at most two seconds, not four, and finishing a recording can no longer leave
  a file that won't play. A recording that hits a write error is left playable instead of being finished wrongly.
- Recordings shorter than four seconds are now reported as saved.
- Back-to-back clips no longer lose a fraction of a second of sound where they meet.
- Locking the PC, a UAC prompt or a monitor turning off no longer plays the failure tone.
- Hotkeys come back after an unfinished key change, and keys another app held at startup are retried. The tray shows
  trouble while the save key is taken.
- A setting file with one bad value keeps the rest, and the damaged file is kept as `settings.json.bad`.
- Rotated monitors are recorded upright, and output sizes past the encoder's limit are scaled to fit.
- The clip player no longer acts on a different clip after a save adds one to the gallery.
- Shared links stay attached to renamed clips, cancelling a share no longer shows an error, and uploads retry on
  network errors.
- A failed update restarts tape instead of leaving it running without its engine, and clips go to the Videos folder
  Windows reports when it has been moved.
