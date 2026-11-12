//! WASAPI shared-mode capture: loopback from an output device, process loopback that leaves tape's
//! own sounds out, or a microphone.

use std::cell::Cell;
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tape_core::audio::{to_stereo, Resampler};
use tape_core::ipc::AudioDevice;
use tape_core::packet::{from_micros, AUDIO_RATE};
use tape_core::settings::FOLLOW_DEFAULT;
use windows::core::{implement, Interface, GUID, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, BLOB, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::Variant::VT_BLOB;
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

/// What a stream delivers.
struct Layout {
    sample: Sample,
    channels: usize,
    block_align: usize,
    rate: u32,
    /// For the log.
    text: String,
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
    /// Process loopback wants an event to signal, though packets are polled for here.
    event: Option<HANDLE>,
    /// Process loopback leaving tape's processes out.
    excludes_tape: bool,
}

/// Told when an asynchronous activation finishes. Windows calls it on a thread of its own, so it
/// has to be agile, which `#[implement]` objects are unless asked otherwise.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated {
    done: Mutex<Option<Sender<()>>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _operation: windows_core::Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        // The result is read from the operation by the thread that asked, once this has said so.
        if let Some(done) = self.done.lock().unwrap().take() {
            let _ = done.send(());
        }
        Ok(())
    }
}

/// How long activation may take. It answers in milliseconds; this only guards against a hang.
const ACTIVATE_LIMIT: Duration = Duration::from_secs(5);

/// A process loopback client for what `pid` and the processes it started play, or for everything
/// on the default output but that, as in Microsoft's ApplicationLoopback sample. Needs Windows 10
/// 2004 (build 19041); earlier versions fail the activation.
fn process_loopback(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<IAudioClient, String> {
    let text = |e: windows::core::Error| e.to_string();
    // Boxed so that if activation never finishes, the parameters it may still read can be left behind.
    let mut params = Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    });
    // A VT_BLOB pointing at the parameters, which only have to live until activation is done.
    let mut blob = PROPVARIANT::default();
    unsafe {
        let inner = &mut *blob.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB { cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32, pBlobData: (&mut *params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast() };
    }
    let (done, finished) = channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated { done: Mutex::new(Some(done)) }.into();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&blob), &handler)
    }
    .map_err(text)?;
    if finished.recv_timeout(ACTIVATE_LIMIT).is_err() {
        std::mem::forget(params);
        return Err("process loopback activation never finished".into());
    }
    let (mut result, mut client) = (windows::core::HRESULT(0), None);
    unsafe { operation.GetActivateResult(&mut result, &mut client) }.map_err(text)?;
    result.ok().map_err(text)?;
    client.ok_or_else(|| "process loopback gave no client".to_string())?.cast::<IAudioClient>().map_err(text)
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
            let layout = Layout { sample, channels, block_align, rate, text: format_text };
            Input::start(source, enumerator, client, name, following, clock, layout)
        }
    }

    /// Game audio as process loopback: everything on the default output except what `pid` and
    /// the processes it started play, or only that with `include`. The default device's name is
    /// what shows, since that is what's being heard.
    pub fn open_process_loopback(pid: u32, include: bool, clock: Clock) -> Result<Input, String> {
        let enumerator = enumerator()?;
        let endpoint = default_device(&enumerator, Source::System)?;
        let following = device_id(&endpoint);
        let name = friendly_name(&endpoint);
        let mode = if include { PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE } else { PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE };
        let client = process_loopback(pid, mode)?;
        // There's no mix format to ask for: the stream takes the one it's given and converts to it.
        let format = WAVEFORMATEX {
            wFormatTag: TAG_PCM,
            nChannels: 2,
            nSamplesPerSec: AUDIO_RATE,
            nAvgBytesPerSec: AUDIO_RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let text = |e: windows::core::Error| e.to_string();
        unsafe {
            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_100NS, 0, &format, None).map_err(|e| format!("{e} (process loopback)"))?;
            let event = CreateEventW(None, false, false, PCWSTR::null()).map_err(text)?;
            if let Err(e) = client.SetEventHandle(event) {
                let _ = CloseHandle(event);
                return Err(text(e));
            }
            let what = if include { "only" } else { "leaving out" };
            let layout = Layout { sample: Sample::Int16, channels: 2, block_align: 4, rate: AUDIO_RATE, text: format!("process loopback {what} process {pid}, int 16 bit, {AUDIO_RATE} Hz, 2 channels") };
            let mut input = match Input::start(Source::System, enumerator, client, name, following, clock, layout) {
                Ok(input) => input,
                Err(e) => {
                    let _ = CloseHandle(event);
                    return Err(e);
                }
            };
            input.event = Some(event);
            input.excludes_tape = !include;
            Ok(input)
        }
    }

    fn start(
        source: Source,
        enumerator: IMMDeviceEnumerator,
        client: IAudioClient,
        name: String,
        following: Option<String>,
        clock: Clock,
        layout: Layout,
    ) -> Result<Input, String> {
        let text = |e: windows::core::Error| e.to_string();
        let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(text)?;
        unsafe { client.Start() }.map_err(text)?;
        Ok(Input {
            source,
            enumerator,
            client,
            capture,
            name,
            following,
            checked: Cell::new(Instant::now()),
            clock,
            sample: layout.sample,
            channels: layout.channels,
            block_align: layout.block_align,
            rate: layout.rate,
            resampler: Resampler::new(layout.rate, AUDIO_RATE),
            origin_100ns: qpc_100ns() - clock.now_us() * 10,
            pcm: Vec::new(),
            stereo: Vec::new(),
            format: layout.text,
            silence: SilenceMeter::new(Instant::now()),
            packets: 0,
            event: None,
            excludes_tape: false,
        })
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

    fn excludes_tape(&self) -> bool {
        self.excludes_tape
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
            let estimate = self.clock.now_us() - frames as i64 * 1_000_000 / self.rate as i64;
            let stamped = (qpc as i64 - self.origin_100ns) / 10;
            // A timestamp a second away from now is wrong whatever the driver says about it.
            let at_us = if bad_time || (stamped - estimate).abs() > 1_000_000 { estimate } else { stamped };
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
        if let Some(event) = self.event.take() {
            let _ = unsafe { CloseHandle(event) };
        }
    }
}
