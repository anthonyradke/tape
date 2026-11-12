//! Cuts a finished MP4 down to a stretch of it without re-encoding. The sample tables are read
//! from the `moov`, the samples in range are copied into a new `moov`-first file, and every other
//! box (codec setup, metadata) is carried over byte for byte, so the picture and sound are exactly
//! the original's.
//!
//! Video can only start on a keyframe, so the start moves back to the keyframe at or before the
//! one asked for: `info` lists them so the player can show where a cut can land. Other tracks
//! start with the sample playing at that moment and get an edit list that lines them up again.
//! Works for any non-fragmented MP4, not only tape's: clips copied over from the NVIDIA app have
//! B-frames (`ctts`) and edit lists of their own, which are taken into account.
//!
//! Chapter marks (Nero `chpl`, what ffmpeg writes and reads for MP4) come along: `info` lists them
//! for the player to show and snap to, and the trimmed file keeps the ones inside the cut.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::boxes::{bx, full, Put};
use super::faststart::{top_level, MAX_MOOV};

/// Slack when comparing times, well under a frame at any rate tape records.
const EPSILON: f64 = 0.001;
/// Samples go into the new file interleaved in chunks of about this long.
const CHUNK_SECONDS: f64 = 0.5;
/// A trim shorter than this is almost certainly a slip.
pub const MIN_SECONDS: f64 = 0.5;

pub(super) fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

pub(super) fn be32(b: &[u8], at: usize) -> io::Result<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap())).ok_or_else(|| invalid("short box"))
}

pub(super) fn be64(b: &[u8], at: usize) -> io::Result<u64> {
    b.get(at..at + 8).map(|s| u64::from_be_bytes(s.try_into().unwrap())).ok_or_else(|| invalid("short box"))
}

/// One box inside a run of boxes: its type, the whole box, and its body.
pub(super) struct Child<'a> {
    pub kind: [u8; 4],
    pub whole: &'a [u8],
    pub body: &'a [u8],
}

pub(super) fn children(data: &[u8]) -> io::Result<Vec<Child<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= data.len() {
        let mut size = be32(data, at)? as usize;
        let kind: [u8; 4] = data[at + 4..at + 8].try_into().unwrap();
        let mut header = 8;
        if size == 1 {
            size = be64(data, at + 8)? as usize;
            header = 16;
        } else if size == 0 {
            size = data.len() - at;
        }
        if size < header || at.checked_add(size).is_none_or(|end| end > data.len()) {
            return Err(invalid("damaged box in moov"));
        }
        out.push(Child { kind, whole: &data[at..at + size], body: &data[at + header..at + size] });
        at += size;
    }
    Ok(out)
}

pub(super) fn find<'a>(list: &'a [Child<'a>], kind: &[u8; 4]) -> Option<&'a Child<'a>> {
    list.iter().find(|c| &c.kind == kind)
}

/// Version byte of a full box, and its body after version and flags.
pub(super) fn full_body<'a>(c: &Child<'a>) -> io::Result<(u8, &'a [u8])> {
    if c.body.len() < 4 {
        return Err(invalid("short box"));
    }
    Ok((c.body[0], &c.body[4..]))
}

#[derive(Clone, Copy)]
pub(super) struct Sample {
    pub offset: u64,
    pub size: u32,
    pub dts: i64,
    pub duration: u32,
    pub cts: i32,
    pub key: bool,
}

pub(super) struct Track {
    pub id: u32,
    pub video: bool,
    pub timescale: u32,
    pub samples: Vec<Sample>,
    pub has_ctts: bool,
    pub ctts_version: u8,
    pub has_stss: bool,
    /// Movie-timescale silence before the media starts, from empty edits.
    pub empty: u64,
    /// Media time shown at the start of the (first non-empty) edit.
    pub media_time: i64,
}

impl Track {
    fn seconds(&self, s: &Sample, movie_timescale: u32) -> f64 {
        self.empty as f64 / movie_timescale as f64 + (s.dts + s.cts as i64 - self.media_time) as f64 / self.timescale as f64
    }
}

pub(super) struct Movie {
    /// Ids of QuickTime text tracks that hold chapter titles (`tref/chap`), which ffmpeg adds
    /// beside `chpl`. Trimming drops them: they can't be cut like media, and `chpl` says the same.
    pub chapter_tracks: Vec<u32>,
    pub ftyp: Option<Vec<u8>>,
    pub moov: Vec<u8>,
    pub timescale: u32,
    pub tracks: Vec<Track>,
}

/// `file_len` bounds what the tables may claim: counts in them come from the file, and a few bytes
/// can otherwise ask for billions of samples.
fn read_sample_table(stbl: &[Child], file_len: u64) -> io::Result<(Vec<Sample>, bool, u8, bool)> {
    let need = |k: &[u8; 4]| find(stbl, k).ok_or_else(|| invalid("incomplete sample table"));

    let (_, stsz) = full_body(find(stbl, b"stsz").ok_or_else(|| invalid("unsupported sample size table"))?)?;
    let fixed = be32(stsz, 0)?;
    let count = be32(stsz, 4)? as usize;
    // A list of sizes is bounded by the box holding it; one size for all only by the file.
    if fixed != 0 && count as u64 > file_len / fixed as u64 {
        return Err(invalid("sample table larger than the file"));
    }
    let sizes: Vec<u32> = if fixed != 0 { vec![fixed; count] } else { (0..count).map(|i| be32(stsz, 8 + i * 4)).collect::<io::Result<_>>()? };

    let (_, stts) = full_body(need(b"stts")?)?;
    let mut timing = Vec::with_capacity(sizes.len());
    let mut dts = 0i64;
    for i in 0..be32(stts, 0)? as usize {
        let (count, delta) = (be32(stts, 4 + i * 8)?, be32(stts, 8 + i * 8)?);
        if timing.len() + count as usize > sizes.len() {
            return Err(invalid("sample tables disagree"));
        }
        for _ in 0..count {
            timing.push((dts, delta));
            dts += delta as i64;
        }
    }
    if sizes.len() != timing.len() {
        return Err(invalid("sample tables disagree"));
    }

    let offsets: Vec<u64> = if let Some(c) = find(stbl, b"co64") {
        let (_, b) = full_body(c)?;
        (0..be32(b, 0)? as usize).map(|i| be64(b, 4 + i * 8)).collect::<io::Result<_>>()?
    } else {
        let (_, b) = full_body(need(b"stco")?)?;
        (0..be32(b, 0)? as usize).map(|i| be32(b, 4 + i * 4).map(u64::from)).collect::<io::Result<_>>()?
    };

    // Chunk runs: (first chunk, samples per chunk), one-based chunks.
    let (_, stsc) = full_body(need(b"stsc")?)?;
    let runs: Vec<(usize, usize)> =
        (0..be32(stsc, 0)? as usize).map(|i| Ok((be32(stsc, 4 + i * 12)? as usize, be32(stsc, 8 + i * 12)? as usize))).collect::<io::Result<_>>()?;
    let mut placed = Vec::with_capacity(sizes.len());
    'chunks: for (chunk, &base) in offsets.iter().enumerate() {
        let per = runs.iter().rev().find(|r| r.0 <= chunk + 1).map(|r| r.1).ok_or_else(|| invalid("bad chunk table"))?;
        let mut at = base;
        for _ in 0..per {
            let Some(&size) = sizes.get(placed.len()) else { break 'chunks };
            placed.push(at);
            at += size as u64;
        }
    }
    if placed.len() != sizes.len() {
        return Err(invalid("chunk table doesn't cover every sample"));
    }

    let (has_ctts, ctts_version, mut cts) = match find(stbl, b"ctts") {
        Some(c) => {
            let (version, b) = full_body(c)?;
            let mut list = Vec::with_capacity(sizes.len());
            for i in 0..be32(b, 0)? as usize {
                let (count, offset) = (be32(b, 4 + i * 8)?, be32(b, 8 + i * 8)? as i32);
                // Anything past the last sample is ignored below, so it isn't stored either.
                let count = (count as usize).min(sizes.len() - list.len());
                list.extend(std::iter::repeat_n(offset, count));
            }
            (true, version, list)
        }
        None => (false, 0, Vec::new()),
    };
    cts.resize(sizes.len(), 0);

    let (has_stss, keys) = match find(stbl, b"stss") {
        Some(c) => {
            let (_, b) = full_body(c)?;
            let mut keys = vec![false; sizes.len()];
            for i in 0..be32(b, 0)? as usize {
                if let Some(k) = keys.get_mut((be32(b, 4 + i * 4)? as usize).wrapping_sub(1)) {
                    *k = true;
                }
            }
            (true, keys)
        }
        None => (false, vec![true; sizes.len()]),
    };

    let samples = (0..sizes.len())
        .map(|i| Sample { offset: placed[i], size: sizes[i], dts: timing[i].0, duration: timing[i].1, cts: cts[i], key: keys[i] })
        .collect();
    Ok((samples, has_ctts, ctts_version, has_stss))
}

/// (silence before the media in movie ticks, media time where it starts)
fn read_edits(trak: &[Child]) -> io::Result<(u64, i64)> {
    let Some(edts) = find(trak, b"edts") else { return Ok((0, 0)) };
    let inner = children(edts.body)?;
    let Some(elst) = find(&inner, b"elst") else { return Ok((0, 0)) };
    let (version, b) = full_body(elst)?;
    let mut empty = 0;
    for i in 0..be32(b, 0)? as usize {
        let (duration, media_time) = if version == 1 {
            (be64(b, 4 + i * 20)?, be64(b, 12 + i * 20)? as i64)
        } else {
            (be32(b, 4 + i * 12)? as u64, be32(b, 8 + i * 12)? as i32 as i64)
        };
        if media_time == -1 {
            empty += duration;
        } else {
            return Ok((empty, media_time));
        }
    }
    Ok((empty, 0))
}

fn read_track(trak: &Child, file_len: u64) -> io::Result<Track> {
    let kids = children(trak.body)?;
    let mdia = children(find(&kids, b"mdia").ok_or_else(|| invalid("track without media"))?.body)?;
    let (version, mdhd) = full_body(find(&mdia, b"mdhd").ok_or_else(|| invalid("no mdhd"))?)?;
    let timescale = be32(mdhd, if version == 1 { 16 } else { 8 })?;
    // Only needed to recognise a chapter track, so a track without one still reads.
    let id = match find(&kids, b"tkhd") {
        Some(c) => full_body(c).and_then(|(version, b)| be32(b, if version == 1 { 16 } else { 8 })).unwrap_or(0),
        None => 0,
    };
    let (_, hdlr) = full_body(find(&mdia, b"hdlr").ok_or_else(|| invalid("no hdlr"))?)?;
    let video = hdlr.get(4..8) == Some(b"vide");
    let minf = children(find(&mdia, b"minf").ok_or_else(|| invalid("no minf"))?.body)?;
    let stbl = children(find(&minf, b"stbl").ok_or_else(|| invalid("no stbl"))?.body)?;
    find(&stbl, b"stsd").ok_or_else(|| invalid("no stsd"))?;
    let (samples, has_ctts, ctts_version, has_stss) = read_sample_table(&stbl, file_len)?;
    let (empty, media_time) = read_edits(&kids)?;
    if timescale == 0 {
        return Err(invalid("track has no timescale"));
    }
    Ok(Track { id, video, timescale, samples, has_ctts, ctts_version, has_stss, empty, media_time })
}

fn read_movie(file: &mut File) -> io::Result<Movie> {
    let boxes = top_level(file)?;
    let mut read = |start: u64, len: u64| -> io::Result<Vec<u8>> {
        if len > MAX_MOOV {
            return Err(invalid("index too large"));
        }
        let mut data = vec![0; len as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut data)?;
        Ok(data)
    };
    let &(_, start, len) = boxes.iter().find(|b| &b.0 == b"moov").ok_or_else(|| invalid("no index (moov)"))?;
    let moov = read(start, len)?;
    let ftyp = match boxes.iter().find(|b| &b.0 == b"ftyp") {
        Some(&(_, start, len)) => Some(read(start, len)?),
        None => None,
    };
    let kids = children(&moov[8..])?;
    if find(&kids, b"mvex").is_some() || boxes.iter().any(|b| &b.0 == b"moof") {
        return Err(invalid("this file was never finished, so it can't be trimmed"));
    }
    let (version, mvhd) = full_body(find(&kids, b"mvhd").ok_or_else(|| invalid("no mvhd"))?)?;
    let timescale = be32(mvhd, if version == 1 { 16 } else { 8 })?;
    let file_len = file.metadata()?.len();
    let tracks = kids.iter().filter(|c| &c.kind == b"trak").map(|t| read_track(t, file_len)).collect::<io::Result<Vec<_>>>()?;
    if timescale == 0 {
        return Err(invalid("movie has no timescale"));
    }
    let mut chapter_tracks = Vec::new();
    for trak in kids.iter().filter(|c| &c.kind == b"trak") {
        let inner = children(trak.body)?;
        let Some(tref) = find(&inner, b"tref") else { continue };
        for chap in children(tref.body)?.iter().filter(|c| &c.kind == b"chap") {
            chapter_tracks.extend(chap.body.as_chunks::<4>().0.iter().map(|id| u32::from_be_bytes(*id)));
        }
    }
    Ok(Movie { chapter_tracks, ftyp, timescale, tracks, moov })
}

fn main_video(movie: &Movie) -> io::Result<&Track> {
    movie.tracks.iter().find(|t| t.video && !t.samples.is_empty()).ok_or_else(|| invalid("no video in this file"))
}

/// What the player needs to place a cut.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Info {
    /// Seconds from the start of playback to the end of the last frame.
    pub duration: f64,
    /// Where each keyframe shows, in seconds. A trim can only start on one of these.
    pub keyframes: Vec<f64>,
    /// Chapter marks inside the video, in seconds.
    pub markers: Vec<f64>,
}

/// Nero chapters, `moov/udta/chpl`: (start in seconds, title). Version 1, as ffmpeg writes it, has
/// four more bytes before the count. Starts are in 100 ns units.
fn read_chapters(moov: &[u8]) -> Vec<(f64, String)> {
    let parse = || -> io::Result<Vec<(f64, String)>> {
        let kids = children(&moov[8..])?;
        let Some(udta) = find(&kids, b"udta") else { return Ok(Vec::new()) };
        let inner = children(udta.body)?;
        let Some(chpl) = find(&inner, b"chpl") else { return Ok(Vec::new()) };
        let (version, b) = full_body(chpl)?;
        let mut at = if version == 1 { 4 } else { 0 };
        let count = *b.get(at).ok_or_else(|| invalid("short box"))?;
        at += 1;
        let mut out = Vec::new();
        for _ in 0..count {
            let start = be64(b, at)?;
            let len = *b.get(at + 8).ok_or_else(|| invalid("short box"))? as usize;
            let title = b.get(at + 9..at + 9 + len).ok_or_else(|| invalid("short box"))?;
            out.push((start as f64 / HUNDRED_NS, String::from_utf8_lossy(title).into_owned()));
            at += 9 + len;
        }
        Ok(out)
    };
    parse().unwrap_or_default()
}

const HUNDRED_NS: f64 = 10_000_000.0;

/// The `chpl` for `chapters`, in ffmpeg's version 1 layout.
fn write_chapters(o: &mut Vec<u8>, chapters: &[(f64, String)]) {
    full(o, b"chpl", 1, 0, |o| {
        o.u32(0);
        let count = chapters.len().min(255);
        o.push(count as u8);
        for (start, title) in &chapters[..count] {
            o.u64((start.max(0.0) * HUNDRED_NS).round() as u64);
            // At most 255 bytes, cut on a character boundary.
            let mut end = title.len().min(255);
            while !title.is_char_boundary(end) {
                end -= 1;
            }
            o.push(end as u8);
            o.extend_from_slice(&title.as_bytes()[..end]);
        }
    });
}

/// The chapters a cut from `begin` to `finish` keeps, moved to its start: one at zero for the
/// chapter already playing at `begin`, then every one that starts inside.
fn chapters_between(chapters: &[(f64, String)], begin: f64, finish: f64) -> Vec<(f64, String)> {
    let playing = chapters.iter().rfind(|c| c.0 <= begin + EPSILON).map(|c| (0.0, c.1.clone()));
    let inside = chapters.iter().filter(|c| c.0 > begin + EPSILON && c.0 < finish - EPSILON).map(|c| (c.0 - begin, c.1.clone()));
    playing.into_iter().chain(inside).collect()
}

pub fn info(path: &Path) -> io::Result<Info> {
    let movie = read_movie(&mut File::open(path)?)?;
    let video = main_video(&movie)?;
    let at = |s: &Sample| video.seconds(s, movie.timescale);
    let duration = video.samples.iter().map(|s| at(s) + s.duration as f64 / video.timescale as f64).fold(0.0, f64::max);
    let keyframes = video.samples.iter().filter(|s| s.key).map(at).map(|t| t.max(0.0)).collect();
    // A chapter at the very start or end marks nothing a cut could use.
    let markers = read_chapters(&movie.moov).into_iter().map(|c| c.0).filter(|&t| t > EPSILON && t < duration - EPSILON).collect();
    Ok(Info { duration, keyframes, markers })
}

/// Which samples of a track the trimmed file keeps.
pub(super) struct Cut<'a> {
    pub track: &'a Track,
    /// Original index of the box, so the right `trak` gets the right tables.
    pub index: usize,
    pub samples: &'a [Sample],
    /// New edit list: silence first (movie ticks), then media from this media time.
    pub empty: u64,
    pub media_time: i64,
    /// Movie ticks the trimmed file lasts; nothing shows after this.
    pub limit: u64,
    /// File offsets of this track's chunks in the new file, with the number of samples in each.
    pub chunks: Vec<(u64, u32)>,
}

impl Cut<'_> {
    fn media_ticks(&self) -> u64 {
        self.samples.iter().map(|s| s.duration as u64).sum()
    }

    /// Media ticks from the start of the edit to the end of the last sample shown, which with
    /// B-frames isn't the last one decoded.
    fn shown_ticks(&self) -> u64 {
        let base = self.samples[0].dts;
        let end = self.samples.iter().map(|s| s.dts - base + s.cts as i64 + s.duration as i64).max().unwrap_or(0);
        (end - self.media_time).max(0) as u64
    }

    /// Movie ticks from the start until this track ends.
    fn presented(&self, movie_timescale: u32) -> u64 {
        let media = self.shown_ticks();
        (self.empty + media * movie_timescale as u64 / self.track.timescale as u64).min(self.limit)
    }
}

/// The moment `start` would actually begin at: the keyframe at or before it.
pub fn snap(info: &Info, start: f64) -> f64 {
    info.keyframes.iter().copied().filter(|&k| k <= start + EPSILON).fold(info.keyframes.first().copied().unwrap_or(0.0), f64::max)
}

/// Writes `from` between `start` and `end` seconds to `to` (replacing it if it exists) and returns
/// where the copy really starts and ends in the original's time.
pub fn trim(from: &Path, to: &Path, start: f64, end: f64) -> io::Result<(f64, f64)> {
    let mut source = File::open(from)?;
    let movie = read_movie(&mut source)?;
    let video = main_video(&movie)?;
    let ts = movie.timescale;

    // Video: from the keyframe showing at `start` to the last frame that shows before `end`.
    let first = video
        .samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.key && video.seconds(s, ts) <= start + EPSILON)
        .map(|(i, _)| i)
        .next_back()
        .or_else(|| video.samples.iter().position(|s| s.key))
        .ok_or_else(|| invalid("no keyframes"))?;
    let begin = video.seconds(&video.samples[first], ts);
    let last = video.samples.iter().rposition(|s| video.seconds(s, ts) < end - EPSILON).filter(|&l| l >= first);
    let Some(last) = last else { return Err(invalid("nothing between those times")) };
    // With B-frames, frames shown after `end` can be needed to decode ones before it. They're kept
    // but the edit list stops before they show.
    let finish = video.samples[first..=last]
        .iter()
        .filter(|s| video.seconds(s, ts) < end - EPSILON)
        .map(|s| video.seconds(s, ts) + s.duration as f64 / video.timescale as f64)
        .fold(begin, f64::max);
    if finish - begin < MIN_SECONDS {
        return Err(invalid("too short to save"));
    }

    let mut cuts = Vec::new();
    for (index, track) in movie.tracks.iter().enumerate() {
        if movie.chapter_tracks.contains(&track.id) {
            continue;
        }
        let range = if std::ptr::eq(track, video) {
            Some((first, last))
        } else {
            // From the sample playing at `begin` (or the first after it) to the last starting before the end.
            let from = track.samples.iter().rposition(|s| track.seconds(s, ts) <= begin + EPSILON).unwrap_or(0);
            let to = track.samples.iter().rposition(|s| track.seconds(s, ts) < finish - EPSILON);
            to.filter(|&to| to >= from).map(|to| (from, to))
        };
        let Some((a, b)) = range else { continue };
        let first_sample = &track.samples[a];
        let shows = track.seconds(first_sample, ts) - begin;
        // Silence before it if it starts late, or skip into it if it started early.
        let (empty, media_time) = if shows >= 0.0 {
            ((shows * ts as f64).round() as u64, first_sample.cts as i64)
        } else {
            (0, first_sample.cts as i64 + (-shows * track.timescale as f64).round() as i64)
        };
        let limit = ((finish - begin) * ts as f64).round() as u64;
        cuts.push(Cut { track, index, samples: &track.samples[a..=b], empty, media_time, limit, chunks: Vec::new() });
    }

    // Interleave: every track's samples for each half second in turn.
    let mut order: Vec<(usize, usize, usize)> = Vec::new(); // (cut, from, to)
    let mut next = vec![0usize; cuts.len()];
    let mut window = 1;
    while cuts.iter().zip(&next).any(|(c, &n)| n < c.samples.len()) {
        let until = window as f64 * CHUNK_SECONDS;
        for (i, c) in cuts.iter().enumerate() {
            let from = next[i];
            let base = c.samples[0].dts;
            let mut to = from;
            while to < c.samples.len() && ((c.samples[to].dts - base) as f64 / c.track.timescale as f64) < until {
                to += 1;
            }
            if to > from {
                order.push((i, from, to));
                next[i] = to;
            }
        }
        window += 1;
    }
    let payload: u64 = cuts.iter().flat_map(|c| c.samples).map(|s| s.size as u64).sum();
    let wide = payload + (1 << 26) > u32::MAX as u64;

    let place = |cuts: &mut Vec<Cut>, base: u64| {
        for c in cuts.iter_mut() {
            c.chunks.clear();
        }
        let mut at = base;
        for &(i, from, to) in &order {
            cuts[i].chunks.push((at, (to - from) as u32));
            at += cuts[i].samples[from..to].iter().map(|s| s.size as u64).sum::<u64>();
        }
    };
    let ftyp = movie.ftyp.clone().unwrap_or_default();
    let chapters = chapters_between(&read_chapters(&movie.moov), begin, finish);
    place(&mut cuts, 0);
    let sized = new_moov(&movie, &cuts, wide, Some(&chapters))?.len() as u64;
    let data_at = ftyp.len() as u64 + sized + 16;
    place(&mut cuts, data_at);
    let moov = new_moov(&movie, &cuts, wide, Some(&chapters))?;
    debug_assert_eq!(moov.len() as u64, sized);

    let temporary = to.with_extension("trim.tmp");
    let result = (|| {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&temporary)?);
        out.write_all(&ftyp)?;
        out.write_all(&moov)?;
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&(payload + 16).to_be_bytes())?;
        // Runs of samples that sit together in the source are copied in one go.
        let mut run: Option<(u64, u64)> = None;
        for &(i, from, to) in &order {
            for s in &cuts[i].samples[from..to] {
                run = match run {
                    Some((start, len)) if start + len == s.offset => Some((start, len + s.size as u64)),
                    other => {
                        if let Some((start, len)) = other {
                            copy(&mut source, &mut out, start, len)?;
                        }
                        Some((s.offset, s.size as u64))
                    }
                };
            }
        }
        if let Some((start, len)) = run {
            copy(&mut source, &mut out, start, len)?;
        }
        out.into_inner().map_err(|e| e.into_error())?.sync_all()
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(e);
    }
    std::fs::rename(&temporary, to)?;
    Ok((begin, finish))
}

fn copy(from: &mut File, to: &mut impl Write, start: u64, length: u64) -> io::Result<()> {
    from.seek(SeekFrom::Start(start))?;
    if io::copy(&mut from.take(length), to)? != length {
        return Err(invalid("file ended early"));
    }
    Ok(())
}

/// Writes `value` over a version 0 (32-bit) or version 1 (64-bit) duration field of a full box.
fn patch_duration(whole: &[u8], header: usize, v0_at: usize, v1_at: usize, value: u64) -> Vec<u8> {
    let mut out = whole.to_vec();
    let version = out.get(header).copied().unwrap_or(0);
    if version == 1 {
        let at = header + 4 + v1_at;
        if let Some(s) = out.get_mut(at..at + 8) {
            s.copy_from_slice(&value.to_be_bytes());
        }
    } else {
        let at = header + 4 + v0_at;
        if let Some(s) = out.get_mut(at..at + 4) {
            s.copy_from_slice(&(value.min(u32::MAX as u64) as u32).to_be_bytes());
        }
    }
    out
}

fn header_len(c: &Child) -> usize {
    c.whole.len() - c.body.len()
}

/// The original `moov` with new sample tables, durations and edit lists. Tracks left with nothing
/// in range are dropped, and so is `mvex` (only a repaired recording has one); every other box is
/// copied as it was. `chapters`, when given, replace any the original had.
pub(super) fn new_moov(movie: &Movie, cuts: &[Cut], wide: bool, chapters: Option<&[(f64, String)]>) -> io::Result<Vec<u8>> {
    let ts = movie.timescale;
    let longest = cuts.iter().map(|c| c.presented(ts)).max().unwrap_or(0);
    let mut out = Vec::new();
    let kids = children(&movie.moov[8..])?;
    let mut track = 0;
    bx(&mut out, b"moov", |o| {
        for c in &kids {
            match &c.kind {
                b"mvhd" => o.extend_from_slice(&patch_duration(c.whole, header_len(c), 12, 20, longest)),
                b"trak" => {
                    if let Some(cut) = cuts.iter().find(|x| x.index == track) {
                        // Parsed fine once already, in read_movie.
                        let _ = new_trak(o, c, cut, ts, wide, chapters.is_some());
                    }
                    track += 1;
                }
                b"mvex" => {}
                b"udta" if chapters.is_some() => {
                    let _ = new_udta(o, c, chapters.unwrap_or_default());
                }
                _ => o.extend_from_slice(c.whole),
            }
        }
        // A file without user data gets some only to carry chapters.
        if let Some(list) = chapters.filter(|l| !l.is_empty()) {
            if find(&kids, b"udta").is_none() {
                bx(o, b"udta", |o| write_chapters(o, list));
            }
        }
    });
    Ok(out)
}

fn new_udta(o: &mut Vec<u8>, udta: &Child, chapters: &[(f64, String)]) -> io::Result<()> {
    let kids = children(udta.body)?;
    bx(o, b"udta", |o| {
        for c in kids.iter().filter(|c| &c.kind != b"chpl") {
            o.extend_from_slice(c.whole);
        }
        if !chapters.is_empty() {
            write_chapters(o, chapters);
        }
    });
    Ok(())
}

fn new_trak(o: &mut Vec<u8>, trak: &Child, cut: &Cut, movie_timescale: u32, wide: bool, chapters: bool) -> io::Result<()> {
    let kids = children(trak.body)?;
    bx(o, b"trak", |o| {
        for c in &kids {
            match &c.kind {
                b"tkhd" => {
                    o.extend_from_slice(&patch_duration(c.whole, header_len(c), 16, 24, cut.presented(movie_timescale)));
                    edit_list(o, cut, movie_timescale);
                }
                b"edts" => {}
                // Points at a chapter track, which a trim drops.
                b"tref" if chapters => {
                    let inner = children(c.body).unwrap_or_default();
                    if inner.iter().any(|r| &r.kind != b"chap") {
                        bx(o, b"tref", |o| inner.iter().filter(|r| &r.kind != b"chap").for_each(|r| o.extend_from_slice(r.whole)));
                    }
                }
                b"mdia" => {
                    let _ = new_mdia(o, c, cut, wide);
                }
                _ => o.extend_from_slice(c.whole),
            }
        }
    });
    Ok(())
}

fn edit_list(o: &mut Vec<u8>, cut: &Cut, movie_timescale: u32) {
    let media = cut.presented(movie_timescale).saturating_sub(cut.empty);
    let big = cut.empty > u32::MAX as u64 || media > u32::MAX as u64 || cut.media_time > i32::MAX as i64;
    bx(o, b"edts", |o| {
        full(o, b"elst", big as u8, 0, |o| {
            let entry = |o: &mut Vec<u8>, duration: u64, media_time: i64| {
                if big {
                    o.u64(duration);
                    o.u64(media_time as u64);
                } else {
                    o.u32(duration as u32);
                    o.u32(media_time as i32 as u32);
                }
                o.u32(0x0001_0000);
            };
            o.u32(if cut.empty > 0 { 2 } else { 1 });
            if cut.empty > 0 {
                entry(o, cut.empty, -1);
            }
            entry(o, media, cut.media_time);
        });
    });
}

fn new_mdia(o: &mut Vec<u8>, mdia: &Child, cut: &Cut, wide: bool) -> io::Result<()> {
    let kids = children(mdia.body)?;
    bx(o, b"mdia", |o| {
        for c in &kids {
            match &c.kind {
                b"mdhd" => o.extend_from_slice(&patch_duration(c.whole, header_len(c), 12, 20, cut.media_ticks())),
                b"minf" => {
                    let _ = new_minf(o, c, cut, wide);
                }
                _ => o.extend_from_slice(c.whole),
            }
        }
    });
    Ok(())
}

fn new_minf(o: &mut Vec<u8>, minf: &Child, cut: &Cut, wide: bool) -> io::Result<()> {
    let kids = children(minf.body)?;
    let stbl = find(&kids, b"stbl").ok_or_else(|| invalid("no stbl"))?;
    let stsd = find(&children(stbl.body)?, b"stsd").ok_or_else(|| invalid("no stsd"))?.whole.to_vec();
    bx(o, b"minf", |o| {
        for c in &kids {
            if &c.kind == b"stbl" {
                new_stbl(o, &stsd, cut, wide);
            } else {
                o.extend_from_slice(c.whole);
            }
        }
    });
    Ok(())
}

/// Runs of equal values, as (count, value).
fn runs<T: PartialEq + Copy>(values: impl Iterator<Item = T>) -> Vec<(u32, T)> {
    let mut out: Vec<(u32, T)> = Vec::new();
    for v in values {
        match out.last_mut() {
            Some((n, last)) if *last == v => *n += 1,
            _ => out.push((1, v)),
        }
    }
    out
}

fn new_stbl(o: &mut Vec<u8>, stsd: &[u8], cut: &Cut, wide: bool) {
    let samples = cut.samples;
    bx(o, b"stbl", |o| {
        o.extend_from_slice(stsd);
        let stts = runs(samples.iter().map(|s| s.duration));
        full(o, b"stts", 0, 0, |o| {
            o.u32(stts.len() as u32);
            for (count, delta) in stts {
                o.u32(count);
                o.u32(delta);
            }
        });
        if cut.track.has_ctts {
            let ctts = runs(samples.iter().map(|s| s.cts));
            full(o, b"ctts", cut.track.ctts_version, 0, |o| {
                o.u32(ctts.len() as u32);
                for (count, offset) in ctts {
                    o.u32(count);
                    o.u32(offset as u32);
                }
            });
        }
        if cut.track.has_stss {
            let keys: Vec<u32> = samples.iter().enumerate().filter(|(_, s)| s.key).map(|(i, _)| i as u32 + 1).collect();
            full(o, b"stss", 0, 0, |o| {
                o.u32(keys.len() as u32);
                for k in keys {
                    o.u32(k);
                }
            });
        }
        full(o, b"stsc", 0, 0, |o| {
            let mut list: Vec<(u32, u32)> = Vec::new();
            for (i, &(_, n)) in cut.chunks.iter().enumerate() {
                if list.last().map(|r| r.1) != Some(n) {
                    list.push((i as u32 + 1, n));
                }
            }
            o.u32(list.len() as u32);
            for (first, n) in list {
                o.u32(first);
                o.u32(n);
                o.u32(1);
            }
        });
        full(o, b"stsz", 0, 0, |o| {
            o.u32(0);
            o.u32(samples.len() as u32);
            for s in samples {
                o.u32(s.size);
            }
        });
        if wide {
            full(o, b"co64", 0, 0, |o| {
                o.u32(cut.chunks.len() as u32);
                for c in &cut.chunks {
                    o.u64(c.0);
                }
            });
        } else {
            full(o, b"stco", 0, 0, |o| {
                o.u32(cut.chunks.len() as u32);
                for c in &cut.chunks {
                    o.u32(c.0 as u32);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_back_to_the_keyframe_before() {
        let info = Info { duration: 10.0, keyframes: vec![0.0, 2.0, 4.0, 6.0], markers: vec![] };
        assert_eq!(snap(&info, 3.9), 2.0);
        assert_eq!(snap(&info, 4.0), 4.0);
        assert_eq!(snap(&info, 0.0), 0.0);
        assert_eq!(snap(&info, 9.0), 6.0);
    }

    use super::super::boxes::{bx, full, Put};

    /// A one-track file with 10 one-byte samples in one chunk, and `tables` as its sample tables
    /// after stsd (stts, stsz and any others; stsc and stco come after them).
    fn crafted(name: &str, tables: impl FnOnce(&mut Vec<u8>)) -> std::path::PathBuf {
        let mut file = Vec::new();
        bx(&mut file, b"ftyp", |o| o.extend_from_slice(b"isom\0\0\0\0"));
        let data_at = file.len() as u32 + 8;
        bx(&mut file, b"mdat", |o| o.zeros(10));
        bx(&mut file, b"moov", |o| {
            full(o, b"mvhd", 0, 0, |o| {
                o.zeros(8);
                o.u32(1000);
                o.zeros(84);
            });
            bx(o, b"trak", |o| {
                bx(o, b"mdia", |o| {
                    full(o, b"mdhd", 0, 0, |o| {
                        o.zeros(8);
                        o.u32(10);
                        o.zeros(8);
                    });
                    full(o, b"hdlr", 0, 0, |o| {
                        o.u32(0);
                        o.extend_from_slice(b"vide");
                        o.zeros(13);
                    });
                    bx(o, b"minf", |o| {
                        bx(o, b"stbl", |o| {
                            full(o, b"stsd", 0, 0, |o| o.u32(0));
                            tables(o);
                            full(o, b"stsc", 0, 0, |o| {
                                o.u32(1);
                                o.u32(1);
                                o.u32(10);
                                o.u32(1);
                            });
                            full(o, b"stco", 0, 0, |o| {
                                o.u32(1);
                                o.u32(data_at);
                            });
                        });
                    });
                });
            });
        });
        let dir = std::env::temp_dir().join(format!("tape-trim-crafted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, file).unwrap();
        path
    }

    fn stts(o: &mut Vec<u8>, count: u32) {
        full(o, b"stts", 0, 0, |o| {
            o.u32(1);
            o.u32(count);
            o.u32(1);
        });
    }

    fn stsz(o: &mut Vec<u8>, fixed: u32, count: u32) {
        full(o, b"stsz", 0, 0, |o| {
            o.u32(fixed);
            o.u32(count);
            if fixed == 0 {
                for _ in 0..count {
                    o.u32(1);
                }
            }
        });
    }

    #[test]
    fn a_sound_crafted_file_reads() {
        let path = crafted("sound.mp4", |o| {
            stts(o, 10);
            stsz(o, 1, 10);
        });
        let info = info(&path).unwrap();
        assert_eq!(info.keyframes.len(), 10);
        assert!((info.duration - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_huge_timing_count_is_refused_without_allocating_it() {
        let path = crafted("stts.mp4", |o| {
            stts(o, u32::MAX);
            stsz(o, 1, 10);
        });
        assert!(info(&path).is_err());
    }

    #[test]
    fn a_fixed_size_table_larger_than_the_file_is_refused() {
        let path = crafted("stsz.mp4", |o| {
            stts(o, 10);
            stsz(o, 1, u32::MAX);
        });
        assert!(info(&path).is_err());
    }

    #[test]
    fn a_huge_offset_table_count_is_capped() {
        let path = crafted("ctts.mp4", |o| {
            stts(o, 10);
            stsz(o, 1, 10);
            full(o, b"ctts", 0, 0, |o| {
                o.u32(1);
                o.u32(u32::MAX);
                o.u32(0);
            });
        });
        assert_eq!(info(&path).unwrap().keyframes.len(), 10);
    }

    #[test]
    fn a_child_box_size_that_wraps_is_refused() {
        let path = crafted("wrap.mp4", |o| {
            stts(o, 10);
            stsz(o, 1, 10);
            o.u32(1);
            o.extend_from_slice(b"junk");
            o.u64(u64::MAX - 3);
        });
        assert!(info(&path).is_err());
    }

    #[test]
    fn chapters_round_trip_and_follow_the_cut() {
        let list = vec![(0.0, "Start".to_string()), (2.5, "Ace".to_string()), (7.25, "é".repeat(200))];
        let mut moov = Vec::new();
        bx(&mut moov, b"moov", |o| bx(o, b"udta", |o| write_chapters(o, &list)));
        let read = read_chapters(&moov);
        assert_eq!(read.len(), 3);
        assert_eq!(read[1], (2.5, "Ace".to_string()));
        assert_eq!(read[2].1, "é".repeat(127), "cut to 255 bytes on a character boundary");
        let kept = chapters_between(&list, 2.0, 7.0);
        assert_eq!(kept, [(0.0, "Start".to_string()), (0.5, "Ace".to_string())]);
        assert!(chapters_between(&[], 1.0, 2.0).is_empty());
    }

    #[test]
    fn runs_group_equal_neighbours() {
        assert_eq!(runs([1, 1, 2, 1].into_iter()), vec![(2, 1), (1, 2), (1, 1)]);
    }
}
