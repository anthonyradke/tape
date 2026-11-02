//! The real backend: DXGI or Windows Graphics Capture into D3D11, NVENC for video, WASAPI and
//! the Media Foundation AAC encoder for audio.

mod aac;
mod capture;
mod convert;
mod cursor;
mod encoder;
mod gfx;
mod selftest;
pub mod share;
mod video;
mod wasapi;

use std::path::PathBuf;

use tape_core::game::Foreground;
use tape_core::ipc::Probe;
use tape_core::packet::Codec;
use tape_core::settings::Settings;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, QueryFullProcessImageNameW, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, GWL_STYLE, WS_CAPTION,
};

use crate::backend::{AacEncoder, AudioInput, Backend, Clock, Source, VideoContext};

pub struct Windows;

/// Joins the multithreaded COM apartment. Safe to repeat on a thread.
pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

impl Windows {
    pub fn new() -> Self {
        unsafe {
            // Desktop Duplication refuses a process that isn't DPI aware.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            // Frame pacing relies on 1 ms timers.
            timeBeginPeriod(1);
            let _ = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
        }
        Windows
    }
}

fn title(window: HWND) -> Option<String> {
    let mut buffer = [0u16; 256];
    let len = unsafe { GetWindowTextW(window, &mut buffer) };
    (len > 0).then(|| String::from_utf16_lossy(&buffer[..len as usize]))
}

/// Limited information is all this asks for, which anti-cheat software allows.
fn executable(window: HWND) -> Option<String> {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    let got = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len) };
    let _ = unsafe { CloseHandle(process) };
    got.ok().map(|_| String::from_utf16_lossy(&buffer[..len as usize]))
}

/// Borderless and exclusive fullscreen both pass. A maximized window keeps its title bar.
fn fills_monitor(window: HWND) -> bool {
    unsafe {
        if GetWindowLongW(window, GWL_STYLE) as u32 & WS_CAPTION.0 == WS_CAPTION.0 {
            return false;
        }
        let mut rect = RECT::default();
        let mut monitor = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetWindowRect(window, &mut rect).is_err() || !GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &mut monitor).as_bool() {
            return false;
        }
        let area = monitor.rcMonitor;
        rect.left <= area.left && rect.top <= area.top && rect.right >= area.right && rect.bottom >= area.bottom
    }
}

impl Backend for Windows {
    fn probe(&self) -> Probe {
        com_init();
        let mut probe = Probe::default();
        match gfx::outputs() {
            Ok(outputs) => {
                probe.monitors = outputs.iter().enumerate().map(|(i, o)| o.monitor(i)).collect();
                let nvidia = outputs.iter().find(|o| o.is_nvidia());
                if let Some(o) = nvidia.or(outputs.first()) {
                    probe.gpu = o.gpu.clone();
                    probe.driver = o.driver.clone();
                }
                match nvidia {
                    Some(o) => match encoder::supported(o) {
                        Ok(codecs) => probe.codecs = codecs,
                        Err(e) => probe.nvenc_error = Some(e),
                    },
                    None => probe.nvenc_error = Some("No monitor is connected to an NVIDIA GPU".into()),
                }
            }
            Err(e) => probe.nvenc_error = Some(e),
        }
        probe.outputs = wasapi::devices(Source::System);
        probe.inputs = wasapi::devices(Source::Mic);
        probe
    }

    fn run_video(&self, settings: &Settings, ctx: &VideoContext) -> Result<(), String> {
        video::run(settings, ctx)
    }

    fn open_audio(&self, source: Source, device: &str, clock: Clock) -> Result<Box<dyn AudioInput>, String> {
        Ok(Box::new(wasapi::Input::open(source, device, clock)?))
    }

    fn aac_encoder(&self) -> Result<Box<dyn AacEncoder>, String> {
        Ok(Box::new(aac::Aac::new()?))
    }

    fn audio_thread_setup(&self) {
        com_init();
        video::raise_thread("Audio");
    }

    fn foreground(&self) -> Option<Foreground> {
        let window = unsafe { GetForegroundWindow() };
        if window.is_invalid() {
            return None;
        }
        Some(Foreground { title: title(window).unwrap_or_default(), exe: executable(window).unwrap_or_default(), fullscreen: fills_monitor(window) })
    }

    fn default_folder(&self) -> PathBuf {
        tape_core::naming::default_folder()
    }

    fn self_test(&self, settings: &Settings) -> String {
        selftest::report(self, settings)
    }
}

pub fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
    }
}
