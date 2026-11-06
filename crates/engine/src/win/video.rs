//! The video thread. One loop does everything in order (capture, convert, encode), so nothing
//! needs locking and the D3D11 context stays on one thread.

use std::sync::atomic::Ordering;

use tape_core::mp4::codec::parse_frame;
use tape_core::packet::{from_micros, VideoFormat};
use tape_core::settings::{CaptureMethod, Settings};
use windows::core::PCWSTR;
use windows::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};

use super::capture::{Capture, CaptureError, Duplication, GraphicsCapture};
use super::convert::Converter;
use super::encoder::Encoder;
use super::gfx::{self, Gfx, OutputInfo};
use crate::backend::VideoContext;
use crate::log;

/// Between attempts to reopen capture, which fails for as long as a UAC prompt is up.
const REOPEN_US: i64 = 500_000;
/// Attempts before Auto gives up on Desktop Duplication.
const AUTO_ATTEMPTS: u32 = 3;
/// Past this much lag the loop stops trying to catch up and counts the frames as dropped.
const MAX_LAG_FRAMES: i64 = 2;

/// Asks the scheduler to treat this thread as media work, so a busy game can't starve it.
pub fn raise_thread(task: &str) {
    let name: Vec<u16> = task.encode_utf16().chain([0]).collect();
    let mut index = 0;
    unsafe {
        let _ = AvSetMmThreadCharacteristicsW(PCWSTR(name.as_ptr()), &mut index);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

fn open_capture(
    gfx: &Gfx,
    output: &OutputInfo,
    settings: &Settings,
    use_graphics: bool,
) -> Result<Box<dyn Capture>, CaptureError> {
    if use_graphics {
        Ok(Box::new(GraphicsCapture::open(gfx, output, settings.show_cursor)?))
    } else {
        Ok(Box::new(Duplication::open(gfx, output, settings.show_cursor)?))
    }
}

pub fn run(settings: &Settings, ctx: &VideoContext) -> Result<(), String> {
    super::com_init();
    raise_thread("Capture");

    let mut output = gfx::find(&settings.monitor)?;
    if !output.is_nvidia() {
        return Err(format!("{} is connected to {}, not an NVIDIA GPU", output.id, output.gpu));
    }
    let gfx = Gfx::new(&output.adapter)?;
    let (width, height) = settings.output_size(output.width, output.height);
    let mut converter = Converter::new(&gfx, width, height)?;
    let target = converter.target().clone();
    let mut encoder = Encoder::open(&gfx, &target, settings, width, height)?;

    ctx.stats.width.store(width, Ordering::Relaxed);
    ctx.stats.height.store(height, Ordering::Relaxed);
    log!("video: {width}x{height} {} fps {:?} {} Mbps on {}", settings.fps, settings.codec, settings.bitrate_mbps, output.gpu);

    let mut use_graphics = settings.capture == CaptureMethod::Graphics;
    let mut capture: Option<Box<dyn Capture>> = None;
    let mut attempts = 0;
    let mut next_open = 0i64;

    let fps = settings.fps;
    let period = 1_000_000.0 / fps as f64;
    let mut format = VideoFormat { codec: settings.codec, width, height, fps, config: Vec::new() };
    let start_us = ctx.clock.now_us();
    let mut tick = 0i64;
    let mut last_pts = i64::MIN;
    let mut force_key = true;
    let mut has_image = false;
    let mut changed = false;

    while !ctx.stop.load(Ordering::Relaxed) {
        ctx.stats.beat(&ctx.clock);
        let due = start_us + (tick as f64 * period) as i64;

        // Take new images until this frame is due.
        loop {
            let now = ctx.clock.now_us();
            if capture.is_none() && now >= next_open {
                next_open = now + REOPEN_US;
                let opened = match gfx::current(&output.adapter, &output.id) {
                    gfx::Current::Found(fresh) => {
                        output = fresh;
                        open_capture(&gfx, &output, settings, use_graphics)
                    }
                    gfx::Current::Missing => Err(CaptureError::Transient("The monitor is off or disconnected".into())),
                    // The engine restarts the pipeline, which makes a new device on the right GPU.
                    gfx::Current::Elsewhere => return Err(format!("{} changed GPUs, starting capture again", output.id)),
                };
                match opened {
                    Ok(c) => {
                        *ctx.stats.method.lock().unwrap() = c.method().to_string();
                        log!("capture: {}", c.method());
                        capture = Some(c);
                        attempts = 0;
                    }
                    Err(e) => {
                        attempts += 1;
                        log!("capture: {}", e.message());
                        *ctx.stats.problem.lock().unwrap() = Some(e.message().to_string());
                        let refused = matches!(e, CaptureError::Refused(_));
                        // Auto keeps alternating, so a refusal that has passed (a display change
                        // mid-open) doesn't leave it stuck on the fallback for good.
                        if settings.capture == CaptureMethod::Auto && refused && attempts >= AUTO_ATTEMPTS {
                            use_graphics = !use_graphics;
                            attempts = 0;
                            log!("capture: switching to {}", if use_graphics { "Windows Graphics Capture" } else { "Desktop Duplication" });
                        }
                    }
                }
            }
            let remaining = due - ctx.clock.now_us();
            if remaining <= 0 {
                break;
            }
            let wait = ((remaining + 999) / 1000).min(50) as u32;
            match capture.as_mut() {
                Some(c) => match c.next(wait) {
                    Ok(updated) => changed |= updated,
                    Err(e) => {
                        // After a GPU reset everything on this device is dead, so start over.
                        if let Err(removed) = unsafe { gfx.device.GetDeviceRemovedReason() } {
                            return Err(format!("the GPU was reset: {removed}"));
                        }
                        // Otherwise the last image keeps being encoded, so the timeline has no
                        // gap and the replay buffer stays usable through a UAC prompt.
                        log!("capture: {}", e.message());
                        capture = None;
                    }
                },
                None => std::thread::sleep(std::time::Duration::from_micros(remaining.min(50_000) as u64)),
            }
        }

        let began = ctx.clock.now_us();
        if changed || !has_image {
            if let Some(source) = capture.as_mut().and_then(|c| c.frame()) {
                converter.convert(&source)?;
                has_image = true;
                changed = false;
            }
        }
        // The output texture holds garbage until the first conversion, so nothing is encoded
        // before then. An unchanged screen re-encodes the same texture, which costs almost nothing.
        if has_image {
            force_key |= ctx.sink.take_keyframe_request();
            let pts = from_micros(due, fps).max(last_pts + 1);
            last_pts = pts;
            let data = encoder.encode(pts as u64, force_key)?;
            force_key = false;
            if !data.is_empty() {
                let parsed = parse_frame(settings.codec, &data);
                // Parameter sets are repeated on every keyframe and nowhere else, which makes them
                // a test for keyframes that works the same for all three codecs.
                let key = parsed.config.is_some();
                if format.config.is_empty() {
                    format.config = parsed.config.unwrap_or_default();
                }
                if format.config.is_empty() {
                    force_key = true; // nothing can be written until the parameter sets are known
                } else {
                    ctx.stats.bytes.fetch_add(parsed.sample.len() as u64, Ordering::Relaxed);
                    ctx.sink.video(&format, pts, key, parsed.sample);
                    ctx.stats.encoded.fetch_add(1, Ordering::Relaxed);
                    ctx.stats.running.store(true, Ordering::Relaxed);
                    if capture.is_some() {
                        *ctx.stats.problem.lock().unwrap() = None;
                    }
                }
            }
        }
        let now = ctx.clock.now_us();
        ctx.stats.encode_us.store((now - began) as u32, Ordering::Relaxed);

        tick += 1;
        let behind = ((now - start_us) as f64 / period) as i64 - tick;
        if behind > MAX_LAG_FRAMES {
            // Seconds of lag is the PC waking from sleep, not frames lost to load.
            if behind < fps as i64 * 2 {
                ctx.stats.dropped.fetch_add(behind as u64, Ordering::Relaxed);
            }
            tick += behind;
        }
    }
    ctx.stats.running.store(false, Ordering::Relaxed);
    Ok(())
}
