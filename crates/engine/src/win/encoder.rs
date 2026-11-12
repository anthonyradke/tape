//! NVENC, called directly. Frames stay on the GPU the whole way, so the only copy into system
//! memory is the finished bitstream.

use std::ffi::c_void;
use std::mem::{transmute, zeroed};
use std::ptr::null_mut;

use nvenc::*;
use tape_core::ipc::CodecLimit;
use tape_core::packet::Codec;
use tape_core::settings::{Preset, Settings};
use windows::core::{s, w, Interface};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use super::gfx::{Gfx, OutputInfo};
use crate::log;

/// How far apart keyframes are. A clip starts on one, so this bounds how much extra a clip holds.
const KEYFRAME_SECONDS: u32 = 2;
const CHROMA_420: u32 = 1;
/// In `NV_ENC_CONFIG` VUI terms: BT.709 primaries, transfer and matrix.
const BT709: u32 = 1;
const VIDEO_FORMAT_UNSPECIFIED: u32 = 5;

type CreateInstance = unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type MaxVersion = unsafe extern "system" fn(*mut u32) -> NVENCSTATUS;

fn codec_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => CODEC_H264,
        Codec::Hevc => CODEC_HEVC,
        Codec::Av1 => CODEC_AV1,
    }
}

struct Library {
    module: HMODULE,
    api: Box<NV_ENCODE_API_FUNCTION_LIST>,
}

impl Library {
    fn load() -> Result<Library, String> {
        unsafe {
            let module = LoadLibraryW(w!("nvEncodeAPI64.dll")).map_err(|e| format!("NVENC isn't available, is the NVIDIA driver installed? ({e})"))?;
            let symbol = |name| GetProcAddress(module, name).ok_or("nvEncodeAPI64.dll is missing an entry point".to_string());
            let max: MaxVersion = transmute(symbol(s!("NvEncodeAPIGetMaxSupportedVersion"))?);
            let create: CreateInstance = transmute(symbol(s!("NvEncodeAPICreateInstance"))?);

            let mut version = 0u32;
            check(max(&mut version), "version query")?;
            let have = (version >> 4, version & 0xf);
            if have < (NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION) {
                return Err(format!(
                    "The NVIDIA driver is too old: it offers NVENC {}.{}, tape needs {NVENCAPI_MAJOR_VERSION}.{NVENCAPI_MINOR_VERSION}. Update the driver.",
                    have.0, have.1
                ));
            }
            let mut api: Box<NV_ENCODE_API_FUNCTION_LIST> = Box::new(zeroed());
            api.version = FUNCTION_LIST_VER;
            check(create(api.as_mut()), "function table")?;
            Ok(Library { module, api })
        }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.module);
        }
    }
}

fn status_name(status: NVENCSTATUS) -> String {
    match status {
        NV_ENC_ERR_NO_ENCODE_DEVICE => "no encoder on this GPU".into(),
        NV_ENC_ERR_UNSUPPORTED_DEVICE => "this GPU isn't supported".into(),
        NV_ENC_ERR_DEVICE_NOT_EXIST => "the GPU went away".into(),
        NV_ENC_ERR_OUT_OF_MEMORY => "out of video memory".into(),
        NV_ENC_ERR_ENCODER_BUSY => "encoder busy".into(),
        NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => "too many encoder sessions in use".into(),
        NV_ENC_ERR_UNSUPPORTED_PARAM | NV_ENC_ERR_INVALID_PARAM => "this GPU doesn't support these settings".into(),
        other => format!("error {other}"),
    }
}

fn check(status: NVENCSTATUS, what: &str) -> Result<(), String> {
    if status == NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(format!("NVENC {what}: {}", status_name(status)))
    }
}

fn open_session(library: &Library, device: &ID3D11Device) -> Result<*mut c_void, String> {
    let mut params: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { zeroed() };
    params.version = OPEN_SESSION_VER;
    params.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    params.device = device.as_raw();
    params.apiVersion = API_VERSION;
    let mut session = null_mut();
    check(
        unsafe { library.api.nvEncOpenEncodeSessionEx.unwrap()(&mut params, &mut session) },
        "session",
    )?;
    Ok(session)
}

fn codecs_of(library: &Library, session: *mut c_void) -> Result<Vec<Codec>, String> {
    let mut count = 0;
    unsafe {
        check(library.api.nvEncGetEncodeGUIDCount.unwrap()(session, &mut count), "codec count")?;
        let mut guids = vec![GUID::default(); count as usize];
        let mut written = 0;
        check(library.api.nvEncGetEncodeGUIDs.unwrap()(session, guids.as_mut_ptr(), count, &mut written), "codec list")?;
        guids.truncate(written as usize);
        Ok([Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .filter(|c| guids.iter().any(|g| same_guid(g, &codec_guid(*c))))
            .collect())
    }
}

fn cap(library: &Library, session: *mut c_void, codec: Codec, which: NV_ENC_CAPS) -> Option<u32> {
    let mut param: NV_ENC_CAPS_PARAM = unsafe { zeroed() };
    param.version = CAPS_PARAM_VER;
    param.capsToQuery = which;
    let mut value = 0;
    let status = unsafe { library.api.nvEncGetEncodeCaps.unwrap()(session, codec_guid(codec), &mut param, &mut value) };
    (status == NV_ENC_SUCCESS && value > 0).then_some(value as u32)
}

/// The largest frame the encoder takes for `codec`, or no limit if the driver doesn't say.
fn max_size_of(library: &Library, session: *mut c_void, codec: Codec) -> (u32, u32) {
    let width = cap(library, session, codec, NV_ENC_CAPS_WIDTH_MAX).unwrap_or(u32::MAX);
    let height = cap(library, session, codec, NV_ENC_CAPS_HEIGHT_MAX).unwrap_or(u32::MAX);
    (width, height)
}

/// Which codecs this GPU can encode and how large, by opening a session and asking.
pub fn supported(output: &OutputInfo) -> Result<Vec<CodecLimit>, String> {
    let library = Library::load()?;
    let gfx = Gfx::new(&output.adapter)?;
    let session = open_session(&library, &gfx.device)?;
    let codecs = codecs_of(&library, session).map(|codecs| {
        codecs
            .into_iter()
            .map(|codec| {
                let (max_width, max_height) = max_size_of(&library, session, codec);
                CodecLimit { codec, max_width, max_height }
            })
            .collect()
    });
    unsafe { library.api.nvEncDestroyEncoder.unwrap()(session) };
    codecs
}

pub struct Encoder {
    library: Library,
    session: *mut c_void,
    bitstream: NV_ENC_OUTPUT_PTR,
    registered: NV_ENC_REGISTERED_PTR,
    width: u32,
    height: u32,
    frame: u32,
    /// What the session was set up with, kept for `set_bitrate`. The config is boxed so the pointer
    /// to it inside `init` stays good.
    config: Box<NV_ENC_CONFIG>,
    init: NV_ENC_INITIALIZE_PARAMS,
}

impl Encoder {
    /// A session that isn't set up yet, so its limits can decide the size it's then set up with.
    pub fn new(gfx: &Gfx) -> Result<Encoder, String> {
        let library = Library::load()?;
        let session = open_session(&library, &gfx.device)?;
        // Plain C structs, for which all zeros is a valid value; `initialize` fills them in.
        let (config, init) = unsafe { (Box::new(zeroed()), zeroed()) };
        Ok(Encoder { library, session, bitstream: null_mut(), registered: null_mut(), width: 0, height: 0, frame: 0, config, init })
    }

    /// The largest frame this session takes for `codec`.
    pub fn max_size(&self, codec: Codec) -> Result<(u32, u32), String> {
        if !codecs_of(&self.library, self.session)?.contains(&codec) {
            return Err(format!("This GPU can't encode {}", super::codec_name(codec)));
        }
        Ok(max_size_of(&self.library, self.session, codec))
    }

    pub fn initialize(&mut self, input: &ID3D11Texture2D, settings: &Settings, width: u32, height: u32) -> Result<(), String> {
        (self.width, self.height) = (width, height);
        let api = &self.library.api;
        let codec = settings.codec;
        if !codecs_of(&self.library, self.session)?.contains(&codec) {
            return Err(format!("This GPU can't encode {}", super::codec_name(codec)));
        }
        let guid = codec_guid(codec);
        let preset = match settings.preset {
            Preset::Fast => PRESET_P1,
            Preset::Balanced => PRESET_P3,
            Preset::Quality => PRESET_P5,
        };
        unsafe {
            let tuning = NV_ENC_TUNING_INFO_HIGH_QUALITY;
            let mut preset_config: NV_ENC_PRESET_CONFIG = zeroed();
            preset_config.version = PRESET_CONFIG_VER;
            preset_config.presetCfg.version = CONFIG_VER;
            check(
                api.nvEncGetEncodePresetConfigEx.unwrap()(self.session, guid, preset, tuning, &mut preset_config),
                "preset",
            )?;
            let mut config = preset_config.presetCfg;
            config.version = CONFIG_VER;
            config.profileGUID = match codec {
                Codec::H264 => PROFILE_H264_HIGH,
                Codec::Hevc => PROFILE_HEVC_MAIN,
                Codec::Av1 => PROFILE_AV1_MAIN,
            };
            let gop = settings.fps * KEYFRAME_SECONDS;
            config.gopLength = gop;
            // No B-frames and no lookahead: every frame goes in and comes straight back out,
            // decode order equals display order, and the encoder holds no frames across calls.
            config.frameIntervalP = 1;

            let rc = &mut config.rcParams;
            rc.version = RC_PARAMS_VER;
            rc.rateControlMode = NV_ENC_PARAMS_RC_VBR;
            rc.averageBitRate = settings.bitrate_mbps * 1_000_000;
            rc.maxBitRate = settings.peak_kbps() * 1000;
            rc.vbvBufferSize = rc.maxBitRate;
            rc.vbvInitialDelay = rc.maxBitRate;
            rc.multiPass = NV_ENC_MULTI_PASS_DISABLED;
            rc.lookaheadDepth = 0;
            rc.set_enableLookahead(0);
            rc.set_enableAQ(1);

            let idr = gop;
            match codec {
                Codec::H264 => {
                    let c = &mut config.encodeCodecConfig.h264Config;
                    c.level = NV_ENC_LEVEL_AUTOSELECT;
                    c.idrPeriod = idr;
                    c.useBFramesAsRef = NV_ENC_BFRAME_REF_MODE_DISABLED;
                    c.chromaFormatIDC = CHROMA_420;
                    c.set_repeatSPSPPS(1);
                    let vui = &mut c.h264VUIParameters;
                    vui.videoSignalTypePresentFlag = 1;
                    vui.videoFormat = VIDEO_FORMAT_UNSPECIFIED;
                    vui.videoFullRangeFlag = 0;
                    vui.colourDescriptionPresentFlag = 1;
                    vui.colourPrimaries = BT709;
                    vui.transferCharacteristics = BT709;
                    vui.colourMatrix = BT709;
                }
                Codec::Hevc => {
                    let c = &mut config.encodeCodecConfig.hevcConfig;
                    c.level = NV_ENC_LEVEL_AUTOSELECT;
                    c.idrPeriod = idr;
                    c.useBFramesAsRef = NV_ENC_BFRAME_REF_MODE_DISABLED;
                    c.set_chromaFormatIDC(CHROMA_420);
                    c.set_repeatSPSPPS(1);
                    let vui = &mut c.hevcVUIParameters;
                    vui.videoSignalTypePresentFlag = 1;
                    vui.videoFormat = VIDEO_FORMAT_UNSPECIFIED;
                    vui.videoFullRangeFlag = 0;
                    vui.colourDescriptionPresentFlag = 1;
                    vui.colourPrimaries = BT709;
                    vui.transferCharacteristics = BT709;
                    vui.colourMatrix = BT709;
                }
                Codec::Av1 => {
                    let c = &mut config.encodeCodecConfig.av1Config;
                    c.level = NV_ENC_LEVEL_AV1_AUTOSELECT;
                    c.idrPeriod = idr;
                    c.useBFramesAsRef = NV_ENC_BFRAME_REF_MODE_DISABLED;
                    c.set_chromaFormatIDC(CHROMA_420);
                    c.set_repeatSeqHdr(1);
                    c.set_outputAnnexBFormat(0);
                    c.colorPrimaries = BT709;
                    c.transferCharacteristics = BT709;
                    c.matrixCoefficients = BT709;
                    c.colorRange = 0;
                }
            }

            let mut init: NV_ENC_INITIALIZE_PARAMS = zeroed();
            init.version = INITIALIZE_PARAMS_VER;
            init.encodeGUID = guid;
            init.presetGUID = preset;
            init.tuningInfo = tuning;
            init.encodeWidth = self.width;
            init.encodeHeight = self.height;
            init.darWidth = self.width;
            init.darHeight = self.height;
            init.maxEncodeWidth = self.width;
            init.maxEncodeHeight = self.height;
            init.frameRateNum = settings.fps;
            init.frameRateDen = 1;
            init.enablePTD = 1;
            init.enableEncodeAsync = 0;
            *self.config = config;
            init.encodeConfig = &mut *self.config;
            check(api.nvEncInitializeEncoder.unwrap()(self.session, &mut init), "initialize")?;
            self.init = init;
            let config = *self.config;
            log!(
                "nvenc: {} {}x{} at {} fps, preset {:?} (P{}), high quality tuning, VBR {} / {} kbps, vbv {} kbit, gop {gop}, no B-frames, API {NVENCAPI_MAJOR_VERSION}.{NVENCAPI_MINOR_VERSION}",
                super::codec_name(codec),
                self.width,
                self.height,
                settings.fps,
                settings.preset,
                match settings.preset {
                    Preset::Fast => 1,
                    Preset::Balanced => 3,
                    Preset::Quality => 5,
                },
                config.rcParams.averageBitRate / 1000,
                config.rcParams.maxBitRate / 1000,
                config.rcParams.vbvBufferSize / 1000,
            );

            let mut output: NV_ENC_CREATE_BITSTREAM_BUFFER = zeroed();
            output.version = CREATE_BITSTREAM_BUFFER_VER;
            check(api.nvEncCreateBitstreamBuffer.unwrap()(self.session, &mut output), "bitstream buffer")?;
            self.bitstream = output.bitstreamBuffer;

            let mut register: NV_ENC_REGISTER_RESOURCE = zeroed();
            register.version = REGISTER_RESOURCE_VER;
            register.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            register.resourceToRegister = input.as_raw();
            register.width = self.width;
            register.height = self.height;
            register.pitch = 0;
            register.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
            register.bufferUsage = NV_ENC_INPUT_IMAGE;
            check(api.nvEncRegisterResource.unwrap()(self.session, &mut register), "register input")?;
            self.registered = register.registeredResource;
        }
        Ok(())
    }

    /// Changes the bitrate of the running session (nvEncReconfigureEncoder), which keeps its
    /// output going with no new session and so no gap. The next frame is an IDR, with the rate
    /// control starting fresh from it.
    pub fn set_bitrate(&mut self, settings: &Settings) -> Result<(), String> {
        let api = &self.library.api;
        let rc = &mut self.config.rcParams;
        rc.averageBitRate = settings.bitrate_mbps * 1_000_000;
        rc.maxBitRate = settings.peak_kbps() * 1000;
        rc.vbvBufferSize = rc.maxBitRate;
        rc.vbvInitialDelay = rc.maxBitRate;
        unsafe {
            let mut params: NV_ENC_RECONFIGURE_PARAMS = zeroed();
            params.version = RECONFIGURE_PARAMS_VER;
            params.reInitEncodeParams = self.init;
            params.reInitEncodeParams.encodeConfig = &mut *self.config;
            params.set_resetEncoder(1);
            params.set_forceIDR(1);
            check(api.nvEncReconfigureEncoder.unwrap()(self.session, &mut params), "reconfigure")
        }
    }

    /// Encodes the registered texture. Blocks until the bitstream is ready.
    pub fn encode(&mut self, timestamp: u64, force_key: bool) -> Result<Vec<u8>, String> {
        let api = &self.library.api;
        unsafe {
            let mut map: NV_ENC_MAP_INPUT_RESOURCE = zeroed();
            map.version = MAP_INPUT_RESOURCE_VER;
            map.registeredResource = self.registered;
            check(api.nvEncMapInputResource.unwrap()(self.session, &mut map), "map input")?;

            let mut picture: NV_ENC_PIC_PARAMS = zeroed();
            picture.version = PIC_PARAMS_VER;
            picture.inputWidth = self.width;
            picture.inputHeight = self.height;
            picture.inputPitch = 0;
            picture.frameIdx = self.frame;
            picture.inputTimeStamp = timestamp;
            picture.inputBuffer = map.mappedResource;
            picture.bufferFmt = map.mappedBufferFmt;
            picture.outputBitstream = self.bitstream;
            picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
            if force_key {
                picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
            }
            self.frame = self.frame.wrapping_add(1);

            let status = api.nvEncEncodePicture.unwrap()(self.session, &mut picture);
            let result = match status {
                NV_ENC_SUCCESS => self.read_output(),
                // Buffering a frame. Without B-frames this shouldn't happen, and it isn't a failure.
                NV_ENC_ERR_NEED_MORE_INPUT => Ok(Vec::new()),
                other => check(other, "encode").map(|_| Vec::new()),
            };
            api.nvEncUnmapInputResource.unwrap()(self.session, map.mappedResource);
            result
        }
    }

    unsafe fn read_output(&self) -> Result<Vec<u8>, String> {
        let api = &self.library.api;
        let mut lock: NV_ENC_LOCK_BITSTREAM = zeroed();
        lock.version = LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream;
        check(api.nvEncLockBitstream.unwrap()(self.session, &mut lock), "read bitstream")?;
        let data = std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, lock.bitstreamSizeInBytes as usize).to_vec();
        check(api.nvEncUnlockBitstream.unwrap()(self.session, self.bitstream), "release bitstream")?;
        Ok(data)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let api = &self.library.api;
        unsafe {
            if !self.registered.is_null() {
                api.nvEncUnregisterResource.unwrap()(self.session, self.registered);
            }
            if !self.bitstream.is_null() {
                api.nvEncDestroyBitstreamBuffer.unwrap()(self.session, self.bitstream);
            }
            api.nvEncDestroyEncoder.unwrap()(self.session);
        }
    }
}
