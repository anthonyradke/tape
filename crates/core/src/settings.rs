use serde::{Deserialize, Serialize};

use crate::packet::Codec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Fast,
    Balanced,
    Quality,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMethod {
    /// Desktop Duplication, falling back to Windows Graphics Capture.
    Auto,
    Duplication,
    Graphics,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Hotkeys {
    pub save_clip: String,
    pub toggle_recording: String,
    pub toggle_replay: String,
    pub overlay: String,
    /// Saves only the last `Settings::short_clip_seconds`.
    pub short_clip: String,
    /// Marks a moment in a recording.
    pub marker: String,
}

impl Default for Hotkeys {
    /// The same keys ShadowPlay uses, so muscle memory carries over.
    fn default() -> Self {
        Hotkeys {
            save_clip: "Alt+F10".into(),
            toggle_recording: "Alt+F9".into(),
            toggle_replay: "Alt+Shift+F10".into(),
            overlay: "Alt+Z".into(),
            // Beside the save key. Neither Windows nor the NVIDIA app uses it.
            short_clip: "Alt+F11".into(),
            // Next to record. ShadowPlay had it for broadcasting, which the NVIDIA app dropped.
            marker: "Alt+F8".into(),
        }
    }
}

pub const FOLLOW_DEFAULT: &str = "default";
pub const NO_DEVICE: &str = "";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Empty means Videos\Tape. Clips go in a folder per game under it.
    pub folder: String,
    /// Output device name, empty for the primary monitor.
    pub monitor: String,
    /// Output size in pixels along the short edge, as in 1440p, 0 for the monitor's own resolution.
    /// Can be above the monitor's, which upscales.
    pub resolution: u32,
    pub fps: u32,
    pub codec: Codec,
    pub bitrate_mbps: u32,
    pub preset: Preset,
    pub capture: CaptureMethod,
    pub show_cursor: bool,
    pub replay: bool,
    pub replay_seconds: u32,
    /// Length of a clip from the short clip key, cut to the replay length if that's shorter.
    pub short_clip_seconds: u32,
    /// Output device to record: "default" follows Windows, "" records none, otherwise an id.
    pub system_audio: String,
    /// Leave tape's own sounds (its cues and clips played in it) out of game audio. Windows can
    /// only do that for the default output device, so a chosen device ignores this.
    pub exclude_tape_audio: bool,
    /// Input device, same convention.
    pub microphone: String,
    pub system_volume: u32,
    pub mic_volume: u32,
    /// How far the mic is moved earlier, for headsets and mic software that deliver it late.
    pub mic_delay_ms: u32,
    /// Keep game audio and mic as separate tracks instead of mixing them.
    pub separate_tracks: bool,
    pub hotkeys: Hotkeys,
    pub sounds: bool,
    /// Address of the share server behind "Copy link", empty until set up. See share/README.md.
    pub share_server: String,
    /// Its upload key, which lets this app upload and delete links.
    pub share_key: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            folder: String::new(),
            monitor: String::new(),
            resolution: 0,
            fps: 60,
            codec: Codec::H264,
            bitrate_mbps: 50,
            preset: Preset::Balanced,
            capture: CaptureMethod::Auto,
            show_cursor: true,
            replay: true,
            replay_seconds: 120,
            short_clip_seconds: 30,
            system_audio: FOLLOW_DEFAULT.into(),
            exclude_tape_audio: true,
            microphone: NO_DEVICE.into(),
            system_volume: 100,
            mic_volume: 100,
            mic_delay_ms: 0,
            separate_tracks: false,
            hotkeys: Hotkeys::default(),
            sounds: true,
            share_server: String::new(),
            share_key: String::new(),
        }
    }
}

pub const AUDIO_KBPS: u32 = 192;
/// The mixer waits this much longer for the mic at most, so it's kept short.
pub const MAX_MIC_DELAY_MS: u32 = 300;
/// Peak bitrate as a multiple of the target; the encoder runs VBR.
const PEAK: f64 = 1.5;

impl Settings {
    /// Reads a settings file field by field: anything that doesn't fit (a wrong type, a value
    /// from a newer version) falls back to its default and the rest is kept, so one bad value
    /// can't wipe the hotkeys, folder and share key. The flag says whether anything was dropped.
    pub fn from_json_lenient(text: &str) -> (Settings, bool) {
        let Ok(serde_json::Value::Object(file)) = serde_json::from_str::<serde_json::Value>(text) else {
            return (Settings::default(), true);
        };
        let Ok(serde_json::Value::Object(mut accepted)) = serde_json::to_value(Settings::default()) else {
            return (Settings::default(), true);
        };
        let mut dropped = false;
        merge(&mut accepted, &file, &mut dropped, &|candidate| {
            serde_json::from_value::<Settings>(serde_json::Value::Object(candidate.clone())).is_ok()
        });
        let settings = serde_json::from_value(serde_json::Value::Object(accepted)).unwrap_or_default();
        (settings, dropped)
    }

    /// Forces every value into a range the engine can run, so a hand-edited or old settings file
    /// can never stop it starting.
    pub fn normalized(mut self) -> Self {
        self.fps = self.fps.clamp(15, 360);
        self.bitrate_mbps = self.bitrate_mbps.clamp(1, 500);
        self.replay_seconds = self.replay_seconds.clamp(5, 3600);
        self.short_clip_seconds = self.short_clip_seconds.clamp(5, 3600);
        self.system_volume = self.system_volume.min(200);
        self.mic_volume = self.mic_volume.min(200);
        self.mic_delay_ms = self.mic_delay_ms.min(MAX_MIC_DELAY_MS);
        if self.resolution != 0 {
            self.resolution = self.resolution.clamp(144, 4320);
        }
        self.share_server = self.share_server.trim().trim_end_matches('/').to_string();
        self.share_key = self.share_key.trim().to_string();
        self
    }

    /// Worst-case RAM for the replay buffer, for showing next to the length setting.
    pub fn replay_ram_bytes(&self) -> u64 {
        let tracks = if self.separate_tracks { 2 } else { 1 };
        let audio = (AUDIO_KBPS * tracks) as f64 * 1000.0;
        let video = self.bitrate_mbps as f64 * 1e6 * PEAK;
        // The ring keeps a few seconds past the replay length to reach a keyframe.
        ((video + audio) / 8.0 * (self.replay_seconds + 3) as f64) as u64
    }

    pub fn peak_kbps(&self) -> u32 {
        (self.bitrate_mbps as f64 * 1000.0 * PEAK) as u32
    }

    /// Output size for a monitor, up or down, keeping its shape, with even dimensions as the encoder
    /// needs. Goes by the short edge, so 1440p means the same on a monitor turned on its side.
    /// `max` is the largest the encoder takes (H.264 on NVENC stops at 4096 wide, which a 21:9
    /// monitor at 2160p passes); a size over it is scaled down to fit, still keeping the shape.
    pub fn output_size(&self, width: u32, height: u32, max: (u32, u32)) -> (u32, u32) {
        let short = width.min(height).max(1) as u64;
        let scale = |edge: u32| edge as u64 * self.resolution as u64 / short;
        let (mut w, mut h) = if self.resolution == 0 { (width as u64, height as u64) } else { (scale(width), scale(height)) };
        let (max_w, max_h) = (max.0.max(2) as u64, max.1.max(2) as u64);
        if w > max_w || h > max_h {
            // Whichever edge is further over decides the factor.
            if w * max_h >= h * max_w {
                (w, h) = (max_w, h * max_w / w.max(1));
            } else {
                (w, h) = (w * max_h / h.max(1), max_h);
            }
        }
        (w as u32 & !1, h as u32 & !1)
    }

    /// Whether changing from `other` means restarting the encoder.
    pub fn video_differs(&self, other: &Settings) -> bool {
        self.monitor != other.monitor
            || self.resolution != other.resolution
            || self.fps != other.fps
            || self.codec != other.codec
            || self.bitrate_mbps != other.bitrate_mbps
            || self.preset != other.preset
            || self.capture != other.capture
            || self.show_cursor != other.show_cursor
    }

    /// Whether changing from `other` means reopening audio devices.
    pub fn audio_differs(&self, other: &Settings) -> bool {
        self.system_audio != other.system_audio
            || self.exclude_tape_audio != other.exclude_tape_audio
            || self.microphone != other.microphone
            || self.separate_tracks != other.separate_tracks
    }
}

type Map = serde_json::Map<String, serde_json::Value>;

/// Copies each field of `file` that `fits` into `into`, field by field. Unknown keys are left
/// out as serde would; nested objects (the hotkeys) are merged the same way, one key at a time.
fn merge(into: &mut Map, file: &Map, dropped: &mut bool, fits: &dyn Fn(&Map) -> bool) {
    for (key, value) in file {
        let Some(old) = into.get(key).cloned() else { continue };
        into.insert(key.clone(), value.clone());
        if fits(into) {
            continue;
        }
        into.insert(key.clone(), old.clone());
        match (old, value) {
            (serde_json::Value::Object(mut inner), serde_json::Value::Object(given)) => {
                let fits_inner = |candidate: &Map| {
                    let mut whole = into.clone();
                    whole.insert(key.clone(), serde_json::Value::Object(candidate.clone()));
                    fits(&whole)
                };
                merge(&mut inner, given, dropped, &fits_inner);
                into.insert(key.clone(), serde_json::Value::Object(inner));
            }
            _ => *dropped = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_files_fill_in_defaults() {
        let s: Settings = serde_json::from_str(r#"{"fps": 144, "somethingOld": 1}"#).unwrap();
        assert_eq!(s.fps, 144);
        assert_eq!(s.bitrate_mbps, 50);
        assert_eq!(s.hotkeys.save_clip, "Alt+F10");
    }

    #[test]
    fn settings_saved_before_the_overlay_existed_get_its_key() {
        let s: Settings = serde_json::from_str(r#"{"hotkeys": {"saveClip": "F8"}}"#).unwrap();
        assert_eq!((s.hotkeys.save_clip.as_str(), s.hotkeys.overlay.as_str()), ("F8", "Alt+Z"));
    }

    fn lenient(text: &str) -> (Settings, bool) {
        Settings::from_json_lenient(text)
    }

    const KEPT: &str = r#""folder": "D:\\Clips", "shareKey": "k", "hotkeys": {"saveClip": "F8"}"#;

    fn assert_kept(s: &Settings) {
        assert_eq!((s.folder.as_str(), s.share_key.as_str(), s.hotkeys.save_clip.as_str()), ("D:\\Clips", "k", "F8"));
    }

    #[test]
    fn a_float_where_a_whole_number_goes_keeps_everything_else() {
        let (s, dropped) = lenient(&format!(r#"{{{KEPT}, "systemVolume": 100.0, "fps": 144}}"#));
        assert!(dropped);
        assert_kept(&s);
        assert_eq!((s.system_volume, s.fps), (100, 144));
    }

    #[test]
    fn a_null_string_keeps_everything_else() {
        let (s, dropped) = lenient(&format!(r#"{{{KEPT}, "monitor": null}}"#));
        assert!(dropped);
        assert_kept(&s);
        assert_eq!(s.monitor, "");
    }

    #[test]
    fn a_negative_volume_keeps_everything_else() {
        let (s, dropped) = lenient(&format!(r#"{{{KEPT}, "micVolume": -1}}"#));
        assert!(dropped);
        assert_kept(&s);
        assert_eq!(s.mic_volume, 100);
    }

    #[test]
    fn an_unknown_preset_from_a_newer_version_keeps_everything_else() {
        let (s, dropped) = lenient(&format!(r#"{{{KEPT}, "preset": "lossless", "codec": "hevc"}}"#));
        assert!(dropped);
        assert_kept(&s);
        assert_eq!((s.preset, s.codec), (Preset::Balanced, Codec::Hevc));
    }

    #[test]
    fn one_bad_hotkey_keeps_the_others() {
        let (s, dropped) = lenient(r#"{"hotkeys": {"saveClip": "F8", "overlay": 5}}"#);
        assert!(dropped);
        assert_eq!((s.hotkeys.save_clip.as_str(), s.hotkeys.overlay.as_str()), ("F8", "Alt+Z"));
    }

    #[test]
    fn a_good_file_drops_nothing() {
        let text = serde_json::to_string(&Settings { fps: 144, folder: "x".into(), ..Default::default() }).unwrap();
        let (s, dropped) = lenient(&text);
        assert!(!dropped);
        assert_eq!(s, Settings { fps: 144, folder: "x".into(), ..Default::default() });
        // Unknown keys are what older and newer versions leave, not damage.
        assert!(!lenient(r#"{"somethingOld": 1}"#).1);
    }

    #[test]
    fn a_file_that_isnt_json_is_reported() {
        assert_eq!(lenient("{\"fps\": 14"), (Settings::default(), true));
        assert_eq!(lenient("[]"), (Settings::default(), true));
    }

    #[test]
    fn nonsense_values_are_clamped() {
        let s = Settings { fps: 0, bitrate_mbps: 99_999, replay_seconds: 0, resolution: 3, ..Default::default() };
        let s = s.normalized();
        assert_eq!((s.fps, s.bitrate_mbps, s.replay_seconds, s.resolution), (15, 500, 5, 144));
    }

    const NO_LIMIT: (u32, u32) = (u32::MAX, u32::MAX);

    #[test]
    fn output_keeps_aspect_and_stays_even() {
        let s = Settings { resolution: 1080, ..Default::default() };
        assert_eq!(s.output_size(2560, 1440, NO_LIMIT), (1920, 1080));
        assert_eq!(s.output_size(3440, 1440, NO_LIMIT), (2580, 1080));
        let native = Settings::default();
        assert_eq!(native.output_size(1367, 769, NO_LIMIT), (1366, 768));
    }

    #[test]
    fn output_can_be_larger_than_the_monitor() {
        let at = |resolution| Settings { resolution, ..Default::default() }.output_size(1920, 1080, NO_LIMIT);
        assert_eq!(at(1440), (2560, 1440));
        assert_eq!(at(2160), (3840, 2160));
    }

    #[test]
    fn resolution_goes_by_the_short_edge() {
        let s = Settings { resolution: 1440, ..Default::default() };
        assert_eq!(s.output_size(1080, 1920, NO_LIMIT), (1440, 2560));
    }

    #[test]
    fn output_fits_the_encoder_limits() {
        let h264 = (4096, 4096);
        let s = Settings { resolution: 2160, ..Default::default() };
        // 5160x2160 is what 2160p asks of a 21:9 monitor.
        assert_eq!(s.output_size(3440, 1440, h264), (4096, 1714));
        assert_eq!(s.output_size(1440, 3440, h264), (1714, 4096));
        assert_eq!(s.output_size(3440, 1440, (8192, 8192)), (5160, 2160));
        // Native size of a super-wide monitor, and a limit tighter in height than width.
        assert_eq!(Settings::default().output_size(7680, 2160, h264), (4096, 1152));
        assert_eq!(Settings::default().output_size(3840, 2160, (4096, 2048)), (3640, 2048));
        // Within the limits nothing changes, and the result is always even.
        assert_eq!(s.output_size(1920, 1080, h264), (3840, 2160));
        let (w, h) = Settings::default().output_size(5001, 3001, (4095, 4095));
        assert!(w <= 4095 && h <= 4095 && w % 2 == 0 && h % 2 == 0, "{w}x{h}");
    }

    #[test]
    fn ram_estimate_matches_the_bitrate() {
        let s = Settings { bitrate_mbps: 50, replay_seconds: 120, ..Default::default() };
        let gb = s.replay_ram_bytes() as f64 / 1e9;
        assert!((1.1..1.2).contains(&gb), "{gb}");
    }

    #[test]
    fn only_encoder_settings_restart_the_encoder() {
        let a = Settings::default();
        assert!(!a.video_differs(&Settings { replay_seconds: 30, sounds: false, ..a.clone() }));
        assert!(a.video_differs(&Settings { bitrate_mbps: 10, ..a.clone() }));
        assert!(a.audio_differs(&Settings { microphone: "x".into(), ..a.clone() }));
        assert!(a.audio_differs(&Settings { exclude_tape_audio: false, ..a.clone() }));
        assert!(!a.audio_differs(&Settings { mic_volume: 50, mic_delay_ms: 80, ..a.clone() }));
    }
}
