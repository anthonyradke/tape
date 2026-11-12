//! WASAPI shared-mode capture: loopback from an output device, or a microphone.

use std::cell::Cell;
use std::time::{Duration, Instant};

use tape_core::audio::{to_stereo, Resampler};
use tape_core::ipc::AudioDevice;
use tape_core::packet::{from_micros, AUDIO_RATE};
use tape_core::settings::FOLLOW_DEFAULT;
use windows::core::{GUID, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use super::com_init;
use crate::backend::{AudioBlock, AudioInput, Clock, Source};
use crate::log;
use crate::mix::SilenceMeter;

const BUFFER_100NS: i64 = 10_000_000; // 1 s, far more than the 5 ms poll needs
const DEFAULT_CHECK: Duration = Duration::from_secs(2);
const TAG_PCM: u16 = 1;
const TAG_FLOAT: u16 = 3;
const TAG_EXTENSIBLE: u16 = 0xfffe;
/// Offset of SubFormat in WAVEFORMATEXTENSIBLE, whose first two bytes repeat the plain tag.
const SUBFORMAT_OFFSET: usize = 24;

fn flow(source: Source) -> EDataFlow {
    match source {
        Source::System => eRender,
        Source::Mic => eCapture,
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| e.to_string())
}

fn device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let raw: PWSTR = device.GetId().ok()?;
        let id = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        id
    }
}

fn friendly_name(device: &IMMDevice) -> String {
    unsafe {
        let Ok(store) = device.OpenPropertyStore(STGM_READ) else { return "Audio device".into() };
        let Ok(mut value) = store.GetValue(&PKEY_Device_FriendlyName) else { return "Audio device".into() };
        let name = value.Anonymous.Anonymous.Anonymous.pwszVal.to_string().unwrap_or_default();
        let _ = PropVariantClear(&mut value);
        name
    }
}

pub fn devices(source: Source) -> Vec<AudioDevice> {
    com_init();
    let list = || -> Result<Vec<AudioDevice>, String> {
        let e = enumerator()?;
        let text = |e: windows::core::Error| e.to_string();
        let collection = unsafe { e.EnumAudioEndpoints(flow(source), DEVICE_STATE_ACTIVE) }.map_err(text)?;
        let count = unsafe { collection.GetCount() }.map_err(text)?;
        let mut out = Vec::new();
        for i in 0..count {
            let device = unsafe { collection.Item(i) }.map_err(text)?;
            if let Some(id) = device_id(&device) {
                out.push(AudioDevice { id, name: friendly_name(&device) });
            }
        }
        Ok(out)
    };
    list().unwrap_or_default()
}

fn default_device(e: &IMMDeviceEnumerator, source: Source) -> Result<IMMDevice, String> {
    unsafe { e.GetDefaultAudioEndpoint(flow(source), eConsole) }.map_err(|_| match source {
        Source::System => "No output device".to_string(),
        Source::Mic => "No microphone".to_string(),
    })
}

fn qpc_100ns() -> i64 {
    let (mut counter, mut frequency) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    (counter as i128 * 10_000_000 / frequency.max(1) as i128) as i64
}

#[derive(Clone, Copy)]
enum Sample {
    Float32,
    Int16,
    Int24,
    Int32,
}

pub struct Input {
    source: Source,
    enumerator: IMMDeviceEnumerator,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    name: String,
    /// The default device's id, when following the default.
    following: Option<String>,
    checked: Cell<Instant>,
    sample: Sample,
    channels: usize,
    block_align: usize,
    rate: u32,
    resampler: Resampler,
    /// Subtracted from WASAPI timestamps to put them on the engine clock.
    origin_100ns: i64,
    clock: Clock,
    pcm: Vec<f32>,
    stereo: Vec<f32>,
    /// The mix format as WASAPI gave it, for the log.
    format: String,
    silence: SilenceMeter,
    /// Packets read so far. The first one is often flagged as a discontinuity, which means nothing.
    packets: u64,
}

impl Input {
    pub fn open(source: Source, device: &str, clock: Clock) -> Result<Input, String> {
        let enumerator = enumerator()?;
        let text = |e: windows::core::Error| e.to_string();
        let endpoint = if device == FOLLOW_DEFAULT {
            default_device(&enumerator, source)?
        } else {
            let wide: Vec<u16> = device.encode_utf16().chain([0]).collect();
            unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) }.map_err(|_| "Device not found".to_string())?
        };
        let following = (device == FOLLOW_DEFAULT).then(|| device_id(&endpoint)).flatten();
        let name = friendly_name(&endpoint);

        unsafe {
            let client: IAudioClient = endpoint.Activate(CLSCTX_ALL, None).map_err(text)?;
            let format = client.GetMixFormat().map_err(text)?;
            let f = format.read_unaligned();
            let mut tag = f.wFormatTag;
            if tag == TAG_EXTENSIBLE {
                let sub: GUID = (format as *const u8).add(SUBFORMAT_OFFSET).cast::<GUID>().read_unaligned();
                tag = sub.data1 as u16;
            }
            let sample = match (tag, f.wBitsPerSample) {
                (TAG_FLOAT, 32) => Sample::Float32,
                (TAG_PCM, 16) => Sample::Int16,
                (TAG_PCM, 24) => Sample::Int24,
                (TAG_PCM, 32) => Sample::Int32,
                (t, b) => {
                    CoTaskMemFree(Some(format as *const _));
                    return Err(format!("Unsupported audio format ({t}, {b} bit)"));
                }
            };
            let (channels, rate, block_align) = (f.nChannels as usize, f.nSamplesPerSec, f.nBlockAlign as usize);
            let kind = match sample {
                Sample::Float32 => "float",
                _ => "int",
            };
            let bits = f.wBitsPerSample; // a packed field, so copied out before formatting
            let format_text = format!("{kind} {bits} bit, {rate} Hz, {channels} channels");
            let flags = if source == Source::System { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
            let started = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_100NS, 0, format, None);
            CoTaskMemFree(Some(format as *const _));
            started.map_err(|e| format!("{e} ({name}, {format_text})"))?;
            let capture: IAudioCaptureClient = client.GetService().map_err(text)?;
            client.Start().map_err(text)?;
            Ok(Input {
                source,
                enumerator,
                client,
                capture,
                name,
                following,
                checked: Cell::new(Instant::now()),
                clock,
                sample,
                channels,
                block_align,
                rate,
                resampler: Resampler::new(rate, AUDIO_RATE),
                origin_100ns: qpc_100ns() - clock.now_us() * 10,
                pcm: Vec::new(),
                stereo: Vec::new(),
                format: format_text,
                silence: SilenceMeter::new(Instant::now()),
                packets: 0,
            })
        }
    }

    fn decode(&mut self, data: *const u8, frames: usize, silent: bool) {
        let samples = frames * self.channels;
        self.pcm.clear();
        if silent || data.is_null() {
            self.pcm.resize(samples, 0.0);
            return;
        }
        let bytes = unsafe { std::slice::from_raw_parts(data, frames * self.block_align) };
        let width = self.block_align / self.channels.max(1);
        for s in bytes.chunks_exact(width.max(1)).take(samples) {
            self.pcm.push(match self.sample {
                Sample::Float32 => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                Sample::Int16 => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
                Sample::Int24 => i32::from_le_bytes([0, s[0], s[1], s[2]]) as f32 / 2147483648.0,
                Sample::Int32 => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2147483648.0,
            });
        }
    }
}

impl AudioInput for Input {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn describe(&self) -> String {
        format!("{}, {}", self.name, self.format)
    }

    fn read(&mut self, out: &mut Vec<AudioBlock>) -> Result<(), String> {
        loop {
            let waiting = unsafe { self.capture.GetNextPacketSize() }.map_err(|e| format!("Device lost: {e}"))?;
            if waiting == 0 {
                return Ok(());
            }
            let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0u32, 0u32, 0u64);
            unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }.map_err(|e| format!("Device lost: {e}"))?;
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let discontinuity = self.packets > 0 && flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
            self.packets += 1;
            if let Some(line) = self.silence.packet(silent, Instant::now()) {
                log!("audio {:?} ({}): {line}", self.source, self.name);
            }
            self.decode(data, frames as usize, silent);
            unsafe { self.capture.ReleaseBuffer(frames) }.map_err(|e| format!("Device lost: {e}"))?;

            // Some drivers leave the timestamp empty, or flag it as wrong, after a glitch.
            let bad_time = qpc == 0 || flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0;
            let at_us = if bad_time {
                self.clock.now_us() - frames as i64 * 1_000_000 / self.rate as i64
            } else {
                (qpc as i64 - self.origin_100ns) / 10
            };
            self.stereo.clear();
            to_stereo(&self.pcm, self.channels, &mut self.stereo);
            let mut stereo = Vec::with_capacity(self.stereo.len() + 16);
            self.resampler.process(&self.stereo, &mut stereo);
            out.push(AudioBlock { at: from_micros(at_us, AUDIO_RATE), stereo, discontinuity });
        }
    }

    fn is_stale(&self) -> bool {
        let Some(current) = &self.following else { return false };
        if self.checked.get().elapsed() < DEFAULT_CHECK {
            return false;
        }
        self.checked.set(Instant::now());
        default_device(&self.enumerator, self.source).ok().and_then(|d| device_id(&d)).is_some_and(|id| id != *current)
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}
