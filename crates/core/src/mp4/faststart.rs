//! Moves a finished MP4's `moov` in front of its `mdat`, so a player can start before it has the
//! whole file: what ffmpeg's `-movflags +faststart` does. For files from other muxers, here the
//! shared-link copy Media Foundation writes. Works through the file in a stream with only the
//! `moov` in memory, and writes a new file next to it that replaces the old one when done.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Boxes whose children hold the chunk offset tables.
const CONTAINERS: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];
/// A `moov` larger than this isn't one of ours, so nothing gets read into memory blindly.
pub(crate) const MAX_MOOV: u64 = 64 << 20;

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// Top-level boxes as (type, start, length).
pub(crate) fn top_level(file: &mut File) -> io::Result<Vec<([u8; 4], u64, u64)>> {
    let end = file.metadata()?.len();
    let mut boxes = Vec::new();
    let mut at = 0;
    while at + 8 <= end {
        file.seek(SeekFrom::Start(at))?;
        let mut head = [0u8; 16];
        file.read_exact(&mut head[..8])?;
        let mut size = u32::from_be_bytes(head[..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = head[4..8].try_into().unwrap();
        if size == 1 {
            file.read_exact(&mut head[8..16])?;
            size = u64::from_be_bytes(head[8..16].try_into().unwrap());
        } else if size == 0 {
            size = end - at;
        }
        if size < 8 || at + size > end {
            return Err(invalid("damaged MP4 box"));
        }
        boxes.push((kind, at, size));
        at += size;
    }
    Ok(boxes)
}

/// Adds `shift` to every chunk offset under `data`, a run of boxes.
fn shift_offsets(data: &mut [u8], shift: u64) -> io::Result<()> {
    let mut at = 0;
    while at + 8 <= data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = data[at + 4..at + 8].try_into().unwrap();
        // 64-bit sizes never occur inside a moov of a sane size.
        if size < 8 || at + size > data.len() {
            return Err(invalid("damaged box in moov"));
        }
        let body = &mut data[at + 8..at + size];
        if CONTAINERS.iter().any(|c| **c == kind) {
            shift_offsets(body, shift)?;
        } else if &kind == b"stco" || &kind == b"co64" {
            let wide = &kind == b"co64";
            let width = if wide { 8 } else { 4 };
            let count = u32::from_be_bytes(body.get(4..8).ok_or_else(|| invalid("short stco"))?.try_into().unwrap()) as usize;
            if body.len() < 8 + count * width {
                return Err(invalid("short stco"));
            }
            for entry in body[8..8 + count * width].chunks_exact_mut(width) {
                if wide {
                    let v = u64::from_be_bytes(entry.try_into().unwrap()) + shift;
                    entry.copy_from_slice(&v.to_be_bytes());
                } else {
                    let v = u32::from_be_bytes(entry.try_into().unwrap()) as u64 + shift;
                    let v = u32::try_from(v).map_err(|_| invalid("file too large to move its index"))?;
                    entry.copy_from_slice(&v.to_be_bytes());
                }
            }
        }
        at += size;
    }
    Ok(())
}

fn copy_range(from: &mut File, to: &mut impl Write, start: u64, length: u64) -> io::Result<()> {
    from.seek(SeekFrom::Start(start))?;
    let copied = io::copy(&mut BufReader::with_capacity(1 << 20, from.take(length)), to)?;
    if copied != length {
        return Err(invalid("file ended early"));
    }
    Ok(())
}

/// Does nothing if the `moov` already comes first. True if the file was rewritten.
pub fn faststart(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let boxes = top_level(&mut file)?;
    let moov = boxes.iter().position(|b| &b.0 == b"moov").ok_or_else(|| invalid("no moov"))?;
    let mdat = boxes.iter().position(|b| &b.0 == b"mdat").ok_or_else(|| invalid("no mdat"))?;
    if moov < mdat {
        return Ok(false);
    }
    let (_, moov_start, moov_len) = boxes[moov];
    if moov_len > MAX_MOOV {
        return Err(invalid("moov too large"));
    }
    let mut index = vec![0u8; moov_len as usize];
    file.seek(SeekFrom::Start(moov_start))?;
    file.read_exact(&mut index)?;
    // Everything from the first mdat on moves down by the moov's length.
    shift_offsets(&mut index[8..], moov_len)?;

    let insert = boxes[mdat].1;
    let end = file.metadata()?.len();
    let temporary = path.with_extension("faststart.tmp");
    let result = (|| {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&temporary)?);
        copy_range(&mut file, &mut out, 0, insert)?;
        out.write_all(&index)?;
        copy_range(&mut file, &mut out, insert, moov_start - insert)?;
        copy_range(&mut file, &mut out, moov_start + moov_len, end - moov_start - moov_len)?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()
    })();
    drop(file);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(e);
    }
    std::fs::rename(&temporary, path)?;
    Ok(true)
}
