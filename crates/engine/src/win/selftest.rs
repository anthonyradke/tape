//! Runs each part of the pipeline on this machine and reports what happened in plain text. This
//! is what to look at first when something doesn't work on real hardware.

use std::fmt::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tape_core::audio::{peak, to_i16};
use tape_core::mp4::write_clip;
use tape_core::packet::{Codec, AAC_FRAME};
use tape_core::settings::{CaptureMethod, Settings, FOLLOW_DEFAULT};
use tape_core::sink::Sink;
use windows::core::{s, w, PCWSTR};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT, SND_PURGE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

use super::capture::{Capture, CaptureError, Duplication, GraphicsCapture};
use super::convert::Converter;
use super::cursor::Overlay;
use super::gfx::{self, Gfx, OutputInfo};
use super::{codec_name, encoder, wasapi, Windows};
use crate::backend::{AudioBlock, AudioContext, AudioInput, AudioShared, Backend, Clock, Source, VideoContext, VideoStats};
use crate::mix;

const CAPTURE_SECONDS: u64 = 2;
const ENCODE_SECONDS: u64 = 4;

macro_rules! line {
    ($out:expr, $($arg:tt)*) => { let _ = writeln!($out, $($arg)*); };
}

fn os_version() -> String {
    unsafe {
        let Ok(ntdll) = LoadLibraryW(w!("ntdll.dll")) else { return "unknown".into() };
        let Some(raw) = GetProcAddress(ntdll, s!("RtlGetVersion")) else { return "unknown".into() };
        let get: unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32 = std::mem::transmute(raw);
        let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
        get(&mut info);
        format!("{}.{}.{}", info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
    }
}

/// Reads a frame back and summarizes its brightness, which tells a real image from a black one,
/// the classic sign of capture silently not working. Takes NV12 (luma plane) or BGRA (green).
fn image_stats(gfx: &Gfx, texture: &ID3D11Texture2D) -> Result<String, String> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let (width, height) = (desc.Width as usize, desc.Height as usize);
    let (step, offset, floor) = if desc.Format == DXGI_FORMAT_NV12 { (1, 0, 17) } else { (4, 1, 1) };
    let staging = gfx.create_texture(&D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..desc
    })?;
    unsafe {
        gfx.context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gfx.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(|e| e.to_string())?;
        let (mut low, mut high, mut sum, mut count) = (255u8, 0u8, 0u64, 0u64);
        for y in (0..height).step_by(4) {
            let row = std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize), width * step);
            for pixel in row.chunks_exact(step).step_by(4) {
                let v = pixel[offset];
                low = low.min(v);
                high = high.max(v);
                sum += v as u64;
                count += 1;
            }
        }
        gfx.context.Unmap(&staging, 0);
        let mean = sum / count.max(1);
        let verdict = if high <= floor { "ALL BLACK" } else if low == high { "flat color" } else { "has picture detail" };
        Ok(format!("{low}..{high}, mean {mean}: {verdict}"))
    }
}

fn check_capture(out: &mut String, output: &OutputInfo, graphics: bool) {
    let label = if graphics { "Windows Graphics Capture" } else { "Desktop Duplication" };
    let result = (|| -> Result<String, String> {
        let gfx = Gfx::new(&output.adapter)?;
        let opened: Result<Box<dyn Capture>, CaptureError> = if graphics {
            GraphicsCapture::open(&gfx, output, true).map(|c| Box::new(c) as _)
        } else {
            Duplication::open(&gfx, output, true).map(|c| Box::new(c) as _)
        };
        let mut capture = opened.map_err(|e| e.message().to_string())?;
        let started = Instant::now();
        let mut frames = 0;
        while started.elapsed() < Duration::from_secs(CAPTURE_SECONDS) {
            if capture.next(20).map_err(|e| e.message().to_string())? {
                frames += 1;
            }
        }
        let Some(frame) = capture.frame() else {
            return Ok(format!("{frames} updates, no image (a static desktop may not produce one)"));
        };
        let raw = image_stats(&gfx, &frame)?;
        let (width, height) = (output.width & !1, output.height & !1);
        let converted = match Converter::new(&gfx, width, height) {
            Ok(mut converter) => {
                converter.convert(&frame, capture.turn())?;
                format!("converted {}", image_stats(&gfx, converter.target())?)
            }
            Err(e) => format!("not converted, {e}"),
        };
        Ok(format!("{frames} updates in {CAPTURE_SECONDS} s, raw {raw}, {converted}"))
    })();
    match result {
        Ok(text) => {
            line!(out, "  {label}: OK, {text}");
        }
        Err(e) => {
            line!(out, "  {label}: FAILED, {e}");
        }
    }
}

fn check_overlay(out: &mut String, output: &OutputInfo) {
    let result = Gfx::new(&output.adapter).and_then(|gfx| Overlay::new(&gfx));
    match result {
        Ok(_) => {
            line!(out, "  Pointer overlay: OK, shaders compiled");
        }
        Err(e) => {
            line!(out, "  Pointer overlay: FAILED, {e}");
        }
    }
}

/// Where this run's clips go. The process id keeps two runs at once from writing, or sharing, each
/// other's files.
fn clip_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tape-selftest-{}-{name}.mp4", std::process::id()))
}

/// Clips left by earlier runs, once they're a day old: recent ones may still be looked at.
fn remove_old_clips() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("tape-selftest-") && name.ends_with(".mp4")) {
            continue;
        }
        let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(86_400));
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `label` names the line in the report and the file it leaves behind, which is returned when
/// the check wrote one, for the sharing check.
fn check_encode(out: &mut String, settings: &Settings, codec: Codec, label: &str) -> Option<(String, std::path::PathBuf)> {
    let file = clip_path(label);
    let _ = std::fs::remove_file(&file);
    let settings = Settings { codec, replay: true, replay_seconds: 30, ..settings.clone() };
    let sink = Sink::new(true, 30);
    sink.set_audio(mix::tracks(&settings));
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(VideoStats::default());
    let clock = Clock::new();
    let ctx = VideoContext { sink: sink.clone(), clock, stop: stop.clone(), stats: stats.clone(), bitrate_mbps: Default::default() };
    let thread = {
        let settings = settings.clone();
        std::thread::spawn(move || super::video::run(&settings, &ctx))
    };
    let shared = Arc::new(AudioShared {
        system_volume: AtomicU32::new(100),
        mic_volume: AtomicU32::new(100),
        mic_delay_ms: AtomicU32::new(0),
        system: Mutex::new(None),
        mic: Mutex::new(None),
        ..Default::default()
    });
    let audio_thread = {
        let settings = settings.clone();
        let ctx = AudioContext { sink: sink.clone(), clock, stop: stop.clone(), shared };
        std::thread::spawn(move || mix::run(&Windows, &settings, &ctx))
    };
    std::thread::sleep(Duration::from_secs(ENCODE_SECONDS));
    stop.store(true, Ordering::Relaxed);
    let result = thread.join().unwrap_or_else(|_| Err("panicked".into()));
    let audio = audio_thread.join().unwrap_or_else(|_| Err("panicked".into()));
    let name = label;
    if let Err(e) = result {
        line!(out, "  {name}: FAILED, {e}");
        return None;
    }
    if let Err(e) = audio {
        line!(out, "  {name}: audio FAILED, {e}");
    }
    let (encoded, dropped) = (stats.encoded.load(Ordering::Relaxed), stats.dropped.load(Ordering::Relaxed));
    let ms = stats.encode_us.load(Ordering::Relaxed) as f32 / 1000.0;
    let method = stats.method.lock().unwrap().clone();
    let clip = sink.clip(30);
    let written = clip.as_ref().map(|c| {
        std::fs::File::create(&file).and_then(|f| write_clip(std::io::BufWriter::new(f), &c.tracks, &c.packets))
    });
    match written {
        Some(Ok(bytes)) => {
            let (width, height) = (stats.width.load(Ordering::Relaxed), stats.height.load(Ordering::Relaxed));
            line!(out, "  {name}: OK, {width}x{height}, {encoded} frames, {dropped} dropped, last frame {ms:.1} ms, {method}, wrote {bytes} bytes to {}", file.display());
            Some((name.to_string(), file))
        }
        Some(Err(e)) => {
            line!(out, "  {name}: FAILED writing the file, {e}");
            let _ = std::fs::remove_file(&file);
            None
        }
        None => {
            line!(out, "  {name}: FAILED, encoder produced nothing usable ({encoded} frames)");
            None
        }
    }
}

/// One second of a quiet tone as a WAV file. Loopback delivers nothing while nothing is playing,
/// so this is what shows that game audio really gets captured.
fn test_tone() -> Vec<u8> {
    const RATE: u32 = 48_000;
    let data = RATE * 2;
    let mut wav = Vec::with_capacity(44 + data as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    for n in 0..RATE {
        let t = n as f32 / RATE as f32;
        let value = (t * 440.0 * std::f32::consts::TAU).sin() * 0.1 * 32767.0;
        wav.extend_from_slice(&(value as i16).to_le_bytes());
    }
    wav
}

fn check_audio(out: &mut String, label: &str, source: Source) {
    let clock = Clock::new();
    match wasapi::Input::open(source, FOLLOW_DEFAULT, clock) {
        Err(e) => {
            line!(out, "  {label}: not available, {e}");
        }
        Ok(mut input) => {
            let tone = test_tone();
            if source == Source::System {
                let _ = unsafe { PlaySoundW(PCWSTR(tone.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT) };
            }
            let (mut blocks, mut samples, mut loudest) = (0, 0, 0.0f32);
            let until = Instant::now() + Duration::from_secs(1);
            let mut first_gap = None;
            while Instant::now() < until {
                let mut got: Vec<AudioBlock> = Vec::new();
                if let Err(e) = input.read(&mut got) {
                    line!(out, "  {label}: FAILED reading, {e}");
                    return;
                }
                for b in got {
                    blocks += 1;
                    samples += b.stereo.len() / 2;
                    loudest = loudest.max(peak(&b.stereo));
                    first_gap.get_or_insert(clock.now_samples() - b.at);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if source == Source::System {
                unsafe {
                    let _ = PlaySoundW(PCWSTR::null(), None, SND_PURGE);
                }
            }
            let latency = first_gap.map_or("unknown".to_string(), |gap| format!("{gap} samples"));
            line!(out, "  {label}: OK, {}, {blocks} packets, {samples} samples in 1 s, peak {loudest:.3}, latency {latency}", input.name());
        }
    }
}

/// Process loopback, which keeps tape's own sounds out of game audio. Two streams listen while this
/// process plays the tone: one for this process only, which should hear it, and one for everything
/// else, which shouldn't (unless something else is playing at the same time).
fn check_process_loopback(out: &mut String) {
    let clock = Clock::new();
    let pid = std::process::id();
    let opened = wasapi::Input::open_process_loopback(pid, true, clock)
        .and_then(|only| Ok((only, wasapi::Input::open_process_loopback(pid, false, clock)?)));
    let (mut only, mut others) = match opened {
        Ok(both) => both,
        Err(e) => {
            line!(out, "  Process loopback: FAILED, {e} (game audio falls back to the whole output)");
            return;
        }
    };
    let tone = test_tone();
    let _ = unsafe { PlaySoundW(PCWSTR(tone.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT) };
    let (mut heard, mut left_out, mut samples) = (0.0f32, 0.0f32, 0);
    let until = Instant::now() + Duration::from_secs(1);
    let mut failed = None;
    while Instant::now() < until && failed.is_none() {
        for (input, loudest) in [(&mut only, &mut heard), (&mut others, &mut left_out)] {
            let mut got: Vec<AudioBlock> = Vec::new();
            if let Err(e) = input.read(&mut got) {
                failed = Some(e);
            }
            for b in got {
                samples += b.stereo.len() / 2;
                *loudest = loudest.max(peak(&b.stereo));
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    unsafe {
        let _ = PlaySoundW(PCWSTR::null(), None, SND_PURGE);
    }
    match failed {
        Some(e) => {
            line!(out, "  Process loopback: FAILED reading, {e}");
        }
        None => {
            let verdict = if heard < 0.01 { "tone not heard (no output device?)" } else if left_out < heard / 4.0 { "tape's own sound left out" } else { "tone also heard with tape left out, or something else is playing" };
            line!(out, "  Process loopback: OK, {}, {samples} samples, tone peak {heard:.3} from tape alone, {left_out:.3} with tape left out: {verdict}", only.name());
        }
    }
}

/// Encodes a 1 kHz tone and writes it as ADTS, which any decoder can read, so a build machine can
/// check that what the encoder makes is real AAC of the format the MP4 declares.
fn check_aac(out: &mut String, backend: &Windows) {
    let file = std::env::temp_dir().join("tape-selftest.aac");
    let result = (|| -> Result<(usize, usize), String> {
        let mut encoder = backend.aac_encoder()?;
        let (mut frames, mut bytes, mut adts, mut position) = (0, 0, Vec::new(), 0usize);
        for _ in 0..100 {
            let mut block = Vec::with_capacity(AAC_FRAME * 2);
            for _ in 0..AAC_FRAME {
                let t = position as f32 / 48_000.0;
                let v = (t * 1000.0 * std::f32::consts::TAU).sin() * 0.5;
                block.extend_from_slice(&[v, v]);
                position += 1;
            }
            let mut pcm = Vec::new();
            to_i16(&block, &mut pcm);
            for frame in encoder.encode(&pcm)? {
                frames += 1;
                bytes += frame.len();
                let total = frame.len() + 7;
                // AAC LC, 48 kHz, stereo, no CRC.
                adts.extend_from_slice(&[0xff, 0xf1, 0x4c, 0x80 | (total >> 11) as u8, (total >> 3) as u8, ((total & 7) << 5) as u8 | 0x1f, 0xfc]);
                adts.extend_from_slice(&frame);
            }
        }
        std::fs::write(&file, adts).map_err(|e| e.to_string())?;
        Ok((frames, bytes))
    })();
    match result {
        Ok((frames, bytes)) => {
            line!(out, "  AAC: OK, 100 frames in, {frames} out, {bytes} bytes, wrote {}", file.display());
        }
        Err(e) => {
            line!(out, "  AAC: FAILED, {e}");
        }
    }
}

/// Makes the shared-link copy of the clips the encode checks wrote in this run: the 4K one shows
/// scaling, the HEVC and AV1 ones whether Windows can decode those formats.
fn check_share(out: &mut String, clips: &[(String, std::path::PathBuf)]) {
    // 4K first: it shows the scaling.
    let order = |name: &str| ["H.264-4K", "H.264", "HEVC", "AV1"].iter().position(|n| *n == name).unwrap_or(usize::MAX);
    let mut clips: Vec<_> = clips.iter().collect();
    clips.sort_by_key(|(name, _)| order(name));
    let any = !clips.is_empty();
    for (name, input) in clips {
        let output = clip_path(&format!("share-{name}"));
        let began = Instant::now();
        match super::share::share_copy(input, &output, None, &mut |_| {}) {
            Ok(c) => {
                let bytes = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                line!(out, "  from {name}: OK, {}x{}, {:.1} s of video in {:.1} s by {}, {bytes} bytes", c.width, c.height, c.seconds, began.elapsed().as_secs_f32(), c.path);
            }
            Err(e) => {
                line!(out, "  from {name}: FAILED, {e}");
            }
        }
    }
    if !any {
        line!(out, "  skipped, no test clips were encoded");
    }
}

pub fn report(backend: &Windows, settings: &Settings) -> String {
    super::com_init();
    let mut out = String::new();
    line!(out, "tape {} self-test", env!("CARGO_PKG_VERSION"));
    line!(out, "Windows {}", os_version());

    line!(out, "\nDisplays");
    let outputs = match gfx::outputs() {
        Ok(o) => o,
        Err(e) => {
            line!(out, "  FAILED, {e}");
            Vec::new()
        }
    };
    for o in &outputs {
        line!(out, "  {} {}x{} {} Hz{} on {} (driver {})", o.id, o.width, o.height, o.hz, if o.primary { " primary" } else { "" }, o.gpu, o.driver);
    }

    line!(out, "\nCapture");
    let chosen = outputs.iter().find(|o| o.id == settings.monitor).or(outputs.first());
    if let Some(output) = chosen {
        check_capture(&mut out, output, false);
        check_capture(&mut out, output, true);
        check_overlay(&mut out, output);
    }

    line!(out, "\nEncode");
    let mut clips = Vec::new();
    let nvidia = outputs.iter().find(|o| o.is_nvidia());
    match nvidia.map(encoder::supported) {
        None => {
            line!(out, "  skipped, no monitor is on an NVIDIA GPU");
        }
        Some(Err(e)) => {
            line!(out, "  FAILED, {e}");
        }
        Some(Ok(limits)) => {
            let names: Vec<_> = limits.iter().map(|l| format!("{} (up to {}x{})", codec_name(l.codec), l.max_width, l.max_height)).collect();
            line!(out, "  this GPU encodes {}", names.join(", "));
            remove_old_clips();
            let codecs: Vec<_> = limits.iter().map(|l| l.codec).collect();
            let settings = Settings { capture: CaptureMethod::Auto, ..settings.clone() };
            for codec in codecs {
                clips.extend(check_encode(&mut out, &settings, codec, codec_name(codec)));
            }
            // The largest size on offer, whatever this monitor is. Upscaling goes through the same
            // scaler but asks more of it and of the encoder's limits.
            clips.extend(check_encode(&mut out, &Settings { resolution: 2160, ..settings.clone() }, Codec::H264, "H.264-4K"));
        }
    }

    line!(out, "\nSharing");
    check_share(&mut out, &clips);

    line!(out, "\nAudio");
    check_audio(&mut out, "Output loopback", Source::System);
    check_process_loopback(&mut out);
    check_audio(&mut out, "Microphone", Source::Mic);
    check_aac(&mut out, backend);
    let (outs, ins) = (wasapi::devices(Source::System), wasapi::devices(Source::Mic));
    line!(out, "  {} output devices, {} input devices", outs.len(), ins.len());
    out
}
