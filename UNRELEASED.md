# Unreleased

- Clicking Update twice, or in Settings and the tray at once, starts one download instead of two.
- Checking for updates or downloading one gives up after a while on a stalled connection, instead of leaving Update stuck until tape restarts.
- When the disk can't keep up with a recording, the file now ends cleanly where it fell behind, instead of having a stretch of broken picture and missing sound at the end.
- A settings file another program has open for a moment at sign-in is read again, instead of being replaced with the defaults.
- Saving a trim twice at once, as with a double click, makes two copies instead of one broken file.
- Stopping a long recording on a slow drive no longer gets the engine restarted as unresponsive, which lost the replay buffer and could leave the file unfinished.
- The gallery waits until a stopped recording is fully saved before showing it, so repairing, renaming or deleting it can't happen mid-save.
