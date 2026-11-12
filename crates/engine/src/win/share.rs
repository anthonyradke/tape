//! The smaller copy of a clip that "Copy link" uploads: H.264 at no more than 1080p and 60 fps,
//! around 10 Mbps at 1080p60, with one AAC track. Plays in every browser and chat app, whatever
//! the original was recorded in. Media Foundation does the work, decoding and encoding on the GPU
//! where it can and in software where it can't.
//!
//! Browsers and chat apps play only the first audio track, so a clip with game audio and mic kept
//! apart has them mixed into one here (`audio::Mixdown`). A range (the trim selection) is cut
//! while converting: the reader seeks to the keyframe before it and what decodes before the start
//! or after the end is dropped, so no trimmed file is written first.

use std::path::Path;

use tape_core::audio::{to_i16, to_stereo, Mixdown};
use tape_core::share::{share_size, ShareTarget};
use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_I8;

use super::gfx;

const HUNDRED_NS: f64 = 10_000_000.0;
/// 160 kb/s, one of the AAC encoder's four fixed rates.
const AAC_BYTES_PER_SECOND: u32 = 20_000;
/// Samples held back at the start while waiting for the other stream's first time. The reader
/// interleaves by time, so it comes within a few, and holding decoded frames on the GPU for long
/// could starve the decoder of surfaces.
const START_HOLD: usize = 8;
const AAC_LC_LEVEL_2: u32 = 0x29;
/// Audio tracks mixed at most. tape writes two; anything past that isn't worth the decoding.
const MAX_AUDIO_TRACKS: usize = 4;

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
/// `range` is (start, end) in seconds, for sharing part of the clip.
pub fn share_copy(input: &Path, output: &Path, range: Option<(f64, f64)>, progress: &mut dyn FnMut(f64)) -> Result<Copied, String> {
    super::com_init();
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| format!("Media Foundation unavailable: {e}"))?;
    let mut first_error = None;
    for (mode, path) in [(Mode::Gpu, "GPU"), (Mode::HardwareEncoder, "GPU encoder"), (Mode::Software, "software")] {
        let _ = std::fs::remove_file(output);
        match convert(input, output, mode, range, progress) {
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

fn convert(input: &Path, output: &Path, mode: Mode, range: Option<(f64, f64)>, progress: &mut dyn FnMut(f64)) -> Result<(u32, u32, f64), String> {
    let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
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
        // The part to copy, in 100 ns units: all of it without a range.
        let (from, until) = match range {
            Some((start, end)) => ((start.max(0.0) * HUNDRED_NS) as i64, (end * HUNDRED_NS) as i64),
            None => (0, i64::MAX),
        };
        let length = if duration > 0 { until.min(duration) - from } else { until.saturating_sub(from) };
        if range.is_some() && length <= 0 {
            return Err("nothing between those times".into());
        }

        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).map_err(text)?;
        reader.SetStreamSelection(video_stream, true).map_err(text)?;

        let decoded = video_type(&MFVideoFormat_NV12, &target).map_err(text)?;
        reader
            .SetCurrentMediaType(video_stream, None, &decoded)
            .map_err(|e| format!("can't decode this clip's video (for HEVC, Windows needs the HEVC Video Extensions): {e}"))?;

        // Every audio track the decoder takes, as float at the first one's rate, to be mixed.
        let mut audio_streams: Vec<(u32, usize)> = Vec::new(); // (stream, channels)
        let mut audio_rate = 0;
        for index in audio_indexes(&reader).into_iter().take(MAX_AUDIO_TRACKS) {
            let t = MFCreateMediaType().map_err(text)?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(text)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float).map_err(text)?;
            t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32).map_err(text)?;
            if reader.SetStreamSelection(index, true).is_err() || reader.SetCurrentMediaType(index, None, &t).is_err() {
                let _ = reader.SetStreamSelection(index, false);
                continue;
            }
            let Ok(pcm) = reader.GetCurrentMediaType(index) else { continue };
            let rate = pcm.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(48_000);
            let channels = pcm.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2) as usize;
            if audio_rate == 0 {
                audio_rate = rate;
            }
            // Tracks at another rate would need resampling; tape never writes those.
            if rate != audio_rate || channels == 0 {
                let _ = reader.SetStreamSelection(index, false);
                continue;
            }
            audio_streams.push((index, channels));
        }
        if audio_streams.len() > 1 {
            crate::log::line(&format!("share copy: mixing {} audio tracks into one", audio_streams.len()));
        }

        if range.is_some() {
            // Lands on the keyframe at or before the start; the frames up to it are decoded and dropped.
            let mut position = PROPVARIANT::default();
            (*position.Anonymous.Anonymous).vt = VT_I8;
            (*position.Anonymous.Anonymous).Anonymous.hVal = from;
            reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &position).map_err(|e| format!("can't seek in the clip: {e}"))?;
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
        if !audio_streams.is_empty() {
            let aac = MFCreateMediaType().map_err(text)?;
            aac.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(text)?;
            aac.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).map_err(text)?;
            aac.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(text)?;
            aac.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio_rate).map_err(text)?;
            aac.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).map_err(text)?;
            aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND).map_err(text)?;
            aac.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, AAC_LC_LEVEL_2).map_err(text)?;
            let stream = writer.AddStream(&aac).map_err(|e| format!("no AAC encoder: {e}"))?;
            let pcm = pcm_type(audio_rate).map_err(text)?;
            writer.SetInputMediaType(stream, &pcm, None).map_err(|e| format!("the AAC encoder refused the audio: {e}"))?;
            out_audio = Some(stream);
        }

        writer.BeginWriting().map_err(text)?;
        let real_video = real_index(&reader, &MFMediaType_Video);
        let frame_of = |hns: i64| (hns as i128 * audio_rate as i128 / HUNDRED_NS as i128) as i64;
        let hns_of = |frame: i64| (frame as i128 * HUNDRED_NS as i128 / audio_rate.max(1) as i128) as i64;
        let mut mix = Mixdown::new(audio_streams.len(), range.map(|_| frame_of(from)), range.map(|_| frame_of(until)));
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
            if is_video && length > 0 {
                let percent = at * 100 / length;
                if percent != shown {
                    shown = percent;
                    progress((at as f64 / length as f64).min(1.0));
                }
            }
            Ok(())
        };
        // The copy starts at zero. With a range that's its start; otherwise the earlier of the
        // video's and the audio's first samples, so neither has samples before the start (which
        // would all land on zero) and they stay in sync.
        let mut base = range.map(|_| from);
        let (mut first_video, mut first_audio) = (None, None);
        let mut held = Vec::new();
        let (mut video_done, mut audio_done) = (false, out_audio.is_none());
        let mut floats = Vec::new();
        let mut stereo = Vec::new();
        let mut mixed = Vec::new();
        let mut pcm = Vec::new();
        while !(video_done && audio_done) {
            let (mut index, mut flags, mut time, mut sample) = (0u32, 0u32, 0i64, None);
            reader
                .ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut index), Some(&mut flags), Some(&mut time), Some(&mut sample))
                .map_err(|e| format!("decoding failed: {e}"))?;
            let ended = flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0;
            // Ready to write: (stream, is video, flags, source time, sample).
            let mut ready: Vec<(u32, bool, u32, i64, Option<IMFSample>)> = Vec::new();
            if index == real_video {
                if ended || time >= until {
                    if !video_done {
                        video_done = true;
                        // Past the end of the range: no need to decode the rest.
                        let _ = reader.SetStreamSelection(video_stream, false);
                    }
                } else if time >= from || range.is_none() {
                    ready.push((out_video, true, flags, time, sample));
                }
            } else if let (Some(out), Some(lane)) = (out_audio, audio_streams.iter().position(|a| a.0 == index)) {
                if ended {
                    mix.end(lane);
                } else if let Some(sample) = sample {
                    read_floats(&sample, &mut floats).map_err(text)?;
                    stereo.clear();
                    to_stereo(&floats, audio_streams[lane].1, &mut stereo);
                    mix.push(lane, frame_of(time), &stereo);
                }
                while let Some(at) = mix.pull(&mut mixed) {
                    pcm.clear();
                    to_i16(&mixed, &mut pcm);
                    let block = pcm_sample(&pcm, hns_of((mixed.len() / 2) as i64)).map_err(text)?;
                    ready.push((out, false, 0, hns_of(at), Some(block)));
                }
                if mix.done() && !audio_done {
                    audio_done = true;
                    for (index, _) in &audio_streams {
                        let _ = reader.SetStreamSelection(*index, false);
                    }
                }
            }
            for (out, is_video, flags, time, sample) in ready {
                match base {
                    Some(start) => write(out, is_video, flags, (time - start).max(0), sample)?,
                    None => {
                        *(if is_video { &mut first_video } else { &mut first_audio }) = Some(time);
                        held.push((out, is_video, flags, time, sample));
                    }
                }
            }
            if base.is_none() {
                let both = first_video.is_some() && (first_audio.is_some() || out_audio.is_none());
                if both || video_done || audio_done || held.len() >= START_HOLD {
                    let start = first_video.into_iter().chain(first_audio).min().unwrap_or(0);
                    base = Some(start);
                    for (out, is_video, flags, time, sample) in held.drain(..) {
                        write(out, is_video, flags, (time - start).max(0), sample)?;
                    }
                }
            }
        }
        drop(write);
        writer.Finalize().map_err(|e| format!("couldn't finish the copy: {e}"))?;
        drop(writer);
        // Index at the front, so the video starts playing before it has all downloaded. Done here
        // rather than with MF_MPEG4SINK_MOOV_BEFORE_MDAT, which on a runner either failed with
        // access denied or, given a read-write stream, scrambled the data it moved.
        tape_core::mp4::faststart::faststart(output).map_err(|e| format!("couldn't finish the copy: {e}"))?;
        Ok((target.width, target.height, length.max(0) as f64 / HUNDRED_NS))
    }
}

/// 16-bit stereo PCM, what the AAC encoder takes.
fn pcm_type(rate: u32) -> windows::core::Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * 4)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        Ok(t)
    }
}

/// A decoded float sample's contents.
fn read_floats(sample: &IMFSample, out: &mut Vec<f32>) -> windows::core::Result<()> {
    out.clear();
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
        buffer.Lock(&mut data, None, Some(&mut len))?;
        if !data.is_null() {
            // The buffer's alignment isn't promised, so the floats are read byte by byte.
            let bytes = std::slice::from_raw_parts(data, len as usize);
            out.extend(bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)));
        }
        buffer.Unlock()?;
    }
    Ok(())
}

/// A block of mixed PCM for the encoder. Its time is set when it's written.
fn pcm_sample(pcm: &[u8], duration: i64) -> windows::core::Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(pcm.len() as u32)?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        std::ptr::copy_nonoverlapping(pcm.as_ptr(), data, pcm.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(pcm.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

/// Real stream indexes of the audio tracks, in file order.
fn audio_indexes(reader: &IMFSourceReader) -> Vec<u32> {
    (0..32)
        .map_while(|i| unsafe { reader.GetNativeMediaType(i, 0) }.ok().map(|t| (i, t)))
        .filter(|(_, t)| unsafe { t.GetGUID(&MF_MT_MAJOR_TYPE) }.ok() == Some(MFMediaType_Audio))
        .map(|(i, _)| i)
        .collect()
}

/// ReadSample reports real stream indexes, not the FIRST_VIDEO_STREAM alias.
fn real_index(reader: &IMFSourceReader, major: &windows::core::GUID) -> u32 {
    (0..32)
        .map_while(|i| unsafe { reader.GetNativeMediaType(i, 0) }.ok().map(|t| (i, t)))
        .find(|(_, t)| unsafe { t.GetGUID(&MF_MT_MAJOR_TYPE) }.ok().as_ref() == Some(major))
        .map_or(u32::MAX, |(i, _)| i)
}
