//! `bindings.rs` is generated from `nvEncodeAPI.h` at nv-codec-headers n12.0.16.1 by
//! `scripts/gen-nvenc.sh`, so struct layouts come from NVIDIA's header rather than by hand.
//! The GUIDs and version macros below are the parts bindgen can't translate.

mod bindings;
pub use bindings::*;

const fn struct_version(version: u32) -> u32 {
    (NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24)) | (version << 16) | (0x7 << 28)
}

pub const API_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);
pub const FUNCTION_LIST_VER: u32 = struct_version(2);
pub const OPEN_SESSION_VER: u32 = struct_version(1);
pub const INITIALIZE_PARAMS_VER: u32 = struct_version(5) | (1 << 31);
pub const CONFIG_VER: u32 = struct_version(8) | (1 << 31);
pub const PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
pub const PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
pub const LOCK_BITSTREAM_VER: u32 = struct_version(2);
pub const CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
pub const REGISTER_RESOURCE_VER: u32 = struct_version(4);
pub const CAPS_PARAM_VER: u32 = struct_version(1);
pub const RC_PARAMS_VER: u32 = struct_version(1);

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> GUID {
    GUID { Data1: data1, Data2: data2, Data3: data3, Data4: data4 }
}

pub const CODEC_H264: GUID = guid(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
pub const CODEC_HEVC: GUID = guid(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
pub const CODEC_AV1: GUID = guid(0x0a352289, 0x0aa7, 0x4759, [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54]);

pub const PROFILE_H264_HIGH: GUID = guid(0xe7cbc309, 0x4f7a, 0x4b89, [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10]);
pub const PROFILE_HEVC_MAIN: GUID = guid(0xb514c39a, 0xb55b, 0x40fa, [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec]);
pub const PROFILE_AV1_MAIN: GUID = guid(0x5f2a39f5, 0xf14e, 0x4f95, [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97]);

pub const PRESET_P1: GUID = guid(0xfc0a8d3e, 0x45f8, 0x4cf8, [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf]);
pub const PRESET_P3: GUID = guid(0x36850110, 0x3a07, 0x441f, [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6]);
pub const PRESET_P5: GUID = guid(0x21c6e6b4, 0x297a, 0x4cba, [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3]);

pub fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every value checked against the macros in the header by `scripts/gen-nvenc.sh`.
    #[test]
    fn versions_match_the_header() {
        assert_eq!(API_VERSION, 12);
        assert_eq!(FUNCTION_LIST_VER, 0x7002_000c);
        assert_eq!(CONFIG_VER, 0xf008_000c);
        assert_eq!(PIC_PARAMS_VER, 0xf006_000c);
    }

    #[test]
    fn struct_sizes_are_those_of_the_x64_abi() {
        use std::mem::size_of;
        assert_eq!(size_of::<GUID>(), 16);
        assert_eq!(size_of::<NV_ENC_LOCK_BITSTREAM>() % 8, 0);
        assert_eq!(size_of::<NV_ENCODE_API_FUNCTION_LIST>() % 8, 0);
    }
}
