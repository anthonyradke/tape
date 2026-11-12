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

/// A name no other test running at the same time will use.
fn scratch(ext: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    tmp(&format!("scratch-{}.{ext}", NEXT.fetch_add(1, Ordering::Relaxed)))
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
            let path = scratch("h264");
            let p = path.to_str().unwrap();
            let mut a = common.to_vec();
            a.extend(["-c:v", "libx264", "-bf", "0", "-preset", "veryfast", "-bsf:v", "h264_metadata=aud=insert", "-f", "h264", p]);
            ffmpeg(&a);
            split_annexb(&fs::read(&path).unwrap(), |nal| nal[0] & 0x1f == 9)
        }
        Codec::Hevc => {
            let path = scratch("hevc");
            let p = path.to_str().unwrap();
            let mut a = common.to_vec();
            a.extend(["-c:v", "libx265", "-x265-params", "bframes=0:log-level=error:keyint=60:min-keyint=60:repeat-headers=1", "-bsf:v", "hevc_metadata=aud=insert", "-f", "hevc", p]);
            ffmpeg(&a);
            split_annexb(&fs::read(&path).unwrap(), |nal| (nal[0] >> 1) & 0x3f == 35)
        }
        Codec::Av1 => {
            let path = scratch("ivf");
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
    let path = scratch("aac");
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
    // Fragments close on the first 1 s keyframe once 2 s have passed: 0-2 s and 2-4 s are on disk,
    // the last second is still held. Longer fragments would leave fewer.
    assert_eq!(frames, 240, "{frames} frames survived");
    decode_clean(&path);
}

#[test]
fn every_step_of_finishing_still_plays() {
    if !have_ffmpeg() {
        return;
    }
    let (tracks, packets) = stream(Codec::H264, 1, 0);
    let mut w = Mp4Writer::new(std::io::Cursor::new(Vec::new()), &tracks).unwrap();
    for p in &packets {
        w.push(p, tracks.timescale(p.track)).unwrap();
    }
    // What the disk holds at each point finishing makes sure of.
    let mut states = Vec::new();
    w.finish_with(|out| {
        states.push(out.get_ref().clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(states.len(), 2);
    for (n, data) in states.iter().enumerate() {
        let path = tmp(&format!("finishing-{n}.mp4"));
        fs::write(&path, data).unwrap();
        let frames = frames_of(&probe(&path), "video")[0];
        assert_eq!(frames, FRAMES as u64, "step {n}: {frames} frames");
    }
    let finished = tmp("finishing-1.mp4");
    assert_eq!(top_level_boxes(&finished), ["ftyp", "free", "mdat", "moov"]);
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

fn frame_hashes(path: &Path) -> String {
    let out = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path).args(["-map", "0", "-f", "framemd5", "-"]).output().unwrap();
    assert!(out.status.success(), "framemd5: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n")
}

/// The shared-link copy comes from Media Foundation with its index at the end. Moving it must
/// leave every frame decoding exactly as before.
#[test]
fn faststart_moves_the_index_without_changing_a_frame() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg");
        return;
    }
    use tape_core::mp4::faststart::faststart;
    let path = tmp("faststart.mp4");
    // Long enough for several chunks per track, with an extra box in front like Media Foundation's.
    ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=60", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
        "-t", "6", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac",
        "-metadata", "title=tape", path.to_str().unwrap(),
    ]);
    let before = top_level_boxes(&path);
    assert!(before.iter().position(|b| b == "moov") > before.iter().position(|b| b == "mdat"), "{before:?}");
    let hashes = frame_hashes(&path);

    assert!(faststart(&path).unwrap());
    let after = top_level_boxes(&path);
    assert!(after.iter().position(|b| b == "moov") < after.iter().position(|b| b == "mdat"), "{after:?}");
    decode_clean(&path);
    assert_eq!(frame_hashes(&path), hashes);

    // Already in order: left alone.
    assert!(!faststart(&path).unwrap());
    assert_eq!(frame_hashes(&path), hashes);
}

/// Hashes of one stream's decoded frames, in order.
fn stream_hashes(path: &Path, stream: usize) -> Vec<String> {
    frame_hashes(path)
        .lines()
        .filter(|l| l.split(',').next().map(str::trim) == Some(&stream.to_string()))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect()
}

fn first_pts(path: &Path, stream: &str) -> f64 {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", stream, "-read_intervals", "%+#1", "-show_entries", "frame=pts_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().trim_end_matches(',').parse().unwrap()
}

fn duration_of(path: &Path) -> f64 {
    probe(path)["format"]["duration"].as_str().unwrap().parse().unwrap()
}

/// Trims each of tape's own layouts and checks every kept frame is bit-identical to the original's.
fn check_trim(codec: Codec, name: &str) {
    if !have_ffmpeg() {
        return;
    }
    use tape_core::mp4::trim::{info, trim};
    let (tracks, packets) = stream(codec, 2, 400);
    for (layout, path) in [("clip", tmp(&format!("{name}-trim-src-clip.mp4"))), ("recording", tmp(&format!("{name}-trim-src-rec.mp4")))] {
        if layout == "clip" {
            clip(&path, &tracks, &packets);
        } else {
            record(&path, &tracks, &packets, true);
        }
        let i = info(&path).unwrap();
        assert_eq!(i.keyframes, [0.0, 1.0, 2.0, 3.0, 4.0], "{layout}");
        assert!((i.duration - 5.0).abs() < 0.02, "{layout} duration {}", i.duration);

        let out = tmp(&format!("{name}-trimmed-{layout}.mp4"));
        // Starts on the keyframe at 1 s, ends before the frame at 3.7 s.
        let (begin, end) = trim(&path, &out, 1.3, 3.7).unwrap();
        assert_eq!(begin, 1.0);
        assert!((end - 3.7).abs() < 0.02, "end {end}");
        assert_eq!(top_level_boxes(&out), ["ftyp", "moov", "mdat"]);
        decode_clean(&out);
        let info_out = probe(&out);
        assert_eq!(frames_of(&info_out, "video"), [162], "{layout}");
        assert_eq!(frames_of(&info_out, "audio").len(), 2, "{layout}");
        assert!((duration_of(&out) - 2.7).abs() < 0.03, "{layout} duration {}", duration_of(&out));
        assert_eq!(stream_hashes(&out, 0), stream_hashes(&path, 0)[60..222], "{layout} frames differ");
        let audio = first_pts(&out, "a:0");
        assert!(audio.abs() < 0.03, "{layout} audio starts at {audio}");
        assert!(first_pts(&out, "v:0").abs() < 0.001);
    }
}

#[test]
fn trimming_h264_keeps_frames_exact() {
    check_trim(Codec::H264, "h264");
}

#[test]
fn trimming_hevc_keeps_frames_exact() {
    check_trim(Codec::Hevc, "hevc");
}

#[test]
fn trimming_av1_keeps_frames_exact() {
    check_trim(Codec::Av1, "av1");
}

/// Audio that started half a second after the video stays half a second in when the cut keeps it,
/// and lines up exactly with the video when the cut starts after it.
#[test]
fn trimming_keeps_late_audio_in_sync() {
    if !have_ffmpeg() {
        return;
    }
    use tape_core::mp4::trim::trim;
    let (tracks, packets) = stream(Codec::H264, 1, 24_000);
    let path = tmp("trim-late.mp4");
    clip(&path, &tracks, &packets);
    let out = tmp("trim-late-0.mp4");
    trim(&path, &out, 0.0, 3.0).unwrap();
    let at = first_pts(&out, "a:0");
    assert!((at - 0.5).abs() < 0.03, "audio starts at {at}");
    let out = tmp("trim-late-2.mp4");
    trim(&path, &out, 2.0, 4.0).unwrap();
    // The audio frame playing at 2 s started up to 21 ms before it, and the edit list skips that.
    let at = first_pts(&out, "a:0");
    assert!(at.abs() < 0.025, "audio starts at {at}");
    decode_clean(&out);
}

/// Files from other muxers, like the NVIDIA app's: B-frames, an edit list and AAC priming.
#[test]
fn trimming_a_file_with_b_frames() {
    if !have_ffmpeg() {
        return;
    }
    use tape_core::mp4::trim::{info, trim};
    let path = tmp("trim-bframes.mp4");
    ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=60", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
        "-t", "6", "-c:v", "libx264", "-bf", "3", "-g", "120", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-c:a", "aac",
        path.to_str().unwrap(),
    ]);
    let i = info(&path).unwrap();
    assert_eq!(i.keyframes, [0.0, 2.0, 4.0]);
    let out = tmp("trim-bframes-out.mp4");
    let (begin, _) = trim(&path, &out, 2.5, 5.0).unwrap();
    assert_eq!(begin, 2.0);
    decode_clean(&out);
    assert_eq!(stream_hashes(&out, 0), stream_hashes(&path, 0)[120..300]);
    assert!(first_pts(&out, "v:0").abs() < 0.001, "video starts at {}", first_pts(&out, "v:0"));
    assert!(first_pts(&out, "a:0").abs() < 0.03, "audio starts at {}", first_pts(&out, "a:0"));
}

#[test]
fn an_unfinished_recording_is_refused() {
    if !have_ffmpeg() {
        return;
    }
    let (tracks, packets) = stream(Codec::H264, 1, 0);
    let path = tmp("trim-crash.mp4");
    record(&path, &tracks, &packets, false);
    let out = tmp("trim-crash-out.mp4");
    assert!(tape_core::mp4::trim::trim(&path, &out, 0.0, 2.0).is_err());
    assert!(!out.exists());
    // A cut that snaps back to a keyframe (at 1 s) is long enough even if the range asked for wasn't.
    let finished = tmp("trim-short.mp4");
    clip(&finished, &tracks, &packets);
    assert!(tape_core::mp4::trim::trim(&finished, &out, 1.2, 1.4).is_err());
    assert!(tape_core::mp4::trim::trim(&finished, &out, 1.8, 1.9).is_ok());
    // Ends before it begins.
    assert!(tape_core::mp4::trim::trim(&finished, &out, 3.0, 2.0).is_err());
}
