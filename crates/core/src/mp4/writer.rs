use std::io::{self, Seek, SeekFrom, Write};
use std::sync::Arc;

use super::boxes::{bx, ftyp, full, moov, Kind, Put, Table};
use crate::packet::{from_micros, Packet, Tracks};

const SYNC_FLAGS: u32 = 0x0200_0000;
const NON_SYNC_FLAGS: u32 = 0x0101_0000;
/// A fragment closes at the first keyframe after this long, which is how much a crash can lose.
const FRAGMENT_SECONDS: i64 = 2;

fn tables(tracks: &Tracks) -> Vec<Table> {
    let v = &tracks.video;
    let video = Kind::Video { codec: v.codec, width: v.width, height: v.height, config: v.config.clone() };
    let mut out = vec![Table::new(video, "Video".into(), v.fps)];
    for a in &tracks.audio {
        out.push(Table::new(Kind::Audio { asc: a.asc.clone() }, a.name.clone(), crate::packet::AUDIO_RATE));
    }
    out
}

struct Sample {
    duration: u32,
    key: bool,
    data: Arc<Vec<u8>>,
}

struct TrackState {
    table: Table,
    /// Pts on the engine timeline of movie time zero, in this track's timescale.
    origin: i64,
    fragment: Vec<Sample>,
    /// Newest sample, held back because its duration is the gap to the next one.
    held: Option<(i64, bool, Arc<Vec<u8>>)>,
    last_duration: u32,
    /// Decode time of the first sample in `fragment`.
    fragment_start: i64,
    started: bool,
}

/// Streaming writer for recordings. Packets for one track must arrive in order, and the first
/// packet must be a video keyframe.
pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    tracks: Vec<TrackState>,
    origin_us: Option<i64>,
    pos: u64,
    init_moov_at: u64,
    mdat_header_at: u64,
    sequence: u32,
    fragment_video_ticks: i64,
    video_ticks_per_fragment: i64,
    bytes: u64,
}

impl<W: Write + Seek> Mp4Writer<W> {
    pub fn new(mut out: W, tracks: &Tracks) -> io::Result<Self> {
        let tables = tables(tracks);
        let mut head = Vec::new();
        ftyp(&mut head, tracks.video.codec);
        let init_moov_at = head.len() as u64;
        moov(&mut head, &tables, true);
        let mdat_header_at = head.len() as u64;
        bx(&mut head, b"free", |o| o.zeros(8));
        out.write_all(&head)?;
        out.flush()?;
        let default_gap = |t: &Table| if matches!(t.kind, Kind::Video { .. }) { 1 } else { crate::packet::AAC_FRAME as u32 };
        Ok(Mp4Writer {
            pos: head.len() as u64,
            video_ticks_per_fragment: FRAGMENT_SECONDS * tracks.video.fps as i64,
            tracks: tables
                .into_iter()
                .map(|table| TrackState {
                    origin: 0,
                    fragment: Vec::new(),
                    held: None,
                    last_duration: default_gap(&table),
                    fragment_start: 0,
                    started: false,
                    table,
                })
                .collect(),
            out,
            origin_us: None,
            init_moov_at,
            mdat_header_at,
            sequence: 0,
            fragment_video_ticks: 0,
            bytes: 0,
        })
    }

    /// Bytes of media written so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn push(&mut self, packet: &Packet, timescale: u32) -> io::Result<()> {
        let us = packet.micros(timescale);
        let origin_us = match self.origin_us {
            Some(o) => o,
            None if packet.track == 0 && packet.key => {
                self.origin_us = Some(us);
                self.tracks[0].origin = packet.pts;
                for t in &mut self.tracks[1..] {
                    t.origin = from_micros(us, t.table.timescale);
                }
                us
            }
            None => return Ok(()),
        };
        if us < origin_us {
            return Ok(());
        }
        let t = &mut self.tracks[packet.track as usize];
        if let Some((pts, key, data)) = t.held.take() {
            let duration = (packet.pts - pts).clamp(1, u32::MAX as i64) as u32;
            t.last_duration = duration;
            t.fragment.push(Sample { duration, key, data });
            if packet.track == 0 {
                self.fragment_video_ticks += duration as i64;
            }
        }
        // Tested after the frame before this one is counted, so a fragment is one GOP when the
        // keyframes are FRAGMENT_SECONDS apart, not two.
        let cut = packet.track == 0 && packet.key && self.fragment_video_ticks >= self.video_ticks_per_fragment;
        let t = &mut self.tracks[packet.track as usize];
        if !t.started {
            t.started = true;
            t.table.start = (packet.pts - t.origin).max(0);
            t.fragment_start = t.table.start;
        }
        t.held = Some((packet.pts, packet.key, packet.data.clone()));
        if cut {
            self.flush_fragment()?;
        }
        Ok(())
    }

    fn flush_fragment(&mut self) -> io::Result<()> {
        if self.tracks.iter().all(|t| t.fragment.is_empty()) {
            return Ok(());
        }
        self.sequence += 1;
        // The moof's size doesn't depend on the offsets inside it, so build it twice.
        let moof_len = self.moof(0).len() as u32;
        let moof = self.moof(moof_len + 8);
        let payload: u64 = self.tracks.iter().flat_map(|t| &t.fragment).map(|s| s.data.len() as u64).sum();

        let moof_at = self.pos;
        self.out.write_all(&moof)?;
        self.out.write_all(&(payload as u32 + 8).to_be_bytes())?;
        self.out.write_all(b"mdat")?;
        let mut at = moof_at + moof.len() as u64 + 8;
        for t in &mut self.tracks {
            if t.fragment.is_empty() {
                continue;
            }
            t.table.chunks.push((at, t.fragment.len() as u32));
            for s in t.fragment.drain(..) {
                self.out.write_all(&s.data)?;
                at += s.data.len() as u64;
                t.table.add_sample(s.duration, s.data.len() as u32, s.key);
                t.fragment_start += s.duration as i64;
            }
        }
        self.out.flush()?;
        self.pos = at;
        self.bytes += payload;
        self.fragment_video_ticks = 0;
        Ok(())
    }

    fn moof(&self, data_offset: u32) -> Vec<u8> {
        let mut out = Vec::new();
        bx(&mut out, b"moof", |o| {
            full(o, b"mfhd", 0, 0, |o| o.u32(self.sequence));
            let mut offset = data_offset;
            for (i, t) in self.tracks.iter().enumerate() {
                if t.fragment.is_empty() {
                    continue;
                }
                bx(o, b"traf", |o| {
                    full(o, b"tfhd", 0, 0x02_0000, |o| o.u32(i as u32 + 1));
                    full(o, b"tfdt", 1, 0, |o| o.u64(t.fragment_start as u64));
                    full(o, b"trun", 0, 0x000701, |o| {
                        o.u32(t.fragment.len() as u32);
                        o.u32(offset);
                        for s in &t.fragment {
                            o.u32(s.duration);
                            o.u32(s.data.len() as u32);
                            o.u32(if s.key { SYNC_FLAGS } else { NON_SYNC_FLAGS });
                        }
                    });
                });
                offset += t.fragment.iter().map(|s| s.data.len() as u32).sum::<u32>();
            }
        });
        out
    }

    /// Writes the final index. Leaves an ordinary MP4: the fragments become opaque bytes inside
    /// one large `mdat` and the normal `moov` at the end points straight at the samples.
    pub fn finish(mut self) -> io::Result<W> {
        for t in &mut self.tracks {
            if let Some((_, key, data)) = t.held.take() {
                t.fragment.push(Sample { duration: t.last_duration, key, data });
            }
        }
        self.flush_fragment()?;

        let tables: Vec<Table> = self.tracks.iter().map(|t| t.table.clone()).collect();
        let mut index = Vec::new();
        moov(&mut index, &tables, false);
        self.out.write_all(&index)?;

        let mdat_size = self.pos - self.mdat_header_at;
        self.out.seek(SeekFrom::Start(self.mdat_header_at))?;
        let mut header = Vec::with_capacity(16);
        header.u32(1);
        header.extend_from_slice(b"mdat");
        header.u64(mdat_size);
        self.out.write_all(&header)?;
        // Turn the fragmented header into padding last, so the file is playable at every step.
        self.out.seek(SeekFrom::Start(self.init_moov_at + 4))?;
        self.out.write_all(b"free")?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Writes a finished clip with `moov` first. `packets` must start on a video keyframe.
pub fn write_clip<W: Write>(mut out: W, tracks: &Tracks, packets: &[Packet]) -> io::Result<u64> {
    let mut tables = tables(tracks);
    let mut by_track: Vec<Vec<&Packet>> = vec![Vec::new(); tables.len()];
    for p in packets {
        by_track[p.track as usize].push(p);
    }
    let Some(first) = by_track[0].first() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "clip has no video"));
    };
    let origin_us = first.micros(tracks.video.fps);
    let end_of_chunk: Vec<i64> = by_track[0]
        .iter()
        .filter(|p| p.key)
        .skip(1)
        .map(|p| p.micros(tracks.video.fps))
        .chain([i64::MAX])
        .collect();

    // Drop audio from before the first frame and fix each track's start.
    for (i, list) in by_track.iter_mut().enumerate().skip(1) {
        let ts = tables[i].timescale;
        list.retain(|p| p.micros(ts) >= origin_us);
        if let Some(p) = list.first() {
            tables[i].start = (p.pts - from_micros(origin_us, ts)).max(0);
        }
    }

    // Chunks in file order: every track's samples up to each keyframe.
    let mut order: Vec<(usize, usize, usize)> = Vec::new();
    let mut next = vec![0usize; by_track.len()];
    for &end in &end_of_chunk {
        for (i, list) in by_track.iter().enumerate() {
            let ts = tables[i].timescale;
            let from = next[i];
            let mut to = from;
            while to < list.len() && list[to].micros(ts) < end {
                to += 1;
            }
            if to > from {
                order.push((i, from, to));
                next[i] = to;
            }
        }
    }

    for (i, list) in by_track.iter().enumerate() {
        let table = &mut tables[i];
        let fallback = if i == 0 { 1 } else { crate::packet::AAC_FRAME as u32 };
        let mut last = fallback;
        for (n, p) in list.iter().enumerate() {
            let duration = match list.get(n + 1) {
                Some(next) => (next.pts - p.pts).clamp(1, u32::MAX as i64) as u32,
                None => last,
            };
            last = duration;
            table.add_sample(duration, p.data.len() as u32, p.key);
        }
    }

    let payload: u64 = order
        .iter()
        .flat_map(|&(i, from, to)| &by_track[i][from..to])
        .map(|p| p.data.len() as u64)
        .sum();

    let mut head = Vec::new();
    ftyp(&mut head, tracks.video.codec);
    let ftyp_len = head.len() as u64;

    // moov's size depends on the width of its offsets, not their values: fix the width up front
    // (the margin covers the headers), measure with placeholder offsets, then place for real.
    let wide = payload + (1 << 26) > u32::MAX as u64;
    for t in &mut tables {
        t.wide = wide;
    }
    let place = |tables: &mut Vec<Table>, base: u64| {
        let mut at = base;
        for t in tables.iter_mut() {
            t.chunks.clear();
        }
        for &(i, from, to) in &order {
            tables[i].chunks.push((at, (to - from) as u32));
            at += by_track[i][from..to].iter().map(|p| p.data.len() as u64).sum::<u64>();
        }
    };
    place(&mut tables, 0);
    let mut sized = Vec::new();
    moov(&mut sized, &tables, false);
    let data_at = ftyp_len + sized.len() as u64 + 16;
    place(&mut tables, data_at);
    moov(&mut head, &tables, false);
    debug_assert_eq!(head.len() as u64 + 16, data_at);

    head.u32(1);
    head.extend_from_slice(b"mdat");
    head.u64(payload + 16);
    out.write_all(&head)?;
    for &(i, from, to) in &order {
        for p in &by_track[i][from..to] {
            out.write_all(&p.data)?;
        }
    }
    out.flush()?;
    Ok(data_at + payload)
}
