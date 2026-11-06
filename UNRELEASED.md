# Unreleased

- Capture recovers after the display setup changes (a monitor sleeping or waking, a game switching resolution, HDR being
  turned on or off). Before, both capture methods kept retrying with the monitor as it was when tape started, and failed
  with "Could not capture the given monitor" until tape was restarted. Auto now also goes back to Desktop Duplication
  instead of staying on Windows Graphics Capture for good.
- Desktop Duplication works on HDR monitors.
- Microphone Sync setting (Settings > Audio, 0 to 300 ms) moves the mic earlier in clips, for headsets and mic software
  that deliver it late. Mic packets whose timestamps the driver flags as wrong are placed by arrival time instead.
- The clip player is a popup over the dimmed window. Switching tabs or pressing Esc closes it, so a clip no longer keeps
  playing behind the Capture tab. In the Alt+Z panel the player opens beside the panel, which stays in place, dimmed;
  clicking the panel closes the player.
