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
}

impl Default for Hotkeys {
    /// The same keys ShadowPlay uses, so muscle memory carries over.
    fn default() -> Self {
        Hotkeys {
            save_clip: "Alt+F10".into(),
            toggle_recording: "Alt+F9".into(),
            toggle_replay: "Alt+Shift+F10".into(),
            overlay: "Alt+Z".into(),
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
    /// Output device to record: "default" follows Windows, "" records none, otherwise an id.
    pub system_audio: String,
    /// Input device, same convention.
    pub microphone: String,
    pub system_volume: u32,
    pub mic_volume: u32,
    /// Keep game audio and mic as separate tracks instead of mixing them.
    pub separate_tracks: bool,
    pub hotkeys: Hotkeys,
    pub sounds: bool,
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
            system_audio: FOLLOW_DEFAULT.into(),
            microphone: NO_DEVICE.into(),
            system_volume: 100,
            mic_volume: 100,
            separate_tracks: false,
            hotkeys: Hotkeys::default(),
            sounds: true,
        }
    }
}

pub const AUDIO_KBPS: u32 = 192;
/// Peak bitrate as a multiple of the target; the encoder runs VBR.
const PEAK: f64 = 1.5;

impl Settings {
    /// Forces every value into a range the engine can run, so a hand-edited or old settings file
    /// can never stop it starting.
    pub fn normalized(mut self) -> Self {
        self.fps = self.fps.clamp(15, 360);
        self.bitrate_mbps = self.bitrate_mbps.clamp(1, 500);
        self.replay_seconds = self.replay_seconds.clamp(5, 3600);
        self.system_volume = self.system_volume.min(200);
        self.mic_volume = self.mic_volume.min(200);
        if self.resolution != 0 {
            self.resolution = self.resolution.clamp(144, 4320);
        }
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
    pub fn output_size(&self, width: u32, height: u32) -> (u32, u32) {
        let short = width.min(height).max(1) as u64;
        let scale = |edge: u32| (edge as u64 * self.resolution as u64 / short) as u32;
        let (w, h) = if self.resolution == 0 { (width, height) } else { (scale(width), scale(height)) };
        (w & !1, h & !1)
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
            || self.microphone != other.microphone
            || self.separate_tracks != other.separate_tracks
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

    #[test]
    fn nonsense_values_are_clamped() {
        let s = Settings { fps: 0, bitrate_mbps: 99_999, replay_seconds: 0, resolution: 3, ..Default::default() };
        let s = s.normalized();
        assert_eq!((s.fps, s.bitrate_mbps, s.replay_seconds, s.resolution), (15, 500, 5, 144));
    }

    #[test]
    fn output_keeps_aspect_and_stays_even() {
        let s = Settings { resolution: 1080, ..Default::default() };
        assert_eq!(s.output_size(2560, 1440), (1920, 1080));
        assert_eq!(s.output_size(3440, 1440), (2580, 1080));
        let native = Settings::default();
        assert_eq!(native.output_size(1367, 769), (1366, 768));
    }

    #[test]
    fn output_can_be_larger_than_the_monitor() {
        let at = |resolution| Settings { resolution, ..Default::default() }.output_size(1920, 1080);
        assert_eq!(at(1440), (2560, 1440));
        assert_eq!(at(2160), (3840, 2160));
    }

    #[test]
    fn resolution_goes_by_the_short_edge() {
        let s = Settings { resolution: 1440, ..Default::default() };
        assert_eq!(s.output_size(1080, 1920), (1440, 2560));
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
        assert!(!a.audio_differs(&Settings { mic_volume: 50, ..a.clone() }));
    }
}
