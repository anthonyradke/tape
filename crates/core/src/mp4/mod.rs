//! MP4 output, written by hand so no part of tape needs ffmpeg.
//!
//! Recordings use the hybrid layout: the file is a valid fragmented MP4 the whole time it is
//! being written, so a crash or power cut costs at most the last two seconds. Finishing appends
//! a normal `moov` and rewrites two box headers in place, leaving an ordinary MP4 with no copy.
//! Clips are written in one go with the `moov` first so they start instantly when shared.

mod boxes;
pub mod chapters;
pub mod faststart;
pub mod repair;
pub mod trim;
pub mod codec;
mod writer;

pub use writer::{write_clip, Mp4Writer};
