//! Chapters as the Nero `chpl` box in `moov/udta`, which is what ffmpeg's mov muxer writes for
//! MP4 and reads back as chapters, and what players like mpv and VLC show on the seek bar.
//! Recordings carry the markers set during them this way.
//!
//! Layout, matching `mov_write_chpl_tag` in ffmpeg's movenc.c: version 1 and zero flags, four
//! reserved bytes, a one-byte count (so at most 255), then per chapter its start as a 64-bit count
//! of 100 ns units and its title as a one-byte length and that many bytes of UTF-8.

use super::boxes::{bx, Put};

/// ffmpeg's chapter timebase in `chpl`: 1/10,000,000 of a second.
const UNITS_PER_SECOND: f64 = 10_000_000.0;
/// The count is one byte.
pub const MAX_CHAPTERS: usize = 255;

/// The `chpl` box for `chapters`, given as (start in seconds, title). Titles longer than 255
/// bytes are cut at a character boundary, and chapters past the 255th are left out.
pub fn chpl(chapters: &[(f64, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    let count = chapters.len().min(MAX_CHAPTERS);
    bx(&mut out, b"chpl", |o| {
        o.u32(0x0100_0000); // version 1, no flags
        o.u32(0); // reserved
        o.push(count as u8);
        for (seconds, title) in &chapters[..count] {
            o.u64((seconds.max(0.0) * UNITS_PER_SECOND).round() as u64);
            let mut end = title.len().min(255);
            while !title.is_char_boundary(end) {
                end -= 1;
            }
            o.push(end as u8);
            o.extend_from_slice(&title.as_bytes()[..end]);
        }
    });
    out
}

/// A `udta` box holding the `chpl` box for `chapters`, to go inside `moov`. Empty when there are
/// none, so a file without markers has no chapter box at all.
pub fn udta(chapters: &[(f64, String)]) -> Vec<u8> {
    if chapters.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    bx(&mut out, b"udta", |o| o.extend_from_slice(&chpl(chapters)));
    out
}

/// "Marker 1", "Marker 2"... at these times in seconds.
pub fn markers(seconds: &[f64]) -> Vec<(f64, String)> {
    seconds.iter().enumerate().map(|(i, &s)| (s, format!("Marker {}", i + 1))).collect()
}

/// Reads a whole `chpl` box (size, type and body) as (start in seconds, title). Takes version 0
/// as well, which has no reserved word. Anything damaged ends the list where it stops making
/// sense rather than failing, since chapters are only a help.
pub fn parse_chpl(data: &[u8]) -> Vec<(f64, String)> {
    let mut out = Vec::new();
    if data.len() < 9 || &data[4..8] != b"chpl" {
        return out;
    }
    let size = (u32::from_be_bytes(data[..4].try_into().unwrap()) as usize).clamp(8, data.len());
    let body = &data[8..size];
    let Some(&version) = body.first() else { return out };
    let mut at = if version == 0 { 4 } else { 8 };
    let Some(&count) = body.get(at) else { return out };
    at += 1;
    for _ in 0..count {
        let Some(head) = body.get(at..at + 9) else { break };
        let start = u64::from_be_bytes(head[..8].try_into().unwrap());
        let length = head[8] as usize;
        let Some(title) = body.get(at + 9..at + 9 + length) else { break };
        out.push((start as f64 / UNITS_PER_SECOND, String::from_utf8_lossy(title).into_owned()));
        at += 9 + length;
    }
    out
}

/// The chapters in a whole `moov` box (size, type and body), from its `udta/chpl`. Empty if it
/// has none.
pub fn from_moov(moov: &[u8]) -> Vec<(f64, String)> {
    if moov.len() < 8 || &moov[4..8] != b"moov" {
        return Vec::new();
    }
    let size = (u32::from_be_bytes(moov[..4].try_into().unwrap()) as usize).clamp(8, moov.len());
    child(&moov[8..size], b"udta").and_then(|udta| child(&udta[8..], b"chpl")).map(parse_chpl).unwrap_or_default()
}

/// The first box of type `kind` in a run of boxes, whole.
fn child<'a>(mut data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    while data.len() >= 8 {
        let size = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
        if size < 8 || size > data.len() {
            return None;
        }
        if &data[4..8] == kind {
            return Some(&data[..size]);
        }
        data = &data[size..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_what_ffmpeg_writes() {
        let data = chpl(&[(1.5, "Marker 1".into())]);
        let mut expected = vec![0, 0, 0, 34];
        expected.extend_from_slice(b"chpl");
        expected.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 1]);
        expected.extend_from_slice(&15_000_000u64.to_be_bytes());
        expected.push(8);
        expected.extend_from_slice(b"Marker 1");
        assert_eq!(data, expected);
    }

    #[test]
    fn reads_back_what_it_writes() {
        let chapters = markers(&[0.0, 12.25, 3600.5]);
        assert_eq!(parse_chpl(&chpl(&chapters)), chapters);
        let mut moov = Vec::new();
        bx(&mut moov, b"moov", |o| {
            bx(o, b"mvhd", |o| o.zeros(100));
            o.extend_from_slice(&udta(&chapters));
        });
        assert_eq!(from_moov(&moov), chapters);
    }

    #[test]
    fn reads_version_0() {
        let mut data = Vec::new();
        bx(&mut data, b"chpl", |o| {
            o.u32(0);
            o.push(1);
            o.u64(20_000_000);
            o.push(1);
            o.push(b'a');
        });
        assert_eq!(parse_chpl(&data), vec![(2.0, "a".to_string())]);
    }

    #[test]
    fn nothing_to_write_is_no_box() {
        assert!(udta(&[]).is_empty());
        let mut moov = Vec::new();
        bx(&mut moov, b"moov", |o| bx(o, b"mvhd", |o| o.zeros(100)));
        assert!(from_moov(&moov).is_empty());
    }

    #[test]
    fn long_titles_and_counts_are_cut_to_fit() {
        let long = "é".repeat(200); // 400 bytes
        let many: Vec<(f64, String)> = (0..300).map(|i| (i as f64, long.clone())).collect();
        let read = parse_chpl(&chpl(&many));
        assert_eq!(read.len(), 255);
        assert_eq!(read[0].1, "é".repeat(127));
    }

    #[test]
    fn damage_ends_the_list_without_failing() {
        let data = chpl(&markers(&[1.0, 2.0]));
        assert_eq!(parse_chpl(&data[..data.len() - 3]).len(), 1);
        assert!(parse_chpl(&data[..6]).is_empty());
        assert!(parse_chpl(b"\0\0\0\x09freex").is_empty());
        assert!(from_moov(b"\0\0\0\x10moov\0\0\0\x50udta").is_empty());
    }
}
