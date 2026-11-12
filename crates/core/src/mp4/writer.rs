use std::io::{self, Seek, SeekFrom, Write};
use std::sync::Arc;

use super::boxes::{bx, ftyp, full, moov, moov_with, Kind, Put, Table};
use super::chapters;
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
    /// The fragmented header's moov after its type, rewritten unchanged when finishing.
    init_moov_body: Vec<u8>,
    mdat_header_at: u64,
    sequence: u32,
    fragment_video_ticks: i64,
    video_ticks_per_fragment: i64,
    bytes: u64,
    /// A write failed part way, so the sample tables no longer match the file.
    failed: bool,
    /// Engine times in microseconds of the markers set during this file, which become chapters.
    markers: Vec<i64>,
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
            init_moov_body: head[init_moov_at as usize + 8..mdat_header_at as usize].to_vec(),
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
            failed: false,
            markers: Vec::new(),
        })
    }

    /// Markers at these engine times (microseconds, on the same timeline as the packets), written
    /// as chapters "Marker 1", "Marker 2"... when the file is finished. Times from before the
    /// file's first frame are left out, so a file split off from a recording keeps only its own.
    pub fn set_markers(&mut self, mut at_us: Vec<i64>) {
        at_us.sort_unstable();
        self.markers = at_us;
    }

    fn chapters(&self) -> Vec<(f64, String)> {
        let Some(origin) = self.origin_us else { return Vec::new() };
        let seconds: Vec<f64> = self.markers.iter().filter(|&&us| us >= origin).map(|&us| (us - origin) as f64 / 1e6).collect();
        chapters::markers(&seconds)
    }

    /// Bytes of media written so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// After an error every later call fails too: what is on disk is then only the fragments
    /// written before it, which still play, and anything more would point at the wrong bytes.
    pub fn push(&mut self, packet: &Packet, timescale: u32) -> io::Result<()> {
        self.check()?;
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

    fn check(&self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("an earlier write to this file failed"));
        }
        Ok(())
    }

    /// The tables are updated as the samples go out, so a failure part way marks the writer
    /// failed rather than leaving tables that point past what reached the disk.
    fn flush_fragment(&mut self) -> io::Result<()> {
        let result = self.write_fragment();
        self.failed |= result.is_err();
        result
    }

    fn write_fragment(&mut self) -> io::Result<()> {
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
    /// one large `mdat` and the normal `moov` at the end points straight at the samples. Returns
    /// the file's final size, which includes the last fragment written here.
    pub fn finish(self) -> io::Result<(W, u64)> {
        self.finish_with(|_| Ok(()))
    }

    /// `finish`, calling `sync` to get everything written so far onto the disk before the header
    /// changes that switch to the final index, and again after them.
    pub fn finish_with(mut self, mut sync: impl FnMut(&mut W) -> io::Result<()>) -> io::Result<(W, u64)> {
        self.check()?;
        for t in &mut self.tracks {
            if let Some((_, key, data)) = t.held.take() {
                t.fragment.push(Sample { duration: t.last_duration, key, data });
            }
        }
        self.flush_fragment()?;

        let tables: Vec<Table> = self.tracks.iter().map(|t| t.table.clone()).collect();
        let mut index = Vec::new();
        moov_with(&mut index, &tables, false, &chapters::udta(&self.chapters()));
        self.out.write_all(&index)?;
        self.out.flush()?;
        let size = self.pos + index.len() as u64;
        // Until here the file is the fragmented one with an unused box at the end. The index has to
        // be on the disk before the headers point at it, or a power cut could keep only the edit.
        sync(&mut self.out)?;

        // Both edits in one write, since they sit next to each other: the fragmented moov becomes
        // padding and the padding after it becomes the header of one mdat over all fragments.
        // Either one alone leaves a file with no playable frames.
        let mdat_size = self.pos - self.mdat_header_at;
        let mut edit = Vec::with_capacity(self.init_moov_body.len() + 20);
        edit.extend_from_slice(b"free");
        edit.extend_from_slice(&self.init_moov_body);
        edit.u32(1);
        edit.extend_from_slice(b"mdat");
        edit.u64(mdat_size);
        self.out.seek(SeekFrom::Start(self.init_moov_at + 4))?;
        self.out.write_all(&edit)?;
        self.out.flush()?;
        sync(&mut self.out)?;
        Ok((self.out, size))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{AudioFormat, Codec, VideoFormat};
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    /// A file whose writes fail once at `room` bytes, as when the disk fills and space is freed
    /// again, or a USB drive drops out for a moment. The bytes stay readable after the writer is
    /// gone.
    struct Disk {
        file: Rc<RefCell<Cursor<Vec<u8>>>>,
        room: u64,
    }

    impl Write for Disk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut file = self.file.borrow_mut();
            let left = self.room.saturating_sub(file.position()) as usize;
            if left == 0 {
                self.room = u64::MAX;
                return Err(io::Error::other("disk full"));
            }
            file.write(&buf[..buf.len().min(left)])
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for Disk {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.file.borrow_mut().seek(to)
        }
    }

    pub(super) fn tracks() -> Tracks {
        Tracks {
            video: VideoFormat { codec: Codec::H264, width: 16, height: 16, fps: 10, config: vec![1, 0x64, 0, 0x1f, 0xff, 0xe0, 0] },
            audio: vec![AudioFormat { name: "a".into(), asc: vec![0x11, 0x90] }],
        }
    }

    pub(super) fn frame(n: i64) -> Packet {
        Packet { track: 0, pts: n, key: n % 20 == 0, data: Arc::new(vec![n as u8; 500]) }
    }

    fn top_level(data: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + 8 <= data.len() {
            let mut size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as u64;
            if size == 1 {
                size = u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap());
            }
            out.push(String::from_utf8_lossy(&data[i + 4..i + 8]).into_owned());
            if size < 8 {
                break;
            }
            i += size as usize;
        }
        out
    }

    #[test]
    fn a_failed_write_leaves_the_fragments_alone() {
        let file = Rc::new(RefCell::new(Cursor::new(Vec::new())));
        let t = tracks();
        let mut w = Mp4Writer::new(Disk { file: file.clone(), room: 30_000 }, &t).unwrap();
        // 2 s fragments of 20 frames, 10 kB each: the third fragment hits the error. Writes work
        // again afterwards, which is what would let a finish write a wrong index.
        let mut failed_at = None;
        for n in 0..100 {
            if w.push(&frame(n), 10).is_err() {
                failed_at = Some(n);
                break;
            }
        }
        let failed_at = failed_at.expect("the disk never filled");
        assert!(w.push(&frame(failed_at + 1), 10).is_err(), "later writes must fail too");
        assert!(w.finish().is_err(), "finishing must be refused");
        let data = file.borrow().get_ref().clone();
        let boxes = top_level(&data);
        // Still the fragmented layout: header moov, padding, two whole fragments.
        assert_eq!(&boxes[..6], ["ftyp", "moov", "free", "moof", "mdat", "moof"], "{boxes:?}");
        assert_eq!(boxes.iter().filter(|b| *b == "moov").count(), 1, "no index may be appended: {boxes:?}");
    }
}
