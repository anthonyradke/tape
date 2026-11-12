//! The host talks to the engine process with one JSON object per line over stdin and stdout.

use serde::{Deserialize, Serialize};

use crate::packet::Codec;
use crate::settings::Settings;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum Command {
    Configure { settings: Box<Settings> },
    SaveClip,
    StartRecording,
    StopRecording,
    SetReplay { on: bool },
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
    fn new_fields_are_sent() {
        let line = serde_json::to_string(&Event::Error { message: "x".into(), clip: false, stopped: true }).unwrap();
        assert!(line.contains(r#""stopped":true"#), "{line}");
        let video = VideoStatus { transient: true, ..Default::default() };
        assert!(serde_json::to_string(&video).unwrap().contains(r#""transient":true"#));
    }
}
