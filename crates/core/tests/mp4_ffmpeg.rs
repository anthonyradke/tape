//! Checks the muxer against a real decoder: encodes short streams with ffmpeg, muxes them with
//! tape's writers, and has ffmpeg read every frame back. Skips if ffmpeg isn't installed.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::Value;
use tape_core::mp4::codec::{nal_units, parse_frame};
use tape_core::mp4::{write_clip, Mp4Writer};
use tape_core::packet::{AudioFormat, Codec, Packet, Tracks, VideoFormat, AAC_FRAME};

const FPS: u32 = 60;
const FRAMES: usize = 300; // 5 s, long enough to span fragments
const GOP: usize = 60;
const AUDIO_ASC: [u8; 2] = [0x11, 0x90];

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tape-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).output().unwrap();
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// One encoded frame per entry, as the encoder would hand it over.
fn encode(codec: Codec) -> Vec<Vec<u8>> {
    let gop = GOP.to_string();
    let frames = FRAMES.to_string();
    let source = format!("testsrc2=size=640x360:rate={FPS}");
    let common = ["-f", "lavfi", "-i", &source, "-frames:v", &frames, "-pix_fmt", "yuv420p", "-g", &gop];
    match codec {
        Codec::H264 => {
            let path = tmp("v.h264");
            let p = path.to_str().unwrap();
            let mut a = common.to_vec();
            a.extend(["-c:v", "libx264", "-bf", "0", "-preset", "veryfast", "-bsf:v", "h264_metadata=aud=insert", "-f", "h264", p]);
            ffmpeg(&a);
            split_annexb(&fs::read(&path).unwrap(), |nal| nal[0] & 0x1f == 9)
        }
        Codec::Hevc => {
            let path = tmp("v.hevc");
            let p = path.to_str().unwrap();
            let mut a = common.to_vec();
            a.extend(["-c:v", "libx265", "-x265-params", "bframes=0:log-level=error:keyint=60:min-keyint=60:repeat-headers=1", "-bsf:v", "hevc_metadata=aud=insert", "-f", "hevc", p]);
            ffmpeg(&a);
            split_annexb(&fs::read(&path).unwrap(), |nal| (nal[0] >> 1) & 0x3f == 35)
        }
        Codec::Av1 => {
            let path = tmp("v.ivf");
            let p = path.to_str().unwrap();
            let mut a = common.to_vec();
            a.extend(["-c:v", "libsvtav1", "-preset", "10", "-svtav1-params", "lookahead=0:tune=0:pred-struct=1:keyint=60:irefresh-type=2", "-f", "ivf", p]);
            ffmpeg(&a);
            split_ivf(&fs::read(&path).unwrap())
        }
    }
}

fn split_annexb(data: &[u8], starts_au: impl Fn(&[u8]) -> bool) -> Vec<Vec<u8>> {
    let mut frames: Vec<Vec<u8>> = Vec::new();
    for nal in nal_units(data) {
        if starts_au(nal) || frames.is_empty() {
            frames.push(Vec::new());
        }
        let f = frames.last_mut().unwrap();
        f.extend_from_slice(&[0, 0, 0, 1]);
        f.extend_from_slice(nal);
    }
    frames
}

fn split_ivf(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = u16::from_le_bytes([data[6], data[7]]) as usize;
    while i + 12 <= data.len() {
        let size = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        out.push(data[i + 12..i + 12 + size].to_vec());
        i += 12 + size;
    }
    out
}

fn aac_frames() -> Vec<Vec<u8>> {
    let path = tmp("a.aac");
    ffmpeg(&["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "6", "-ac", "2", "-c:a", "aac", "-b:a", "192k", "-f", "adts", path.to_str().unwrap()]);
    let data = fs::read(&path).unwrap();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 7 <= data.len() {
        let len = ((data[i + 3] as usize & 3) << 11) | ((data[i + 4] as usize) << 3) | (data[i + 5] as usize >> 5);
        out.push(data[i + 7..i + len].to_vec());
        i += len;
    }
    out
}

/// Stream like the engine's: `base_*` shift everything along the timeline as a long-running
/// engine would, and audio starts `audio_lag` samples after the first frame.
fn stream(codec: Codec, audio_tracks: usize, audio_lag: i64) -> (Tracks, Vec<Packet>) {
    let frames = encode(codec);
    assert_eq!(frames.len(), FRAMES);
    let aac = aac_frames();
    const BASE_SECONDS: i64 = 1000;
    let base_video = BASE_SECONDS * FPS as i64;
    let base_audio = BASE_SECONDS * 48_000 + audio_lag;

    let mut config = None;
    let mut video = Vec::new();
    for (n, f) in frames.iter().enumerate() {
        let parsed = parse_frame(codec, f);
        if config.is_none() {
            config = parsed.config;
        }
        video.push(Packet { track: 0, pts: base_video + n as i64, key: n % GOP == 0, data: Arc::new(parsed.sample) });
    }
    let tracks = Tracks {
        video: VideoFormat { codec, width: 640, height: 360, fps: FPS, config: config.expect("no parameter sets") },
        audio: (0..audio_tracks).map(|i| AudioFormat { name: format!("Audio {i}"), asc: AUDIO_ASC.to_vec() }).collect(),
    };
    // Merge as the sink sees them: audio arrives about 150 ms behind its timestamps.
    let mut merged: Vec<(i64, Packet)> = video.into_iter().map(|p| (p.micros(FPS), p)).collect();
    for t in 0..audio_tracks {
        for (n, data) in aac.iter().enumerate().take(5 * 48_000 / AAC_FRAME) {
            let pts = base_audio + (n * AAC_FRAME) as i64;
            let p = Packet { track: t as u8 + 1, pts, key: true, data: Arc::new(data.clone()) };
            merged.push((p.micros(48_000) + 150_000, p));
        }
    }
    merged.sort_by_key(|(t, _)| *t);
    (tracks, merged.into_iter().map(|(_, p)| p).collect())
}

fn record(path: &Path, tracks: &Tracks, packets: &[Packet], finish: bool) {
    let file = BufWriter::new(File::create(path).unwrap());
    let mut w = Mp4Writer::new(file, tracks).unwrap();
    for p in packets {
        w.push(p, tracks.timescale(p.track)).unwrap();
    }
    if finish {
        w.finish().unwrap();
    } else {
        std::mem::forget(w); // what a crash leaves: only what was flushed
    }
}

fn clip(path: &Path, tracks: &Tracks, packets: &[Packet]) {
    write_clip(BufWriter::new(File::create(path).unwrap()), tracks, packets).unwrap();
}

fn probe(path: &Path) -> Value {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-count_packets", "-show_streams", "-show_format", "-of", "json"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stderr.is_empty(), "ffprobe complained: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Decodes everything; any error from the decoder fails the test.
fn decode_clean(path: &Path) {
    let out = Command::new("ffmpeg").args(["-v", "error", "-xerror", "-i"]).arg(path).args(["-map", "0", "-f", "null", "-"]).output().unwrap();
    assert!(out.status.success() && out.stderr.is_empty(), "decode: {}", String::from_utf8_lossy(&out.stderr));
}

fn frames_of(info: &Value, kind: &str) -> Vec<u64> {
    info["streams"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["codec_type"] == kind)
        .map(|s| s["nb_read_frames"].as_str().unwrap().parse().unwrap())
        .collect()
}

fn top_level_boxes(path: &Path) -> Vec<String> {
    let data = fs::read(path).unwrap();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 8 <= data.len() {
        let mut size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        if size == 1 {
            size = u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap()) as usize;
        }
        out.push(String::from_utf8_lossy(&data[i + 4..i + 8]).into_owned());
        assert!(size >= 8, "bad box at {i}");
        i += size;
    }
    assert_eq!(i, data.len(), "boxes must cover the whole file");
    out
}

fn check_all_writers(codec: Codec, name: &str) {
    if !have_ffmpeg() {
        eprintln!("skipping: no ffmpeg");
        return;
    }
    let (tracks, packets) = stream(codec, 2, 400);

    let recorded = tmp(&format!("{name}-recording.mp4"));
    record(&recorded, &tracks, &packets, true);
    assert_eq!(top_level_boxes(&recorded), ["ftyp", "free", "mdat", "moov"]);
    let info = probe(&recorded);
    assert_eq!(frames_of(&info, "video"), [FRAMES as u64]);
    assert_eq!(frames_of(&info, "audio").len(), 2);
    decode_clean(&recorded);

    let clipped = tmp(&format!("{name}-clip.mp4"));
    clip(&clipped, &tracks, &packets);
    assert_eq!(top_level_boxes(&clipped), ["ftyp", "moov", "mdat"]);
    let info = probe(&clipped);
    assert_eq!(frames_of(&info, "video"), [FRAMES as u64]);
    let v = &info["streams"][0];
    assert_eq!(v["width"], 640);
    assert_eq!(v["r_frame_rate"], format!("{FPS}/1"));
    // ffprobe reports HEVC and AV1 color from the bitstream, and these test streams carry none.
    let data = fs::read(&clipped).unwrap();
    let colr = [&b"nclx"[..], &[0, 1, 0, 1, 0, 1, 0]].concat();
    assert!(data.windows(colr.len()).any(|w| w == colr), "colr box missing");
    let duration: f64 = info["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((duration - 5.0).abs() < 0.05, "duration {duration}");
    decode_clean(&clipped);
}

#[test]
fn h264_recording_and_clip_decode() {
    check_all_writers(Codec::H264, "h264");
}

#[test]
fn hevc_recording_and_clip_decode() {
    check_all_writers(Codec::Hevc, "hevc");
}

#[test]
fn av1_recording_and_clip_decode() {
    check_all_writers(Codec::Av1, "av1");
}

#[test]
fn file_left_by_a_crash_still_plays() {
    if !have_ffmpeg() {
        return;
    }
    let (tracks, packets) = stream(Codec::H264, 1, 0);
    let path = tmp("crash.mp4");
    record(&path, &tracks, &packets, false);
    let boxes = top_level_boxes(&path);
    assert_eq!(boxes[1], "moov");
    assert!(boxes.contains(&"moof".to_string()));
    let info = probe(&path);
    let frames = frames_of(&info, "video")[0];
    // Fragments close on 1 s keyframes once 2 s have passed: all but the last fragment survive.
    assert!(frames >= 180 && frames < FRAMES as u64, "{frames} frames survived");
    decode_clean(&path);
}

#[test]
fn audio_starting_late_stays_in_sync() {
    if !have_ffmpeg() {
        return;
    }
    const LAG: i64 = 24_000; // half a second
    let (tracks, packets) = stream(Codec::H264, 1, LAG);
    let path = tmp("late.mp4");
    clip(&path, &tracks, &packets);
    let first_audio_pts = |path: &Path| -> f64 {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "a:0", "-read_intervals", "%+#1", "-show_entries", "packet=pts_time", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().trim_end_matches(',').parse().unwrap()
    };
    let at = first_audio_pts(&path);
    assert!((at - 0.5).abs() < 0.03, "audio starts at {at}");
    let path = tmp("late-rec.mp4");
    record(&path, &tracks, &packets, true);
    let at = first_audio_pts(&path);
    assert!((at - 0.5).abs() < 0.03, "audio starts at {at}");
}

#[test]
fn dropped_frames_keep_later_frames_on_time() {
    if !have_ffmpeg() {
        return;
    }
    let (tracks, mut packets) = stream(Codec::H264, 0, 0);
    // Lose half a second of non-keyframes in the first GOP: no frame after them may move.
    packets.retain(|p| !(1000 * 60 + 10..1000 * 60 + 40).contains(&p.pts));
    let path = tmp("drops.mp4");
    clip(&path, &tracks, &packets);
    let info = probe(&path);
    // Counted as packets: this test cuts reference frames out of a finished stream, which a real
    // dropped frame (never encoded) doesn't do, so the decoder may discard a damaged frame.
    assert_eq!(info["streams"][0]["nb_read_packets"], (FRAMES - 30).to_string());
    let duration: f64 = info["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((duration - 5.0).abs() < 0.05, "duration {duration}");
}
