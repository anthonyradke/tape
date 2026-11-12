//! Runs the real engine process on the fake backend, talks to it over the real protocol, and has
//! ffprobe read what it writes. Covers what a user would notice: clips, recordings, keeping
//! going through failures. Skips if ffmpeg isn't installed.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tape_core::mp4::codec::nal_units;

const GOP: usize = 60;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Streams shared by every test, built once.
fn fixtures() -> &'static (PathBuf, PathBuf) {
    static FIXTURES: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    FIXTURES.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("tape-e2e-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (h264, aac) = (dir.join("v.h264"), dir.join("a.aac"));
        ffmpeg(&["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=60", "-frames:v", "600", "-pix_fmt", "yuv420p", "-g", "60", "-keyint_min", "60", "-sc_threshold", "0", "-bf", "0", "-c:v", "libx264", "-preset", "ultrafast", "-bsf:v", "h264_metadata=aud=insert", "-f", "h264", h264.to_str().unwrap()]);
        ffmpeg(&["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "4", "-ac", "2", "-c:a", "aac", "-f", "adts", aac.to_str().unwrap()]);

        let mut video = Vec::new();
        let data = fs::read(&h264).unwrap();
        let mut frames: Vec<Vec<u8>> = Vec::new();
        for nal in nal_units(&data) {
            if nal[0] & 0x1f == 9 || frames.is_empty() {
                frames.push(Vec::new());
            }
            let f = frames.last_mut().unwrap();
            f.extend_from_slice(&[0, 0, 0, 1]);
            f.extend_from_slice(nal);
        }
        for f in frames {
            video.extend_from_slice(&(f.len() as u32).to_le_bytes());
            video.extend_from_slice(&f);
        }
        let video_path = dir.join("video.frames");
        fs::write(&video_path, video).unwrap();

        let data = fs::read(&aac).unwrap();
        let mut audio = Vec::new();
        let mut i = 0;
        while i + 7 <= data.len() {
            let len = ((data[i + 3] as usize & 3) << 11) | ((data[i + 4] as usize) << 3) | (data[i + 5] as usize >> 5);
            audio.extend_from_slice(&((len - 7) as u32).to_le_bytes());
            audio.extend_from_slice(&data[i + 7..i + len]);
            i += len;
        }
        let audio_path = dir.join("audio.frames");
        fs::write(&audio_path, audio).unwrap();
        (video_path, audio_path)
    })
}

struct Engine {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<Value>,
    folder: PathBuf,
}

impl Engine {
    fn start(name: &str, env: &[(&str, &str)]) -> Engine {
        let (video, audio) = fixtures();
        let folder = std::env::temp_dir().join(format!("tape-e2e-out-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_tape-engine-fake"))
            .env("TAPE_FAKE", "1")
            .env("TAPE_FAKE_VIDEO", video)
            .env("TAPE_FAKE_AUDIO", audio)
            .env("TAPE_FAKE_GOP", GOP.to_string())
            .env("TAPE_FAKE_FOLDER", &folder)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(serde_json::from_str::<Value>(&line).unwrap()).is_err() {
                    break;
                }
            }
        });
        Engine { child, stdin: Some(stdin), events, folder }
    }

    fn send(&mut self, command: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{command}").unwrap();
        stdin.flush().unwrap();
    }

    fn configure(&mut self, settings: Value) {
        self.send(json!({"cmd": "configure", "settings": settings}));
    }

    /// The next event `accept` likes, within `seconds`.
    fn wait(&self, seconds: u64, what: &str, accept: impl Fn(&Value) -> bool) -> Value {
        let until = Instant::now() + Duration::from_secs(seconds);
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(event) if accept(&event) => return event,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        panic!("timed out waiting for {what}");
    }

    fn wait_status(&self, seconds: u64, what: &str, accept: impl Fn(&Value) -> bool) -> Value {
        self.wait(seconds, what, |e| e["evt"] == "status" && accept(&e["status"]))["status"].clone()
    }

    fn mp4s(&self) -> Vec<PathBuf> {
        // Files sit in a folder per game.
        let games = fs::read_dir(&self.folder).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir());
        let mut files: Vec<_> = games.flat_map(|dir| fs::read_dir(dir).unwrap().flatten().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "mp4")).collect();
        files.sort();
        files
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn probe(path: &Path) -> Value {
    let out = Command::new("ffprobe").args(["-v", "error", "-count_packets", "-show_streams", "-show_format", "-of", "json"]).arg(path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn seconds(info: &Value) -> f64 {
    info["format"]["duration"].as_str().unwrap().parse().unwrap()
}

fn decode_clean(path: &Path) {
    let out = Command::new("ffmpeg").args(["-v", "error", "-xerror", "-i"]).arg(path).args(["-map", "0", "-f", "null", "-"]).output().unwrap();
    assert!(out.status.success() && out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn saved(event: &Value) -> PathBuf {
    PathBuf::from(event["path"].as_str().unwrap())
}

#[test]
fn replay_clip_has_the_requested_length_and_is_named_for_the_game() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("clip", &[]);
    e.configure(json!({"replaySeconds": 5, "microphone": ""}));
    let s = e.wait_status(15, "6 s buffered", |s| s["replaySeconds"].as_f64().unwrap() >= 5.0);
    assert_eq!(s["video"]["running"], true);
    assert_eq!(s["replayOn"], true);
    e.send(json!({"cmd": "saveClip"}));
    let event = e.wait(10, "clip", |ev| ev["evt"] == "saved" || ev["evt"] == "error");
    assert_eq!(event["evt"], "saved", "{event}");
    assert_eq!(event["kind"], "clip");
    let path = saved(&event);
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("Test Game 20") && name.ends_with(".DVR.mp4"), "{name}");
    assert_eq!(path.parent().unwrap().file_name().unwrap(), "Test Game");
    let info = probe(&path);
    let d = seconds(&info);
    assert!((5.0..=6.5).contains(&d), "clip is {d} s");
    assert_eq!(info["streams"].as_array().unwrap().len(), 2);
    decode_clean(&path);
}

#[test]
fn a_short_clip_keeps_only_its_length_and_the_next_clip_follows_it() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("short", &[]);
    e.configure(json!({"replaySeconds": 10, "microphone": ""}));
    e.wait_status(20, "8 s buffered", |s| s["replaySeconds"].as_f64().unwrap() >= 8.0);
    e.send(json!({"cmd": "saveClip", "seconds": 3, "request": 7}));
    let event = e.wait(10, "short clip", |ev| ev["evt"] == "saved" || ev["evt"] == "error");
    assert_eq!(event["evt"], "saved", "{event}");
    assert_eq!(event["request"], 7, "the host matches its own saves by this");
    let short = seconds(&probe(&saved(&event)));
    // Clips start on a keyframe, one a second here, so up to a second more than asked.
    assert!((3.0..=4.2).contains(&short), "short clip is {short} s");
    decode_clean(&saved(&event));
    // More than the replay length is the replay length.
    e.wait_status(10, "2 s more", |s| s["clipSeconds"].as_f64().unwrap() >= 2.0);
    e.send(json!({"cmd": "saveClip", "seconds": 600}));
    let event = e.wait(10, "full clip", |ev| ev["evt"] == "saved" || ev["evt"] == "error");
    assert_eq!(event["evt"], "saved", "{event}");
    let next = seconds(&probe(&saved(&event)));
    assert!(next < 4.0, "the full clip must start where the short one ended, not repeat it: {next} s");
    e.send(json!({"cmd": "saveClip", "request": 8}));
    let event = e.wait(10, "refused clip", |ev| ev["evt"] == "saved" || ev["evt"] == "error");
    assert_eq!((&event["evt"], &event["request"], &event["clip"]), (&json!("error"), &json!(8), &json!(true)), "{event}");
}

#[test]
fn a_second_clip_starts_where_the_first_ended() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("follow-up", &[]);
    e.configure(json!({"replaySeconds": 8, "microphone": ""}));
    e.wait_status(15, "buffered", |s| s["replaySeconds"].as_f64().unwrap() >= 8.0);
    e.send(json!({"cmd": "saveClip"}));
    let first = e.wait(10, "first clip", |ev| ev["evt"] == "saved" || ev["evt"] == "error");
    assert_eq!(first["evt"], "saved", "{first}");

    // Straight after, there's nothing new to save yet.
    e.send(json!({"cmd": "saveClip"}));
    let early = e.wait(10, "answer", |ev| ev["evt"] == "saved" || ev["evt"] == "error");
    assert_eq!(early["message"], "Nothing new since the last clip", "{early}");

    let s = e.wait_status(15, "new footage", |s| s["clipSeconds"].as_f64().unwrap() >= 3.0);
    assert_eq!(s["replaySeconds"].as_f64().unwrap(), 8.0, "the buffer stays full");
    e.send(json!({"cmd": "saveClip"}));
    let second = e.wait(10, "second clip", |ev| ev["evt"] == "saved" || ev["evt"] == "error");
    assert_eq!(second["evt"], "saved", "{second}");
    let path = saved(&second);
    let d = seconds(&probe(&path));
    assert!((3.0..=5.0).contains(&d), "second clip is {d} s");
    decode_clean(&path);
}

#[test]
fn recording_works_with_replay_off_and_is_a_normal_mp4() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("record", &[]);
    e.configure(json!({"replay": false}));
    let s = e.wait_status(5, "idle", |_| true);
    assert_eq!(s["video"]["running"], false, "nothing should run with replay off");
    e.send(json!({"cmd": "startRecording"}));
    let s = e.wait_status(10, "3 s recorded", |s| s["recordingSeconds"].as_f64().unwrap() >= 3.0);
    assert_eq!(s["recording"], true);
    e.send(json!({"cmd": "stopRecording"}));
    let event = e.wait(10, "recording saved", |ev| ev["evt"] == "saved");
    assert_eq!(event["kind"], "recording");
    let info = probe(&saved(&event));
    assert!(seconds(&info) >= 3.0);
    decode_clean(&saved(&event));
    e.wait_status(5, "pipeline stopped", |s| s["video"]["running"] == false && s["recording"] == false);
}

#[test]
fn encoder_failure_recovers_without_losing_the_recording_or_the_buffer() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("recover", &[("TAPE_FAKE_FAIL_AFTER", "120"), ("TAPE_FAKE_FAILURES", "2")]);
    e.configure(json!({"replaySeconds": 30}));
    e.wait_status(5, "running", |s| s["video"]["running"] == true);
    e.send(json!({"cmd": "startRecording"}));
    let s = e.wait_status(25, "two restarts and time after", |s| s["restarts"].as_u64().unwrap() >= 2 && s["recordingSeconds"].as_f64().unwrap() >= 7.0);
    assert_eq!(s["video"]["running"], true);
    assert_eq!(s["restarts"], 2);
    e.send(json!({"cmd": "saveClip"}));
    let clip = e.wait(10, "clip", |ev| ev["evt"] == "saved");
    e.send(json!({"cmd": "stopRecording"}));
    let recording = e.wait(10, "recording", |ev| ev["evt"] == "saved");
    // Identical stream parameters after the restart, so one file continues.
    assert_eq!(e.mp4s().len(), 2);
    assert!(seconds(&probe(&saved(&recording))) >= 7.0);
    decode_clean(&saved(&recording));
    decode_clean(&saved(&clip));
}

#[test]
fn hung_driver_goes_quiet_so_the_host_can_replace_the_process() {
    let mut e = Engine::start("hang", &[("TAPE_FAKE_HANG_AFTER", "30")]);
    e.configure(json!({}));
    e.wait_status(5, "running", |s| s["video"]["running"] == true);
    std::thread::sleep(Duration::from_secs(10));
    while e.events.try_recv().is_ok() {}
    std::thread::sleep(Duration::from_secs(1));
    assert!(e.events.try_recv().is_err(), "a hung engine must stop reporting");
}

#[test]
fn losing_the_host_finishes_the_recording() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("hostgone", &[]);
    e.configure(json!({}));
    e.send(json!({"cmd": "startRecording"}));
    e.wait_status(10, "recording", |s| s["recordingSeconds"].as_f64().unwrap() >= 2.0);
    // Closing stdin is what the host's death looks like.
    e.stdin = None;
    assert!(wait_exit(&mut e.child, 10), "engine should exit with its host");
    let files = e.mp4s();
    assert_eq!(files.len(), 1);
    assert_eq!(top_level(&files[0]).last().unwrap(), "moov", "finished, not left fragmented");
    decode_clean(&files[0]);
}

fn wait_exit(child: &mut Child, seconds: u64) -> bool {
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn top_level(path: &Path) -> Vec<String> {
    let data = fs::read(path).unwrap();
    let (mut out, mut i) = (Vec::new(), 0usize);
    while i + 8 <= data.len() {
        let mut size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        if size == 1 {
            size = u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap()) as usize;
        }
        out.push(String::from_utf8_lossy(&data[i + 4..i + 8]).into_owned());
        i += size;
    }
    out
}

#[test]
fn crash_mid_recording_leaves_a_playable_file() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("crash", &[("TAPE_FAKE_CRASH_AFTER", "420")]);
    e.configure(json!({}));
    e.send(json!({"cmd": "startRecording"}));
    assert!(wait_exit(&mut e.child, 20), "fake crash should end the process");
    let files = e.mp4s();
    assert_eq!(files.len(), 1);
    let info = probe(&files[0]);
    let packets: u64 = info["streams"][0]["nb_read_packets"].as_str().unwrap().parse().unwrap();
    assert!(packets >= 120, "only {packets} frames survived");
    decode_clean(&files[0]);
}

#[test]
fn encoder_settings_changed_mid_recording_wait_for_it_to_end() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("deferred", &[]);
    e.configure(json!({"fps": 60}));
    e.send(json!({"cmd": "startRecording"}));
    e.wait_status(10, "recording", |s| s["recordingSeconds"].as_f64().unwrap() >= 1.0);
    e.configure(json!({"fps": 30, "replaySeconds": 7}));
    let s = e.wait_status(5, "replay length applied", |s| s["replayTarget"] == 7);
    assert_eq!(s["recording"], true);
    assert_eq!(s["restarts"], 0);
    e.wait_status(10, "still going", |s| s["recordingSeconds"].as_f64().unwrap() >= 3.0);
    e.send(json!({"cmd": "stopRecording"}));
    let event = e.wait(10, "saved", |ev| ev["evt"] == "saved");
    assert_eq!(e.mp4s().len(), 1, "the change must not split the file");
    let rate = probe(&saved(&event))["streams"][0]["r_frame_rate"].clone();
    assert_eq!(rate, "60/1");
    e.wait_status(10, "new rate running", |s| s["video"]["running"] == true && (s["video"]["fps"].as_f64().unwrap() - 30.0).abs() < 5.0);
}

fn chapters(path: &Path) -> Vec<(f64, String)> {
    let out = Command::new("ffprobe").args(["-v", "error", "-show_chapters", "-of", "json"]).arg(path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let info: Value = serde_json::from_slice(&out.stdout).unwrap();
    info["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["start_time"].as_str().unwrap().parse().unwrap(), c["tags"]["title"].as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn markers_during_a_recording_become_its_chapters() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("markers", &[]);
    e.configure(json!({}));
    e.send(json!({"cmd": "addMarker"}));
    let none = e.wait(10, "an answer while not recording", |ev| ev["evt"] == "marked");
    assert!(none["seconds"].is_null(), "{none}");
    e.send(json!({"cmd": "startRecording"}));
    e.wait_status(10, "recording", |s| s["recordingSeconds"].as_f64().unwrap() >= 1.0);
    e.send(json!({"cmd": "addMarker"}));
    let first = e.wait(5, "first marker", |ev| ev["evt"] == "marked")["seconds"].as_f64().unwrap();
    e.wait_status(10, "later", |s| s["recordingSeconds"].as_f64().unwrap() >= first + 1.0);
    e.send(json!({"cmd": "addMarker"}));
    let second = e.wait(5, "second marker", |ev| ev["evt"] == "marked")["seconds"].as_f64().unwrap();
    assert!(first >= 0.9 && second > first + 0.9, "{first} {second}");
    e.send(json!({"cmd": "stopRecording"}));
    let event = e.wait(10, "saved", |ev| ev["evt"] == "saved");
    let read = chapters(&saved(&event));
    assert_eq!(read.len(), 2, "{read:?}");
    for ((at, title), (wanted, name)) in read.iter().zip([(first, "Marker 1"), (second, "Marker 2")]) {
        assert!((at - wanted).abs() < 0.001, "{at} vs {wanted}");
        assert_eq!(title, name);
    }
    decode_clean(&saved(&event));
}

#[test]
fn a_games_settings_apply_without_emptying_the_buffer() {
    if !have_ffmpeg() {
        return;
    }
    let log = std::env::temp_dir().join(format!("tape-e2e-pergame-{}.log", std::process::id()));
    let _ = fs::remove_file(&log);
    let mut e = Engine::start("pergame", &[("TAPE_LOG", log.to_str().unwrap())]);
    e.configure(json!({"replaySeconds": 30, "bitrateMbps": 50}));
    e.wait_status(15, "4 s buffered", |s| s["replaySeconds"].as_f64().unwrap() >= 4.0);
    // The fake's window is always "Test Game". Another game's settings change nothing.
    e.configure(json!({"replaySeconds": 30, "bitrateMbps": 50, "perGame": {"Other": {"replaySeconds": 5}, "test game": {"replaySeconds": 60, "bitrateMbps": 20}}}));
    let s = e.wait_status(5, "the game's length", |s| s["replayTarget"] == 60);
    assert!(s["replaySeconds"].as_f64().unwrap() >= 4.0, "the buffer must survive: {s}");
    assert_eq!(s["video"]["running"], true);
    // A recording holds the bitrate back until it ends, as with any encoder change.
    e.send(json!({"cmd": "startRecording"}));
    e.wait_status(10, "recording", |s| s["recordingSeconds"].as_f64().unwrap() >= 1.0);
    e.configure(json!({"replaySeconds": 30, "bitrateMbps": 50, "perGame": {"Test Game": {"bitrateMbps": 30}}}));
    let s = e.wait_status(5, "back to the global length", |s| s["replayTarget"] == 30);
    assert_eq!(s["recording"], true);
    e.wait_status(10, "more recorded", |s| s["recordingSeconds"].as_f64().unwrap() >= 2.0);
    let text = fs::read_to_string(&log).unwrap();
    assert!(!text.contains("30 Mbps, same session"), "applied mid-recording:\n{text}");
    e.send(json!({"cmd": "stopRecording"}));
    e.wait(10, "saved", |ev| ev["evt"] == "saved");
    let s = e.wait_status(5, "after", |s| s["recording"] == false);
    assert!(s["replaySeconds"].as_f64().unwrap() >= 4.0, "{s}");
    std::thread::sleep(Duration::from_millis(500));
    let text = fs::read_to_string(&log).unwrap();
    assert_eq!(text.matches("fake pipeline started").count(), 1, "the pipeline must not restart:\n{text}");
    let changes: Vec<&str> = text.lines().filter(|l| l.contains("same session")).collect();
    assert_eq!(changes.len(), 2, "{text}");
    assert!(changes[0].contains("20 Mbps") && changes[1].contains("30 Mbps"), "{changes:?}");
}

#[test]
fn toggling_replay_mid_recording_keeps_a_deferred_change() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("deferred-replay", &[]);
    e.configure(json!({"fps": 60}));
    e.send(json!({"cmd": "startRecording"}));
    e.wait_status(10, "recording", |s| s["recordingSeconds"].as_f64().unwrap() >= 1.0);
    e.configure(json!({"fps": 30}));
    e.send(json!({"cmd": "setReplay", "on": false}));
    e.wait_status(5, "replay off", |s| s["replayOn"] == false);
    e.send(json!({"cmd": "setReplay", "on": true}));
    e.wait_status(10, "replay on", |s| s["replayOn"] == true && s["recordingSeconds"].as_f64().unwrap() >= 3.0);
    e.send(json!({"cmd": "stopRecording"}));
    let event = e.wait(10, "saved", |ev| ev["evt"] == "saved");
    assert_eq!(probe(&saved(&event))["streams"][0]["r_frame_rate"], "60/1");
    e.wait_status(10, "new rate running", |s| s["video"]["running"] == true && (s["video"]["fps"].as_f64().unwrap() - 30.0).abs() < 5.0);
}

#[test]
fn audio_device_loss_does_not_touch_video() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("audio", &[("TAPE_FAKE_AUDIO_FAILS", "1")]);
    e.configure(json!({"replaySeconds": 5}));
    let s = e.wait_status(5, "device reported lost", |s| s["system"]["problem"].is_string());
    assert_eq!(s["system"]["running"], false);
    assert_eq!(s["restarts"], 0);
    let s = e.wait_status(6, "device back", |s| s["system"]["running"] == true);
    assert_eq!(s["system"]["problem"], Value::Null);
    assert_eq!(s["video"]["running"], true);
}

#[test]
fn a_paused_capture_is_reported_as_transient_and_keeps_the_buffer_going() {
    // Late enough that a status report always comes before the pause, which starts with the
    // first report otherwise and makes the first check race it.
    let mut e = Engine::start("paused", &[("TAPE_FAKE_PAUSE_AFTER", "120")]);
    e.configure(json!({"replaySeconds": 5}));
    let s = e.wait_status(5, "running", |s| s["video"]["running"] == true && s["video"]["problem"].is_null());
    assert_eq!(s["video"]["transient"], false);
    let s = e.wait_status(5, "paused", |s| s["video"]["problem"].is_string());
    assert_eq!(s["video"]["transient"], true);
    assert_eq!(s["video"]["running"], true);
    assert_eq!(s["restarts"], 0);
}

#[test]
fn saving_with_replay_off_says_so() {
    let mut e = Engine::start("off", &[]);
    e.configure(json!({"replay": false}));
    e.send(json!({"cmd": "saveClip"}));
    let event = e.wait(5, "error", |ev| ev["evt"] == "error");
    assert_eq!(event["clip"], true);
    assert_eq!(event["message"], "Instant replay is off");
}

#[test]
fn both_audio_sources_off_is_a_normal_setup() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("silent", &[]);
    e.configure(json!({"replaySeconds": 5, "systemAudio": "", "microphone": ""}));
    e.wait_status(12, "buffered", |s| s["replaySeconds"].as_f64().unwrap() >= 4.0);
    let s = e.wait_status(5, "status", |_| true);
    assert_eq!(s["restarts"], 0, "the audio thread must not count as failing");
    e.send(json!({"cmd": "saveClip"}));
    let event = e.wait(10, "clip", |ev| ev["evt"] == "saved");
    assert_eq!(probe(&saved(&event))["streams"].as_array().unwrap().len(), 1);
    decode_clean(&saved(&event));
}

#[test]
fn missing_aac_encoder_is_reported_and_video_still_saves() {
    if !have_ffmpeg() {
        return;
    }
    let mut e = Engine::start("noaac", &[("TAPE_FAKE_NO_AAC", "1")]);
    e.configure(json!({"replaySeconds": 5}));
    let s = e.wait_status(6, "problem shown", |s| s["system"]["problem"].is_string());
    assert_eq!(s["system"]["problem"], "AAC encoder unavailable");
    assert_eq!(s["system"]["running"], false);
    e.wait_status(12, "buffered", |s| s["replaySeconds"].as_f64().unwrap() >= 3.0);
    e.send(json!({"cmd": "saveClip"}));
    let event = e.wait(10, "clip", |ev| ev["evt"] == "saved");
    decode_clean(&saved(&event));
}

#[test]
fn quality_counters_report_dropped_frames_and_audio_glitches() {
    if !have_ffmpeg() {
        return;
    }
    let count = |s: &Value, window: &str, key: &str| s["quality"][window][key].as_u64().unwrap();
    let mut clean = Engine::start("quality-clean", &[]);
    clean.configure(json!({"replaySeconds": 5}));
    let s = clean.wait_status(10, "running a while", |s| s["quality"]["seconds"].as_f64().unwrap() >= 2.0);
    assert_eq!(s["quality"]["recent"], json!({"droppedFrames": 0, "lateFrames": 0, "systemGlitches": 0, "micGlitches": 0, "slowDisk": 0}));

    let mut e = Engine::start("quality", &[("TAPE_FAKE_DROP_EVERY", "30"), ("TAPE_FAKE_GLITCH_EVERY", "20")]);
    e.configure(json!({"replaySeconds": 5}));
    let s = e.wait_status(10, "drops and glitches counted", |s| {
        s["quality"].is_object() && count(s, "recent", "droppedFrames") >= 3 && count(s, "recent", "systemGlitches") >= 1
    });
    assert_eq!(s["quality"]["total"], s["quality"]["recent"], "everything happened within the recent window");
    assert_eq!(count(&s, "total", "micGlitches"), 0, "the mic is off");
    assert_eq!(count(&s, "total", "slowDisk"), 0);
    // A frame in 30 skipped at 60 fps leaves about 58 delivered.
    let fps = s["video"]["fps"].as_f64().unwrap();
    assert!((50.0..60.0).contains(&fps), "{fps}");
}

#[test]
fn tapes_own_sounds_are_left_out_of_default_game_audio_where_windows_allows() {
    if !have_ffmpeg() {
        return;
    }
    let excludes = |s: &Value| s["system"]["excludesTape"].as_bool().unwrap();
    let mut e = Engine::start("exclude", &[]);
    e.configure(json!({"replaySeconds": 5}));
    let s = e.wait_status(5, "game audio running", |s| s["system"]["running"] == true);
    assert!(excludes(&s), "the default device leaves tape out");
    // Windows can only leave a process out of the default device's mix.
    e.configure(json!({"replaySeconds": 5, "systemAudio": "out"}));
    let s = e.wait_status(5, "chosen device running", |s| s["system"]["running"] == true && !excludes(s));
    assert_eq!(s["system"]["problem"], Value::Null);
    e.configure(json!({"replaySeconds": 5, "excludeTapeAudio": false}));
    e.wait_status(5, "whole output", |s| s["system"]["running"] == true && !excludes(s));

    // Too old a Windows: the whole output is recorded instead, with nothing reported as wrong.
    let mut old = Engine::start("exclude-old", &[("TAPE_FAKE_NO_EXCLUSION", "1")]);
    old.configure(json!({"replaySeconds": 5}));
    let s = old.wait_status(5, "game audio running", |s| s["system"]["running"] == true);
    assert!(!excludes(&s));
    assert_eq!(s["system"]["problem"], Value::Null);
}

#[test]
fn the_probe_reports_ram_and_the_replay_limit() {
    let mut e = Engine::start("probe-ram", &[]);
    e.send(json!({"cmd": "probe"}));
    let probe = e.wait(5, "probe", |ev| ev["evt"] == "probe")["probe"].clone();
    let ram = probe["ramBytes"].as_u64().unwrap();
    let limit = probe["replayLimitBytes"].as_u64().unwrap();
    assert!(ram > 0, "{probe}");
    assert!(limit > ram / 3 && limit < ram / 2, "40% of {ram}: {limit}");
}
