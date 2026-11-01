//! AAC through the encoder built into Windows, 192 kb/s stereo at 48 kHz. Windows N editions
//! leave it out until the Media Feature Pack is installed.

use std::mem::ManuallyDrop;

use tape_core::packet::{AAC_FRAME, AUDIO_RATE};
use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::backend::AacEncoder;

/// CLSID_AACMFTEncoder, from mfapi.h.
const AAC_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);
const CHANNELS: u32 = 2;
const BYTES_PER_SECOND: u32 = 24_000; // 192 kb/s, one of the encoder's four fixed rates
const AAC_LC_LEVEL_2: u32 = 0x29;
const RAW_AAC: u32 = 0;
const FRAME_100NS: i64 = AAC_FRAME as i64 * 10_000_000 / AUDIO_RATE as i64;

pub struct Aac {
    transform: IMFTransform,
    output_size: u32,
    frames: i64,
}

fn text(e: windows::core::Error) -> String {
    e.to_string()
}

impl Aac {
    pub fn new() -> Result<Aac, String> {
        Self::create().map_err(|e| format!("AAC encoder unavailable: {e}"))
    }

    fn create() -> windows::core::Result<Aac> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
            let transform: IMFTransform = CoCreateInstance(&AAC_ENCODER, None, CLSCTX_INPROC_SERVER)?;

            // This encoder wants its output type set first.
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, BYTES_PER_SECOND)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, RAW_AAC)?;
            output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, AAC_LC_LEVEL_2)?;
            transform.SetOutputType(0, &output, 0)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS)?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, CHANNELS * 2)?;
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * CHANNELS * 2)?;
            transform.SetInputType(0, &input, 0)?;

            let info = transform.GetOutputStreamInfo(0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Aac { transform, output_size: info.cbSize.max(8192), frames: 0 })
        }
    }

    fn feed(&mut self, pcm: &[u8]) -> windows::core::Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(pcm.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), data, pcm.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(pcm.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            // The encoder refuses samples without these. They don't reach the file: packet times
            // come from the mixer's own timeline.
            sample.SetSampleTime(self.frames * FRAME_100NS)?;
            sample.SetSampleDuration(FRAME_100NS)?;
            self.frames += 1;
            self.transform.ProcessInput(0, &sample, 0)
        }
    }

    fn drain(&mut self, out: &mut Vec<Vec<u8>>) -> windows::core::Result<()> {
        loop {
            unsafe {
                let buffer = MFCreateMemoryBuffer(self.output_size)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                let mut outputs = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut outputs, &mut status);
                let [mut output] = outputs;
                ManuallyDrop::drop(&mut output.pEvents);
                let sample = ManuallyDrop::take(&mut output.pSample);
                match result {
                    Ok(()) => {
                        let Some(sample) = sample else { continue };
                        let buffer = sample.ConvertToContiguousBuffer()?;
                        let (mut data, mut length) = (std::ptr::null_mut(), 0);
                        buffer.Lock(&mut data, None, Some(&mut length))?;
                        out.push(std::slice::from_raw_parts(data, length as usize).to_vec());
                        buffer.Unlock()?;
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

impl AacEncoder for Aac {
    fn encode(&mut self, pcm: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        debug_assert_eq!(pcm.len(), AAC_FRAME * 2 * CHANNELS as usize);
        let mut out = Vec::new();
        self.feed(pcm).map_err(text)?;
        self.drain(&mut out).map_err(text)?;
        Ok(out)
    }
}
