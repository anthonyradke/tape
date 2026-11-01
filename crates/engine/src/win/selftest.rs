//! Runs each part of the pipeline on this machine and reports what happened in plain text. This
//! is what to look at first when something doesn't work on real hardware.

use std::fmt::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tape_core::audio::{peak, to_i16};
use tape_core::mp4::write_clip;
use tape_core::packet::{Codec, AAC_FRAME};
use tape_core::settings::{CaptureMethod, Settings, FOLLOW_DEFAULT};
use tape_core::sink::Sink;
use windows::core::{s, w};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

use super::capture::{Capture, CaptureError, Duplication, GraphicsCapture};
use super::convert::Converter;
use super::gfx::{self, Gfx, OutputInfo};
use super::{codec_name, encoder, wasapi, Windows};
use crate::backend::{AudioBlock, AudioInput, Backend, Clock, Source, VideoContext, VideoStats};

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

/// Reads the converted frame back and summarizes its brightness, which tells a real image from a
/// black one, the classic sign of capture silently not working.
fn luma_stats(gfx: &Gfx, nv12: &ID3D11Texture2D, width: u32, height: u32) -> Result<String, String> {
    let staging = gfx.create_texture(&D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    })?;
    unsafe {
        gfx.context.CopyResource(&staging, nv12);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gfx.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(|e| e.to_string())?;
        let (mut low, mut high, mut sum, mut count) = (255u8, 0u8, 0u64, 0u64);
        for y in (0..height as usize).step_by(4) {
            let row = std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize), width as usize);
            for &v in row.iter().step_by(4) {
                low = low.min(v);
                high = high.max(v);
                sum += v as u64;
                count += 1;
            }
        }
        gfx.context.Unmap(&staging, 0);
        let mean = sum / count.max(1);
        let verdict = if high <= 17 { "ALL BLACK" } else if low == high { "flat color" } else { "has picture detail" };
        Ok(format!("luma {low}..{high}, mean {mean}: {verdict}"))
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
        let (width, height) = (output.width & !1, output.height & !1);
        let mut converter = Converter::new(&gfx, width, height)?;
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
        converter.convert(&frame)?;
        let image = luma_stats(&gfx, converter.target(), width, height)?;
        Ok(format!("{frames} updates in {CAPTURE_SECONDS} s, {image}"))
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

fn check_encode(out: &mut String, settings: &Settings, codec: Codec) {
    let settings = Settings { codec, replay: true, replay_seconds: 30, ..settings.clone() };
    let sink = Sink::new(true, 30);
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(VideoStats::default());
    let ctx = VideoContext { sink: sink.clone(), clock: Clock::new(), stop: stop.clone(), stats: stats.clone() };
    let thread = {
        let settings = settings.clone();
        std::thread::spawn(move || super::video::run(&settings, &ctx))
    };
    std::thread::sleep(Duration::from_secs(ENCODE_SECONDS));
    stop.store(true, Ordering::Relaxed);
    let result = thread.join().unwrap_or_else(|_| Err("panicked".into()));
    let name = codec_name(codec);
    if let Err(e) = result {
        line!(out, "  {name}: FAILED, {e}");
        return;
    }
    let (encoded, dropped) = (stats.encoded.load(Ordering::Relaxed), stats.dropped.load(Ordering::Relaxed));
    let ms = stats.encode_us.load(Ordering::Relaxed) as f32 / 1000.0;
    let method = stats.method.lock().unwrap().clone();
    let clip = sink.clip(30);
    let file = std::env::temp_dir().join(format!("tape-selftest-{name}.mp4"));
    let written = clip.as_ref().map(|c| {
        std::fs::File::create(&file).and_then(|f| write_clip(std::io::BufWriter::new(f), &c.tracks, &c.packets))
    });
    match written {
        Some(Ok(bytes)) => {
            line!(out, "  {name}: OK, {encoded} frames, {dropped} dropped, last frame {ms:.1} ms, {method}, wrote {bytes} bytes to {}", file.display());
        }
        Some(Err(e)) => {
            line!(out, "  {name}: FAILED writing the file, {e}");
        }
        None => {
            line!(out, "  {name}: FAILED, encoder produced nothing usable ({encoded} frames)");
        }
    }
}

fn check_audio(out: &mut String, label: &str, source: Source) {
    let clock = Clock::new();
    match wasapi::Input::open(source, FOLLOW_DEFAULT, clock) {
        Err(e) => {
            line!(out, "  {label}: not available, {e}");
        }
        Ok(mut input) => {
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
            line!(out, "  {label}: OK, {}, {blocks} packets, {samples} samples in 1 s, peak {loudest:.3}, latency {:?} samples", input.name(), first_gap);
        }
    }
}

fn check_aac(out: &mut String, backend: &Windows) {
    let result = (|| -> Result<(usize, usize), String> {
        let mut encoder = backend.aac_encoder()?;
        let silence = vec![0.0f32; AAC_FRAME * 2];
        let mut pcm = Vec::new();
        to_i16(&silence, &mut pcm);
        let (mut frames, mut bytes) = (0, 0);
        for _ in 0..100 {
            for frame in encoder.encode(&pcm)? {
                frames += 1;
                bytes += frame.len();
            }
        }
        Ok((frames, bytes))
    })();
    match result {
        Ok((frames, bytes)) => {
            line!(out, "  AAC: OK, 100 frames in, {frames} out, {bytes} bytes");
        }
        Err(e) => {
            line!(out, "  AAC: FAILED, {e}");
        }
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
    }

    line!(out, "\nEncode");
    let nvidia = outputs.iter().find(|o| o.is_nvidia());
    match nvidia.map(encoder::supported) {
        None => {
            line!(out, "  skipped, no monitor is on an NVIDIA GPU");
        }
        Some(Err(e)) => {
            line!(out, "  FAILED, {e}");
        }
        Some(Ok(codecs)) => {
            let names: Vec<_> = codecs.iter().map(|c| codec_name(*c)).collect();
            line!(out, "  this GPU encodes {}", names.join(", "));
            for codec in codecs {
                check_encode(&mut out, &Settings { capture: CaptureMethod::Auto, ..settings.clone() }, codec);
            }
        }
    }

    line!(out, "\nAudio");
    check_audio(&mut out, "Output loopback", Source::System);
    check_audio(&mut out, "Microphone", Source::Mic);
    check_aac(&mut out, backend);
    let (outs, ins) = (wasapi::devices(Source::System), wasapi::devices(Source::Mic));
    line!(out, "  {} output devices, {} input devices", outs.len(), ins.len());
    out
}
