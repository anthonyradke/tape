//! ISO BMFF box serialization.

use crate::packet::{Codec, AUDIO_RATE};

pub const MOVIE_TIMESCALE: u32 = 1000;

pub fn bx(out: &mut Vec<u8>, kind: &[u8; 4], f: impl FnOnce(&mut Vec<u8>)) {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(kind);
    f(out);
    let size = (out.len() - at) as u32;
    out[at..at + 4].copy_from_slice(&size.to_be_bytes());
}

pub fn full(out: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32, f: impl FnOnce(&mut Vec<u8>)) {
    bx(out, kind, |o| {
        o.extend_from_slice(&((version as u32) << 24 | flags).to_be_bytes());
        f(o);
    });
}

pub trait Put {
    fn u16(&mut self, v: u16);
    fn u32(&mut self, v: u32);
    fn u64(&mut self, v: u64);
    fn zeros(&mut self, n: usize);
}

impl Put for Vec<u8> {
    fn u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn zeros(&mut self, n: usize) {
        self.resize(self.len() + n, 0);
    }
}

#[derive(Clone)]
pub enum Kind {
    Video { codec: Codec, width: u32, height: u32, config: Vec<u8> },
    Audio { asc: Vec<u8> },
}

/// Everything `moov` needs to describe one track.
#[derive(Clone)]
pub struct Table {
    pub kind: Kind,
    pub name: String,
    pub timescale: u32,
    /// Gap between the start of the movie and the track's first sample, in `timescale` ticks.
    pub start: i64,
    /// Run-length (count, delta).
    pub stts: Vec<(u32, u32)>,
    pub sizes: Vec<u32>,
    /// (file offset, samples in chunk).
    pub chunks: Vec<(u64, u32)>,
    /// One-based sample numbers of keyframes, video only.
    pub sync: Vec<u32>,
    /// Use 64-bit chunk offsets even if every offset would fit in 32.
    pub wide: bool,
}

impl Table {
    pub fn new(kind: Kind, name: String, timescale: u32) -> Self {
        Table { kind, name, timescale, start: 0, stts: vec![], sizes: vec![], chunks: vec![], sync: vec![], wide: false }
    }

    pub fn add_sample(&mut self, delta: u32, size: u32, key: bool) {
        match self.stts.last_mut() {
            Some((count, d)) if *d == delta => *count += 1,
            _ => self.stts.push((1, delta)),
        }
        self.sizes.push(size);
        if key {
            self.sync.push(self.sizes.len() as u32);
        }
    }

    pub fn ticks(&self) -> u64 {
        self.stts.iter().map(|&(c, d)| c as u64 * d as u64).sum()
    }

    fn is_video(&self) -> bool {
        matches!(self.kind, Kind::Video { .. })
    }
}

fn to_movie(ticks: u64, timescale: u32) -> u64 {
    ticks * MOVIE_TIMESCALE as u64 / timescale as u64
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

pub fn ftyp(out: &mut Vec<u8>, codec: Codec) {
    bx(out, b"ftyp", |o| {
        o.extend_from_slice(b"isom");
        o.u32(0x200);
        o.extend_from_slice(b"isomiso2iso5iso6mp41");
        o.extend_from_slice(match codec {
            Codec::H264 => b"avc1",
            Codec::Hevc => b"hvc1",
            Codec::Av1 => b"av01",
        });
    });
}

/// `fragmented` adds `mvex`, marking the track tables as empty until the fragments that follow.
pub fn moov(out: &mut Vec<u8>, tracks: &[Table], fragmented: bool) {
    bx(out, b"moov", |o| {
        let duration = tracks
            .iter()
            .map(|t| to_movie(t.start as u64 + t.ticks(), t.timescale))
            .max()
            .unwrap_or(0);
        full(o, b"mvhd", 0, 0, |o| {
            o.u32(0);
            o.u32(0);
            o.u32(MOVIE_TIMESCALE);
            o.u32(duration.min(u32::MAX as u64) as u32);
            o.u32(0x0001_0000); // rate
            o.u16(0x0100); // volume
            o.zeros(10);
            for v in MATRIX {
                o.u32(v);
            }
            o.zeros(24);
            o.u32(tracks.len() as u32 + 1);
        });
        for (i, t) in tracks.iter().enumerate() {
            trak(o, t, i as u32 + 1);
        }
        if fragmented {
            bx(o, b"mvex", |o| {
                for i in 0..tracks.len() {
                    full(o, b"trex", 0, 0, |o| {
                        o.u32(i as u32 + 1);
                        o.u32(1);
                        o.zeros(12);
                    });
                }
            });
        }
    });
}

fn trak(o: &mut Vec<u8>, t: &Table, id: u32) {
    bx(o, b"trak", |o| {
        let ticks = t.ticks();
        let total = to_movie(t.start as u64 + ticks, t.timescale);
        full(o, b"tkhd", 0, 3, |o| {
            o.u32(0);
            o.u32(0);
            o.u32(id);
            o.u32(0);
            o.u32(total.min(u32::MAX as u64) as u32);
            o.zeros(8);
            o.u16(0); // layer
            o.u16(if t.is_video() { 0 } else { 1 }); // audio tracks are alternatives
            o.u16(if t.is_video() { 0 } else { 0x0100 });
            o.u16(0);
            for v in MATRIX {
                o.u32(v);
            }
            match t.kind {
                Kind::Video { width, height, .. } => {
                    o.u32(width << 16);
                    o.u32(height << 16);
                }
                Kind::Audio { .. } => o.zeros(8),
            }
        });
        if to_movie(t.start as u64, t.timescale) > 0 && ticks > 0 {
            bx(o, b"edts", |o| {
                full(o, b"elst", 0, 0, |o| {
                    o.u32(2);
                    // Empty edit: the track is silent until its first sample.
                    o.u32(to_movie(t.start as u64, t.timescale) as u32);
                    o.u32(u32::MAX);
                    o.u32(0x0001_0000);
                    o.u32(to_movie(ticks, t.timescale).min(u32::MAX as u64) as u32);
                    o.u32(0);
                    o.u32(0x0001_0000);
                });
            });
        }
        bx(o, b"mdia", |o| {
            full(o, b"mdhd", 0, 0, |o| {
                o.u32(0);
                o.u32(0);
                o.u32(t.timescale);
                o.u32(ticks.min(u32::MAX as u64) as u32);
                o.u16(0x55c4); // "und"
                o.u16(0);
            });
            full(o, b"hdlr", 0, 0, |o| {
                o.u32(0);
                o.extend_from_slice(if t.is_video() { b"vide" } else { b"soun" });
                o.zeros(12);
                o.extend_from_slice(t.name.as_bytes());
                o.push(0);
            });
            bx(o, b"minf", |o| {
                if t.is_video() {
                    full(o, b"vmhd", 0, 1, |o| o.zeros(8));
                } else {
                    full(o, b"smhd", 0, 0, |o| o.zeros(4));
                }
                bx(o, b"dinf", |o| {
                    full(o, b"dref", 0, 0, |o| {
                        o.u32(1);
                        full(o, b"url ", 0, 1, |_| {});
                    });
                });
                stbl(o, t);
            });
        });
    });
}

fn stbl(o: &mut Vec<u8>, t: &Table) {
    bx(o, b"stbl", |o| {
        full(o, b"stsd", 0, 0, |o| {
            o.u32(1);
            match &t.kind {
                Kind::Video { codec, width, height, config } => video_entry(o, *codec, *width, *height, config),
                Kind::Audio { asc } => audio_entry(o, asc),
            }
        });
        full(o, b"stts", 0, 0, |o| {
            o.u32(t.stts.len() as u32);
            for &(count, delta) in &t.stts {
                o.u32(count);
                o.u32(delta);
            }
        });
        if t.is_video() && !t.sync.is_empty() {
            full(o, b"stss", 0, 0, |o| {
                o.u32(t.sync.len() as u32);
                for &s in &t.sync {
                    o.u32(s);
                }
            });
        }
        full(o, b"stsc", 0, 0, |o| {
            let mut runs: Vec<(u32, u32)> = Vec::new();
            for (i, &(_, samples)) in t.chunks.iter().enumerate() {
                if runs.last().map(|r| r.1) != Some(samples) {
                    runs.push((i as u32 + 1, samples));
                }
            }
            o.u32(runs.len() as u32);
            for (first, samples) in runs {
                o.u32(first);
                o.u32(samples);
                o.u32(1);
            }
        });
        full(o, b"stsz", 0, 0, |o| {
            o.u32(0);
            o.u32(t.sizes.len() as u32);
            for &s in &t.sizes {
                o.u32(s);
            }
        });
        if t.wide || t.chunks.iter().any(|c| c.0 > u32::MAX as u64) {
            full(o, b"co64", 0, 0, |o| {
                o.u32(t.chunks.len() as u32);
                for c in &t.chunks {
                    o.u64(c.0);
                }
            });
        } else {
            full(o, b"stco", 0, 0, |o| {
                o.u32(t.chunks.len() as u32);
                for c in &t.chunks {
                    o.u32(c.0 as u32);
                }
            });
        }
    });
}

fn video_entry(o: &mut Vec<u8>, codec: Codec, width: u32, height: u32, config: &[u8]) {
    let (entry, cfg): (&[u8; 4], &[u8; 4]) = match codec {
        Codec::H264 => (b"avc1", b"avcC"),
        Codec::Hevc => (b"hvc1", b"hvcC"),
        Codec::Av1 => (b"av01", b"av1C"),
    };
    bx(o, entry, |o| {
        o.zeros(6);
        o.u16(1); // data reference
        o.zeros(16);
        o.u16(width as u16);
        o.u16(height as u16);
        o.u32(0x0048_0000);
        o.u32(0x0048_0000);
        o.u32(0);
        o.u16(1); // frames per sample
        o.zeros(32); // compressor name
        o.u16(0x18);
        o.u16(0xffff);
        bx(o, cfg, |o| o.extend_from_slice(config));
        // BT.709 limited range, which is what the converter writes.
        bx(o, b"colr", |o| {
            o.extend_from_slice(b"nclx");
            o.u16(1);
            o.u16(1);
            o.u16(1);
            o.push(0);
        });
    });
}

fn audio_entry(o: &mut Vec<u8>, asc: &[u8]) {
    bx(o, b"mp4a", |o| {
        o.zeros(6);
        o.u16(1);
        o.zeros(8);
        o.u16(2); // channels
        o.u16(16); // sample size
        o.zeros(4);
        o.u32(AUDIO_RATE << 16);
        full(o, b"esds", 0, 0, |o| {
            let dsi = asc.len() as u8;
            o.extend_from_slice(&[3, 23 + dsi, 0, 1, 0]);
            o.extend_from_slice(&[4, 15 + dsi, 0x40, 0x15, 0, 0, 0]);
            o.u32(0);
            o.u32(0);
            o.extend_from_slice(&[5, dsi]);
            o.extend_from_slice(asc);
            o.extend_from_slice(&[6, 1, 2]);
        });
    });
}
