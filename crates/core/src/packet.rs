//! Encoded data as it moves from the encoders to the replay ring and the recording file.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub const AUDIO_RATE: u32 = 48_000;
pub const AAC_FRAME: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

/// What one encoder session produces. Two sessions with equal formats can share a file, which
/// is what lets a recording survive an encoder restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// avcC, hvcC or av1C payload.
    pub config: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub name: String,
    /// AudioSpecificConfig.
    pub asc: Vec<u8>,
}

/// Track 0 is always video, audio tracks follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracks {
    pub video: VideoFormat,
    pub audio: Vec<AudioFormat>,
}

impl Tracks {
    pub fn timescale(&self, track: u8) -> u32 {
        if track == 0 { self.video.fps } else { AUDIO_RATE }
    }
}

/// One access unit in MP4 sample format.
#[derive(Clone, Debug)]
pub struct Packet {
    pub track: u8,
    /// Presentation time in the track's timescale (frames for video, samples for audio), on the
    /// engine's timeline. B-frames are never used, so decode order equals presentation order.
    pub pts: i64,
    pub key: bool,
    pub data: Arc<Vec<u8>>,
}

impl Packet {
    pub fn micros(&self, timescale: u32) -> i64 {
        to_micros(self.pts, timescale)
    }
}

pub fn to_micros(ticks: i64, timescale: u32) -> i64 {
    (ticks as i128 * 1_000_000 / timescale as i128) as i64
}

pub fn from_micros(us: i64, timescale: u32) -> i64 {
    (us as i128 * timescale as i128 / 1_000_000) as i64
}
