//! Finishes a recording that was cut off by a crash or power cut. What's left on disk is the
//! fragmented layout `Mp4Writer` keeps the file in while it records: ftyp, a header moov with
//! `mvex`, 16 bytes of padding, then moof/mdat pairs, the last of which may be cut short. The
//! sample tables are rebuilt from the fragments' `trun` boxes and the file is finished in place the
//! way `Mp4Writer::finish` does it: the new index goes on the end and reaches the disk before the
//! two box headers that switch to it are rewritten in one write, so a power cut during the repair
//! leaves either the old playable file or the finished one. The samples themselves never move.
//!
//! Everything is read and checked before the first byte is written; a file that doesn't look like
//! one of tape's unfinished recordings is left as it was.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::boxes::Put;
use super::faststart::MAX_MOOV;
use super::trim::{be32, be64, children, find, full_body, invalid, new_moov, Cut, Movie, Sample, Track};

/// Size of the padding `Mp4Writer` leaves after the header moov, which becomes the mdat header.
const PADDING: u64 = 16;
/// A fragment's moof is a few kB; anything far bigger isn't one of tape's.
const MAX_MOOF: u64 = 16 << 20;
/// Only so many boxes are looked at to tell whether a file is unfinished.
const LOOK_AHEAD: usize = 6;

/// tfhd and trun flags, ISO/IEC 14496-12 8.8.7 and 8.8.8.
const TF_BASE_DATA_OFFSET: u32 = 0x1;
const TF_DESCRIPTION_INDEX: u32 = 0x2;
const TF_DEFAULT_DURATION: u32 = 0x8;
const TF_DEFAULT_SIZE: u32 = 0x10;
const TF_DEFAULT_FLAGS: u32 = 0x20;
const TF_BASE_IS_MOOF: u32 = 0x2_0000;
const TR_DATA_OFFSET: u32 = 0x1;
const TR_FIRST_FLAGS: u32 = 0x4;
const TR_DURATION: u32 = 0x100;
const TR_SIZE: u32 = 0x200;
const TR_FLAGS: u32 = 0x400;
const TR_CTS: u32 = 0x800;
const NON_SYNC: u32 = 0x1_0000;

/// A top-level box: type, where it starts, its whole length and its header's.
#[derive(Clone, Copy)]
struct Top {
    kind: [u8; 4],
    at: u64,
    size: u64,
    header: u64,
}

impl Top {
    fn end(&self) -> u64 {
        self.at + self.size
    }
}

/// Top-level boxes up to the first that is damaged or runs past the end of the file, which in a
/// crashed recording is the fragment that was being written. At most `limit` of them.
fn scan(file: &mut File, len: u64, limit: usize) -> io::Result<Vec<Top>> {
    let mut boxes = Vec::new();
    let mut at = 0;
    while at + 8 <= len && boxes.len() < limit {
        file.seek(SeekFrom::Start(at))?;
        let mut head = [0u8; 16];
        file.read_exact(&mut head[..8])?;
        let mut size = u32::from_be_bytes(head[..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = head[4..8].try_into().unwrap();
        let mut header = 8;
        if size == 1 {
            if at + 16 > len {
                break;
            }
            file.read_exact(&mut head[8..16])?;
            size = u64::from_be_bytes(head[8..16].try_into().unwrap());
            header = 16;
        } else if size == 0 {
            size = len - at;
        }
        if size < header || at.checked_add(size).is_none_or(|end| end > len) {
            break;
        }
        boxes.push(Top { kind, at, size, header });
        at += size;
    }
    Ok(boxes)
}

fn read_at(file: &mut File, at: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut data = vec![0; len as usize];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut data)?;
    Ok(data)
}

/// Whether the moov starting at `top` has an `mvex`, read header by header so a big finished
/// index isn't loaded just to look.
fn has_mvex(file: &mut File, top: &Top) -> io::Result<bool> {
    let mut at = top.at + top.header;
    while at + 8 <= top.end() {
        let head = read_at(file, at, 8)?;
        let size = u32::from_be_bytes(head[..4].try_into().unwrap()) as u64;
        if &head[4..8] == b"mvex" {
            return Ok(true);
        }
        if size < 8 {
            return Ok(false);
        }
        at += size;
    }
    Ok(false)
}

/// True for a file still in the fragmented layout: a recording that was never finished, which
/// trim refuses and `repair` can finish. Cheap enough to ask of every file in the gallery: it
/// reads a few box headers.
pub fn unfinished(path: &Path) -> bool {
    let check = || -> io::Result<bool> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let boxes = scan(&mut file, len, LOOK_AHEAD)?;
        if boxes.iter().any(|b| &b.kind == b"moof") {
            return Ok(true);
        }
        // A crash in the first seconds leaves the header and no fragment yet.
        let moov = boxes.iter().position(|b| &b.kind == b"moov");
        let mdat = boxes.iter().position(|b| &b.kind == b"mdat");
        match moov {
            Some(m) if mdat.is_none_or(|d| m < d) => has_mvex(&mut file, &boxes[m]),
            _ => Ok(false),
        }
    };
    check().unwrap_or(false)
}

/// What `mvex/trex` says each track's samples are unless a fragment says otherwise.
#[derive(Clone, Copy, Default)]
struct Defaults {
    duration: u32,
    size: u32,
    flags: u32,
}

/// A track as the header moov describes it, filled in from the fragments.
struct Building {
    id: u32,
    track: Track,
    chunks: Vec<(u64, u32)>,
    defaults: Defaults,
    /// Decode time of the first sample, from the first fragment's `tfdt`.
    start: Option<i64>,
    /// Decode time the next sample would have.
    next_dts: i64,
}

fn read_header(moov: &[u8]) -> io::Result<(u32, Vec<Building>)> {
    let kids = children(&moov[8..])?;
    let mvex = children(find(&kids, b"mvex").ok_or_else(|| invalid("this recording was already finished"))?.body)?;
    let (version, mvhd) = full_body(find(&kids, b"mvhd").ok_or_else(|| invalid("no mvhd"))?)?;
    let timescale = be32(mvhd, if version == 1 { 16 } else { 8 })?;
    if timescale == 0 {
        return Err(invalid("movie has no timescale"));
    }
    let mut tracks = Vec::new();
    for trak in kids.iter().filter(|c| &c.kind == b"trak") {
        let inner = children(trak.body)?;
        let (version, tkhd) = full_body(find(&inner, b"tkhd").ok_or_else(|| invalid("no tkhd"))?)?;
        let id = be32(tkhd, if version == 1 { 16 } else { 8 })?;
        let mdia = children(find(&inner, b"mdia").ok_or_else(|| invalid("track without media"))?.body)?;
        let (version, mdhd) = full_body(find(&mdia, b"mdhd").ok_or_else(|| invalid("no mdhd"))?)?;
        let track_timescale = be32(mdhd, if version == 1 { 16 } else { 8 })?;
        let (_, hdlr) = full_body(find(&mdia, b"hdlr").ok_or_else(|| invalid("no hdlr"))?)?;
        if track_timescale == 0 {
            return Err(invalid("track has no timescale"));
        }
        let video = hdlr.get(4..8) == Some(b"vide");
        let mut defaults = Defaults::default();
        for trex in mvex.iter().filter(|c| &c.kind == b"trex") {
            let (_, b) = full_body(trex)?;
            if be32(b, 0)? == id {
                defaults = Defaults { duration: be32(b, 8)?, size: be32(b, 12)?, flags: be32(b, 16)? };
            }
        }
        let track = Track {
            id,
            video,
            timescale: track_timescale,
            samples: Vec::new(),
            has_ctts: false,
            ctts_version: 0,
            has_stss: video,
            empty: 0,
            media_time: 0,
        };
        tracks.push(Building { id, track, chunks: Vec::new(), defaults, start: None, next_dts: 0 });
    }
    Ok((timescale, tracks))
}

/// One track's samples from one fragment, before they're known to be whole.
struct Run {
    track: usize,
    tfdt: Option<i64>,
    /// (first sample's offset, samples) for each trun.
    chunks: Vec<(u64, u32)>,
    samples: Vec<Sample>,
    ctts_version: Option<u8>,
}

/// Reads one moof. `data` is the payload range of the mdat after it, which every sample must lie in.
fn read_fragment(moof: &[u8], moof_at: u64, data: (u64, u64), tracks: &[Building]) -> io::Result<Vec<Run>> {
    let kids = children(&moof[8..])?;
    let mut runs = Vec::new();
    // Where a traf's data starts when it says nothing: after the previous traf's (8.8.7.1).
    let mut previous_end = moof_at;
    for traf in kids.iter().filter(|c| &c.kind == b"traf") {
        let inner = children(traf.body)?;
        let tfhd = find(&inner, b"tfhd").ok_or_else(|| invalid("fragment without tfhd"))?;
        let flags = be32(tfhd.body, 0)? & 0xff_ffff;
        let (_, b) = full_body(tfhd)?;
        let id = be32(b, 0)?;
        let track = tracks.iter().position(|t| t.id == id).ok_or_else(|| invalid("fragment for a track that isn't there"))?;
        let mut defaults = tracks[track].defaults;
        let mut at = 4;
        let mut base = if flags & TF_BASE_IS_MOOF != 0 { moof_at } else { previous_end };
        if flags & TF_BASE_DATA_OFFSET != 0 {
            base = be64(b, at)?;
            at += 8;
        }
        if flags & TF_DESCRIPTION_INDEX != 0 {
            at += 4;
        }
        if flags & TF_DEFAULT_DURATION != 0 {
            defaults.duration = be32(b, at)?;
            at += 4;
        }
        if flags & TF_DEFAULT_SIZE != 0 {
            defaults.size = be32(b, at)?;
            at += 4;
        }
        if flags & TF_DEFAULT_FLAGS != 0 {
            defaults.flags = be32(b, at)?;
        }
        let tfdt = match find(&inner, b"tfdt") {
            Some(c) => {
                let (version, b) = full_body(c)?;
                Some(if version == 1 { be64(b, 0)? as i64 } else { be32(b, 0)? as i64 })
            }
            None => None,
        };
        let mut run = Run { track, tfdt, chunks: Vec::new(), samples: Vec::new(), ctts_version: None };
        let mut next = base;
        for trun in inner.iter().filter(|c| &c.kind == b"trun") {
            let version = *trun.body.first().ok_or_else(|| invalid("short box"))?;
            let flags = be32(trun.body, 0)? & 0xff_ffff;
            let (_, b) = full_body(trun)?;
            let count = be32(b, 0)? as usize;
            let mut at = 4;
            if flags & TR_DATA_OFFSET != 0 {
                next = base.checked_add_signed(be32(b, at)? as i32 as i64).ok_or_else(|| invalid("bad data offset"))?;
                at += 4;
            }
            let first_flags = if flags & TR_FIRST_FLAGS != 0 {
                at += 4;
                Some(be32(b, at - 4)?)
            } else {
                None
            };
            let fields = [TR_DURATION, TR_SIZE, TR_FLAGS, TR_CTS].iter().filter(|&&f| flags & f != 0).count();
            // A count the box can't hold, or more samples than the mdat has bytes, is damage.
            if count as u64 > data.1 - data.0 + 1 || (fields > 0 && b.len() < at + count * fields * 4) {
                return Err(invalid("damaged fragment"));
            }
            if flags & TR_CTS != 0 {
                run.ctts_version = Some(run.ctts_version.unwrap_or(0).max(version));
            }
            let chunk_at = next;
            for i in 0..count {
                let mut field = || {
                    at += 4;
                    be32(b, at - 4)
                };
                let duration = if flags & TR_DURATION != 0 { field()? } else { defaults.duration };
                let size = if flags & TR_SIZE != 0 { field()? } else { defaults.size };
                let sample_flags = if flags & TR_FLAGS != 0 { field()? } else if i == 0 { first_flags.unwrap_or(defaults.flags) } else { defaults.flags };
                let cts = if flags & TR_CTS != 0 { field()? as i32 } else { 0 };
                let end = next.checked_add(size as u64).ok_or_else(|| invalid("damaged fragment"))?;
                if next < data.0 || end > data.1 {
                    return Err(invalid("fragment points outside its data"));
                }
                let key = !tracks[track].track.video || sample_flags & NON_SYNC == 0;
                run.samples.push(Sample { offset: next, size, dts: 0, duration, cts, key });
                next = end;
            }
            if count > 0 {
                run.chunks.push((chunk_at, count as u32));
            }
        }
        previous_end = next;
        runs.push(run);
    }
    Ok(runs)
}

/// What a repair did.
#[derive(Clone, Debug, PartialEq)]
pub struct Repaired {
    /// Length of the finished file's video, in seconds.
    pub seconds: f64,
    /// Bytes of a fragment that was cut short and had to go.
    pub dropped: u64,
}

/// Finishes the unfinished recording at `path` in place. See the module comment.
pub fn repair(path: &Path) -> io::Result<Repaired> {
    repair_with(path, |f| f.sync_all())
}

/// `repair`, with `sync` standing in for the disk flushes so a test can look at each step.
fn repair_with(path: &Path, mut sync: impl FnMut(&mut File) -> io::Result<()>) -> io::Result<Repaired> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    let boxes = scan(&mut file, len, usize::MAX)?;
    let layout = boxes.len() >= 3
        && &boxes[0].kind == b"ftyp"
        && &boxes[1].kind == b"moov"
        && boxes[1].header == 8
        && &boxes[2].kind == b"free"
        && boxes[2].size == PADDING;
    if !layout {
        if !unfinished(path) {
            return Err(invalid("this file is already finished"));
        }
        return Err(invalid("this file wasn't left by tape, so it can't be repaired"));
    }
    let moov_box = boxes[1];
    if moov_box.size > MAX_MOOV {
        return Err(invalid("index too large"));
    }
    let moov = read_at(&mut file, moov_box.at, moov_box.size)?;
    let (timescale, mut tracks) = read_header(&moov)?;

    // Each moof with the mdat after it. The first one that isn't whole, and anything after it (the
    // index of a finish cut short, say), is dropped.
    let mut end = boxes[2].end();
    let mut i = 3;
    while i + 1 < boxes.len() && &boxes[i].kind == b"moof" && &boxes[i + 1].kind == b"mdat" {
        let (moof, mdat) = (boxes[i], boxes[i + 1]);
        if moof.size > MAX_MOOF {
            break;
        }
        let data = read_at(&mut file, moof.at, moof.size)?;
        let Ok(runs) = read_fragment(&data, moof.at, (mdat.at + mdat.header, mdat.end()), &tracks) else { break };
        for run in runs {
            let t = &mut tracks[run.track];
            if let Some(tfdt) = run.tfdt {
                if t.start.is_none() {
                    t.start = Some(tfdt);
                    t.next_dts = tfdt;
                } else if tfdt > t.next_dts {
                    // A gap between fragments: the sample before it lasts until the next one.
                    if let Some(last) = t.track.samples.last_mut() {
                        last.duration = last.duration.saturating_add((tfdt - t.next_dts).min(u32::MAX as i64) as u32);
                    }
                    t.next_dts = tfdt;
                }
            }
            if let Some(version) = run.ctts_version {
                t.track.has_ctts = true;
                t.track.ctts_version = t.track.ctts_version.max(version);
            }
            for mut s in run.samples {
                s.dts = t.next_dts;
                t.next_dts += s.duration as i64;
                t.track.samples.push(s);
            }
            t.chunks.extend(run.chunks);
        }
        end = mdat.end();
        i += 2;
    }
    let video = tracks.iter().find(|t| t.track.video).ok_or_else(|| invalid("no video in this file"))?;
    if video.track.samples.is_empty() {
        return Err(invalid("it stopped before the first two seconds were written, so there's nothing to keep"));
    }
    let seconds = video.track.samples.iter().map(|s| s.duration as f64).sum::<f64>() / video.track.timescale as f64;

    let movie = Movie { chapter_tracks: Vec::new(), ftyp: None, moov, timescale, tracks: Vec::new() };
    let cuts: Vec<Cut> = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.track.samples.is_empty())
        .map(|(index, t)| Cut {
            track: &t.track,
            index,
            samples: &t.track.samples,
            // A track that started after the video, as audio does, waits that long in silence.
            empty: (t.start.unwrap_or(0).max(0) as u128 * timescale as u128 / t.track.timescale as u128) as u64,
            media_time: t.track.samples[0].cts as i64,
            limit: u64::MAX,
            chunks: t.chunks.clone(),
        })
        .collect();
    let wide = cuts.iter().flat_map(|c| &c.chunks).any(|c| c.0 > u32::MAX as u64);
    let index = new_moov(&movie, &cuts, wide, None)?;

    // From here on the file changes, each step leaving one that plays: first the fragment that was
    // cut short goes, then the index is added after the fragments, which players ignore until the
    // headers point at it.
    if end < len {
        file.set_len(end)?;
    }
    file.seek(SeekFrom::Start(end))?;
    file.write_all(&index)?;
    file.flush()?;
    sync(&mut file)?;

    // As in `Mp4Writer::finish_with`: the header moov becomes padding and the padding after it the
    // header of one mdat over every fragment, in a single write.
    let mut edit = Vec::with_capacity(moov_box.size as usize + 12);
    edit.extend_from_slice(b"free");
    edit.extend_from_slice(&movie.moov[8..]);
    edit.u32(1);
    edit.extend_from_slice(b"mdat");
    edit.u64(end - boxes[2].at);
    file.seek(SeekFrom::Start(moov_box.at + 4))?;
    file.write_all(&edit)?;
    file.flush()?;
    sync(&mut file)?;
    Ok(Repaired { seconds, dropped: len - end })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::Mp4Writer;
    use crate::packet::{AudioFormat, Codec, Packet, Tracks, VideoFormat};
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;
    use std::sync::Arc;

    /// A file the test can still read after the writer is gone, as a crash leaves one.
    struct Disk(Rc<RefCell<Cursor<Vec<u8>>>>);

    impl Write for Disk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for Disk {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.0.borrow_mut().seek(to)
        }
    }

    fn dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tape-repair-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What a crash after `frames` frames (and their audio) leaves on disk: 10 fps, a keyframe
    /// every 2 s, one audio packet per frame.
    fn crashed(name: &str, frames: i64) -> std::path::PathBuf {
        let t = Tracks {
            video: VideoFormat { codec: Codec::H264, width: 16, height: 16, fps: 10, config: vec![1, 0x64, 0, 0x1f, 0xff, 0xe0, 0] },
            audio: vec![AudioFormat { name: "a".into(), asc: vec![0x11, 0x90] }],
        };
        let disk = Rc::new(RefCell::new(Cursor::new(Vec::new())));
        let mut w = Mp4Writer::new(Disk(disk.clone()), &t).unwrap();
        for n in 0..frames {
            w.push(&Packet { track: 0, pts: n, key: n % 20 == 0, data: Arc::new(vec![n as u8; 500]) }, 10).unwrap();
            w.push(&Packet { track: 1, pts: n * 4800, key: true, data: Arc::new(vec![7; 40]) }, 48_000).unwrap();
        }
        drop(w);
        let path = dir().join(name);
        std::fs::write(&path, disk.borrow().get_ref()).unwrap();
        path
    }

    fn top(path: &Path) -> Vec<String> {
        let mut file = File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        scan(&mut file, len, usize::MAX).unwrap().iter().map(|b| String::from_utf8_lossy(&b.kind).into_owned()).collect()
    }

    #[test]
    fn a_crashed_recording_is_finished_in_place() {
        // 2 s fragments of 20 frames: 0-2 s, 2-4 s and 4-6 s are on disk, the rest was still held.
        let path = crashed("crash.mp4", 70);
        assert!(unfinished(&path));
        let repaired = repair(&path).unwrap();
        assert_eq!(repaired.dropped, 0);
        assert!((repaired.seconds - 6.0).abs() < 1e-9, "{}", repaired.seconds);
        assert!(!unfinished(&path));
        assert_eq!(top(&path), ["ftyp", "free", "mdat", "moov"]);
        let info = crate::mp4::trim::info(&path).unwrap();
        assert_eq!(info.keyframes, [0.0, 2.0, 4.0]);
        assert!((info.duration - 6.0).abs() < 1e-9);
        // Done once is done.
        assert!(repair(&path).is_err());
    }

    #[test]
    fn a_fragment_cut_short_is_dropped() {
        let path = crashed("cut.mp4", 70);
        let len = std::fs::metadata(&path).unwrap().len();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(len - 300).unwrap();
        drop(file);
        let repaired = repair(&path).unwrap();
        assert!((repaired.seconds - 4.0).abs() < 1e-9, "{}", repaired.seconds);
        assert!(repaired.dropped > 0);
        assert_eq!(top(&path), ["ftyp", "free", "mdat", "moov"]);
    }

    #[test]
    fn nothing_changes_when_it_cant_be_repaired() {
        // Stopped before the first fragment: only the header is there.
        let path = crashed("empty.mp4", 5);
        assert!(unfinished(&path));
        let before = std::fs::read(&path).unwrap();
        assert!(repair(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn every_step_of_a_repair_leaves_a_file_that_reads() {
        let path = crashed("steps.mp4", 70);
        let len = std::fs::metadata(&path).unwrap().len();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(len - 300).unwrap();
        drop(file);
        let mut states = Vec::new();
        repair_with(&path, |f| {
            f.seek(SeekFrom::Start(0))?;
            let mut all = Vec::new();
            f.read_to_end(&mut all)?;
            states.push(all);
            Ok(())
        })
        .unwrap();
        assert_eq!(states.len(), 2);
        // After the first sync the file is still unfinished (and repairable again), after the second it's done.
        let first = dir().join("steps-0.mp4");
        std::fs::write(&first, &states[0]).unwrap();
        assert!(unfinished(&first));
        assert!((repair(&first).unwrap().seconds - 4.0).abs() < 1e-9);
        assert_eq!(std::fs::read(&first).unwrap(), std::fs::read(&path).unwrap());
    }
}
