//! The smaller copy of a clip that "Copy link" uploads: H.264 at no more than 1080p and 60 fps,
//! around 10 Mbps at 1080p60, with the first audio track as AAC. Plays in every browser and chat
//! app, whatever the original was recorded in. Media Foundation does the work, decoding and
//! encoding on the GPU where it can and in software where it can't.

use std::path::Path;

use tape_core::share::{share_size, ShareTarget};
use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
use windows::Win32::Media::MediaFoundation::*;

use super::gfx;

const HUNDRED_NS: f64 = 10_000_000.0;
/// 160 kb/s, one of the AAC encoder's four fixed rates.
const AAC_BYTES_PER_SECOND: u32 = 20_000;
/// Samples held back at the start while waiting for the other stream's first time. The reader
/// interleaves by time, so it comes within a few, and holding decoded frames on the GPU for long
/// could starve the decoder of surfaces.
const START_HOLD: usize = 8;
const AAC_LC_LEVEL_2: u32 = 0x29;

pub struct Copied {
    pub width: u32,
    pub height: u32,
    pub seconds: f64,
    /// How it was done, for the log: "GPU", "GPU encoder" or "software".
    pub path: &'static str,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Frames stay on the GPU from decoder to encoder.
    Gpu,
    /// Decoded into system memory, encoded by whatever hardware encoder there is.
    HardwareEncoder,
    Software,
}

fn text(e: windows::core::Error) -> String {
    e.to_string()
}

const fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// Tries the fast ways first. Each failed attempt removes its partial file before the next.
pub fn share_copy(input: &Path, output: &Path, progress: &mut dyn FnMut(f64)) -> Result<Copied, String> {
    super::com_init();
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| format!("Media Foundation unavailable: {e}"))?;
    let mut first_error = None;
    for (mode, path) in [(Mode::Gpu, "GPU"), (Mode::HardwareEncoder, "GPU encoder"), (Mode::Software, "software")] {
        let _ = std::fs::remove_file(output);
        match convert(input, output, mode, progress) {
            Ok((width, height, seconds)) => return Ok(Copied { width, height, seconds, path }),
            Err(e) => {
                crate::log::line(&format!("share copy ({path}) failed: {e}"));
                first_error.get_or_insert(e);
            }
        }
    }
    let _ = std::fs::remove_file(output);
    Err(first_error.unwrap_or_default())
}

/// A device on the NVIDIA GPU if there is one, at normal priority: this runs beside a game.
fn device_manager() -> windows::core::Result<IMFDXGIDeviceManager> {
    let adapter: Option<IDXGIAdapter> = gfx::outputs()
        .ok()
        .and_then(|all| all.into_iter().find(|o| o.is_nvidia()))
        .and_then(|o| o.adapter.cast().ok());
    let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            adapter.as_ref(),
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device: ID3D11Device = device.unwrap();
        // Decoder and encoder use it from their own threads.
        let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        let (mut token, mut manager) = (0, None);
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.unwrap();
        manager.ResetDevice(&device, token)?;
        Ok(manager)
    }
}

fn attributes(count: u32) -> windows::core::Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, count)? };
    Ok(attributes.unwrap())
}

fn video_type(subtype: &windows::core::GUID, target: &ShareTarget) -> windows::core::Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(target.width, target.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(target.fps_num, target.fps_den))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(t)
    }
}

fn convert(input: &Path, output: &Path, mode: Mode, progress: &mut dyn FnMut(f64)) -> Result<(u32, u32, f64), String> {
    let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let audio_stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    unsafe {
        let manager = if mode == Mode::Gpu { Some(device_manager().map_err(|e| format!("no GPU device: {e}"))?) } else { None };

        let options = attributes(4).map_err(text)?;
        options.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1).map_err(text)?;
        options.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, (mode != Mode::Software) as u32).map_err(text)?;
        if let Some(manager) = &manager {
            options.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, manager).map_err(text)?;
        }
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(input.as_os_str()), &options).map_err(|e| format!("can't open the clip: {e}"))?;

        let native = reader.GetNativeMediaType(video_stream, 0).map_err(|e| format!("no video in the clip: {e}"))?;
        let size = native.GetUINT64(&MF_MT_FRAME_SIZE).map_err(text)?;
        let rate = native.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(pack(60, 1));
        let target = share_size((size >> 32) as u32, size as u32, (rate >> 32) as u32, rate as u32);
        let duration = reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .map(|v| v.Anonymous.Anonymous.Anonymous.uhVal as i64)
            .unwrap_or(0);

        // Only the first audio track: browsers and chat apps play no other.
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).map_err(text)?;
        reader.SetStreamSelection(video_stream, true).map_err(text)?;
        let has_audio = reader.SetStreamSelection(audio_stream, true).is_ok();

        let decoded = video_type(&MFVideoFormat_NV12, &target).map_err(text)?;
        reader
            .SetCurrentMediaType(video_stream, None, &decoded)
            .map_err(|e| format!("can't decode this clip's video (for HEVC, Windows needs the HEVC Video Extensions): {e}"))?;

        let mut pcm = None;
        if has_audio {
            let t = MFCreateMediaType().map_err(text)?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(text)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).map_err(text)?;
            t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(text)?;
            if reader.SetCurrentMediaType(audio_stream, None, &t).is_ok() {
                pcm = reader.GetCurrentMediaType(audio_stream).ok();
            }
            if pcm.is_none() {
                let _ = reader.SetStreamSelection(audio_stream, false);
            }
        }

        let sink = attributes(4).map_err(text)?;
        sink.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4).map_err(text)?;
        sink.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, (mode != Mode::Software) as u32).map_err(text)?;
        if let Some(manager) = &manager {
            sink.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, manager).map_err(text)?;
        }
        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(output.as_os_str()), None, &sink).map_err(|e| format!("can't write the copy: {e}"))?;

        let encoded = video_type(&MFVideoFormat_H264, &target).map_err(text)?;
        encoded.SetUINT32(&MF_MT_AVG_BITRATE, target.bitrate).map_err(text)?;
        encoded.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32).map_err(text)?;
        let out_video = writer.AddStream(&encoded).map_err(|e| format!("no H.264 encoder: {e}"))?;
        let params = attributes(3).map_err(text)?;
        params.SetUINT32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32).map_err(text)?;
        params.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, target.bitrate).map_err(text)?;
        params.SetUINT32(&CODECAPI_AVEncMPVGOPSize, target.fps_num / target.fps_den.max(1) * 2).map_err(text)?;
        writer.SetInputMediaType(out_video, &decoded, &params).map_err(|e| format!("the H.264 encoder refused the video: {e}"))?;

        let mut out_audio = None;
        if let Some(pcm) = &pcm {
            let rate = pcm.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(48_000);
            let channels = pcm.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2);
            let aac = MFCreateMediaType().map_err(text)?;
            aac.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(text)?;
            aac.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).map_err(text)?;
            aac.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(text)?;
            aac.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).map_err(text)?;
            aac.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels.min(2)).map_err(text)?;
            aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND).map_err(text)?;
            aac.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, AAC_LC_LEVEL_2).map_err(text)?;
            let stream = writer.AddStream(&aac).map_err(|e| format!("no AAC encoder: {e}"))?;
            writer.SetInputMediaType(stream, pcm, None).map_err(|e| format!("the AAC encoder refused the audio: {e}"))?;
            out_audio = Some(stream);
        }

        writer.BeginWriting().map_err(text)?;
        let real_video = real_index(&reader, &MFMediaType_Video);
        // Over 60 fps, frames are dropped onto a 60 fps grid rather than resampled.
        let period = (HUNDRED_NS * target.fps_den as f64 / target.fps_num as f64) as i64;
        let mut next_frame = None;
        let mut shown = -1i64;
        let mut write = |out: u32, is_video: bool, flags: u32, at: i64, sample: Option<IMFSample>| -> Result<(), String> {
            if flags & MF_SOURCE_READERF_STREAMTICK.0 as u32 != 0 {
                writer.SendStreamTick(out, at).map_err(text)?;
            }
            let Some(sample) = sample else { return Ok(()) };
            if is_video {
                if target.drop_frames {
                    let next = next_frame.unwrap_or(at);
                    if at + period / 4 < next {
                        return Ok(());
                    }
                    // Back on the grid after a gap rather than catching up with a burst.
                    next_frame = Some(if at - next > period { at + period } else { next + period });
                }
                sample.SetSampleDuration(period).map_err(text)?;
            }
            sample.SetSampleTime(at).map_err(text)?;
            writer.WriteSample(out, &sample).map_err(|e| format!("encoding failed: {e}"))?;
            if is_video && duration > 0 {
                let percent = at * 100 / duration;
                if percent != shown {
                    shown = percent;
                    progress((at as f64 / duration as f64).min(1.0));
                }
            }
            Ok(())
        };
        // The copy starts at zero from the earlier of the two streams' first samples, so neither
        // has samples before the start (which would all land on zero) and they stay in sync.
        let mut base = None;
        let (mut first_video, mut first_audio) = (None, None);
        let mut held = Vec::new();
        let (mut video_done, mut audio_done) = (false, out_audio.is_none());
        while !(video_done && audio_done) {
            let (mut index, mut flags, mut time, mut sample) = (0u32, 0u32, 0i64, None);
            reader
                .ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut index), Some(&mut flags), Some(&mut time), Some(&mut sample))
                .map_err(|e| format!("decoding failed: {e}"))?;
            let is_video = index == real_video;
            let ended = flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0;
            if ended {
                if is_video {
                    video_done = true;
                } else {
                    audio_done = true;
                }
            }
            let out = match (is_video, out_audio) {
                _ if ended => None,
                (true, _) => Some(out_video),
                (false, Some(audio)) => Some(audio),
                (false, None) => None,
            };
            if base.is_none() {
                if let Some(out) = out {
                    *(if is_video { &mut first_video } else { &mut first_audio }) = Some(time);
                    held.push((out, is_video, flags, time, sample));
                }
                let both = first_video.is_some() && (first_audio.is_some() || out_audio.is_none());
                if both || video_done || audio_done || held.len() >= START_HOLD {
                    let start = first_video.into_iter().chain(first_audio).min().unwrap_or(0);
                    base = Some(start);
                    for (out, is_video, flags, time, sample) in held.drain(..) {
                        write(out, is_video, flags, (time - start).max(0), sample)?;
                    }
                }
                continue;
            }
            if let (Some(out), Some(start)) = (out, base) {
                write(out, is_video, flags, (time - start).max(0), sample)?;
            }
        }
        drop(write);
        writer.Finalize().map_err(|e| format!("couldn't finish the copy: {e}"))?;
        drop(writer);
        // Index at the front, so the video starts playing before it has all downloaded. Done here
        // rather than with MF_MPEG4SINK_MOOV_BEFORE_MDAT, which on a runner either failed with
        // access denied or, given a read-write stream, scrambled the data it moved.
        tape_core::mp4::faststart::faststart(output).map_err(|e| format!("couldn't finish the copy: {e}"))?;
        Ok((target.width, target.height, duration as f64 / HUNDRED_NS))
    }
}

/// ReadSample reports real stream indexes, not the FIRST_VIDEO_STREAM alias.
fn real_index(reader: &IMFSourceReader, major: &windows::core::GUID) -> u32 {
    (0..32)
        .map_while(|i| unsafe { reader.GetNativeMediaType(i, 0) }.ok().map(|t| (i, t)))
        .find(|(_, t)| unsafe { t.GetGUID(&MF_MT_MAJOR_TYPE) }.ok().as_ref() == Some(major))
        .map_or(u32::MAX, |(i, _)| i)
}
