//! The host talks to the engine process with one JSON object per line over stdin and stdout.

use serde::{Deserialize, Serialize};

use crate::packet::Codec;
use crate::settings::Settings;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum Command {
    Configure { settings: Box<Settings> },
    /// `seconds` asks for at most that much, for the short clip key; None is the replay length.
    SaveClip {
        #[serde(default)]
        seconds: Option<u32>,
    },
    StartRecording,
    StopRecording,
    SetReplay { on: bool },
    /// A marker at this point of the recording, which the file gets as a chapter.
    AddMarker,
    Probe,
    Shutdown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VideoStatus {
    pub running: bool,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub method: String,
    /// Time from capture to finished bitstream for the last frame.
    pub encode_ms: f32,
    pub dropped: u64,
    pub bitrate_kbps: u32,
    /// Last problem, cleared once frames flow again.
    pub problem: Option<String>,
    /// The problem is Windows holding capture back for a while (the lock screen, a UAC prompt,
    /// the monitor turned off), which ends by itself and isn't worth an alarm.
    pub transient: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioSource {
    pub name: String,
    pub running: bool,
    pub peak: f32,
    pub problem: Option<String>,
    /// Game audio with tape's own sounds left out.
    pub excludes_tape: bool,
}

/// Things that went wrong without stopping anything, which show in clips as stutter or crackle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QualityCounts {
    /// Frames capture couldn't deliver by their time, which the video skips.
    pub dropped_frames: u64,
    /// Frames that took longer than a frame's time to capture, convert and encode.
    pub late_frames: u64,
    /// Overlaps cut, lost packets, and for the mic gaps filled with silence: game audio, then mic.
    pub system_glitches: u64,
    pub mic_glitches: u64,
    /// Times the recording's writer fell far behind, or the disk couldn't keep up at all.
    pub slow_disk: u64,
}

impl QualityCounts {
    /// What happened between `earlier` and these.
    pub fn since(&self, earlier: &QualityCounts) -> QualityCounts {
        QualityCounts {
            dropped_frames: self.dropped_frames.saturating_sub(earlier.dropped_frames),
            late_frames: self.late_frames.saturating_sub(earlier.late_frames),
            system_glitches: self.system_glitches.saturating_sub(earlier.system_glitches),
            mic_glitches: self.mic_glitches.saturating_sub(earlier.mic_glitches),
            slow_disk: self.slow_disk.saturating_sub(earlier.slow_disk),
        }
    }

    pub fn is_clean(&self) -> bool {
        *self == QualityCounts::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Quality {
    /// Since replay or recording started.
    pub total: QualityCounts,
    pub seconds: f32,
    /// The last `recent_seconds`, at most a few minutes.
    pub recent: QualityCounts,
    pub recent_seconds: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Status {
    pub video: VideoStatus,
    pub system: Option<AudioSource>,
    pub mic: Option<AudioSource>,
    pub replay_on: bool,
    pub replay_seconds: f32,
    /// How long a clip saved now would be: shorter than the buffer right after another clip.
    pub clip_seconds: f32,
    pub replay_target: u32,
    pub replay_bytes: u64,
    pub recording: bool,
    pub recording_seconds: f32,
    pub recording_bytes: u64,
    pub recording_path: Option<String>,
    /// Times the video pipeline has been restarted since the engine started.
    pub restarts: u32,
    /// Where a clip saved now would go.
    pub game: String,
    pub quality: Quality,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub hz: u32,
    pub primary: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

/// The largest frame the GPU's encoder takes for one codec. The output size is scaled down to fit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodecLimit {
    pub codec: Codec,
    pub max_width: u32,
    pub max_height: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Probe {
    pub gpu: String,
    pub driver: String,
    pub codecs: Vec<Codec>,
    /// One entry per codec in `codecs`, when the driver answered.
    pub limits: Vec<CodecLimit>,
    pub nvenc_error: Option<String>,
    pub monitors: Vec<Monitor>,
    pub outputs: Vec<AudioDevice>,
    pub inputs: Vec<AudioDevice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SavedKind {
    Clip,
    Recording,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "evt", rename_all = "camelCase")]
pub enum Event {
    Status { status: Box<Status> },
    Probe { probe: Box<Probe> },
    Saved { kind: SavedKind, path: String, bytes: u64 },
    /// The answer to `AddMarker`: how far into the recording's file it went, or None when no
    /// recording is being written.
    Marked { seconds: Option<f64> },
    /// `clip` is set when this is the answer to a save request, so the host can play the failure cue.
    /// `stopped` is set when the recording has ended. An error in a file that was split off while
    /// the recording goes on in the next one leaves it unset.
    Error {
        message: String,
        clip: bool,
        #[serde(default)]
        stopped: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_from_an_older_engine_still_parse() {
        let event: Event = serde_json::from_str(r#"{"evt":"error","message":"x","clip":false}"#).unwrap();
        assert!(matches!(event, Event::Error { stopped: false, .. }));
        let status: Status = serde_json::from_str(r#"{"video":{"running":true,"problem":"x"}}"#).unwrap();
        assert!(!status.video.transient);
    }

    #[test]
    fn a_save_without_a_length_is_a_full_one() {
        let command: Command = serde_json::from_str(r#"{"cmd":"saveClip"}"#).unwrap();
        assert!(matches!(command, Command::SaveClip { seconds: None }));
        let command: Command = serde_json::from_str(r#"{"cmd":"saveClip","seconds":30}"#).unwrap();
        assert!(matches!(command, Command::SaveClip { seconds: Some(30) }));
    }

    #[test]
    fn new_fields_are_sent() {
        let line = serde_json::to_string(&Event::Error { message: "x".into(), clip: false, stopped: true }).unwrap();
        assert!(line.contains(r#""stopped":true"#), "{line}");
        let video = VideoStatus { transient: true, ..Default::default() };
        assert!(serde_json::to_string(&video).unwrap().contains(r#""transient":true"#));
    }
}
