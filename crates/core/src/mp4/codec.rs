//! Turns encoder output into MP4 samples and codec configuration boxes.

use crate::packet::Codec;

pub struct Parsed {
    pub sample: Vec<u8>,
    /// avcC / hvcC / av1C payload, present when the frame carried parameter sets.
    pub config: Option<Vec<u8>>,
}

pub fn parse_frame(codec: Codec, data: &[u8]) -> Parsed {
    match codec {
        Codec::H264 => parse_h264(data),
        Codec::Hevc => parse_hevc(data),
        Codec::Av1 => parse_av1(data),
    }
}

/// NAL units of an Annex B stream, without start codes.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &s) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&data[s..end]);
        }
    }
    out
}

fn push_nal(sample: &mut Vec<u8>, nal: &[u8]) {
    sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
    sample.extend_from_slice(nal);
}

fn parse_h264(data: &[u8]) -> Parsed {
    let mut sample = Vec::with_capacity(data.len() + 16);
    let (mut sps, mut pps) = (None, None);
    for nal in nal_units(data) {
        match nal[0] & 0x1f {
            7 => sps = Some(nal),
            8 => pps = Some(nal),
            9 => {} // access unit delimiter
            _ => push_nal(&mut sample, nal),
        }
    }
    let config = match (sps, pps) {
        (Some(s), Some(p)) if s.len() >= 4 => Some(avcc(s, p)),
        _ => None,
    };
    Parsed { sample, config }
}

fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut c = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    c.extend_from_slice(sps);
    c.push(1);
    c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    c.extend_from_slice(pps);
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        // 4:2:0 8-bit, no SPS extensions.
        c.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
    }
    c
}

fn parse_hevc(data: &[u8]) -> Parsed {
    let mut sample = Vec::with_capacity(data.len() + 16);
    let (mut vps, mut sps, mut pps) = (None, None, None);
    for nal in nal_units(data) {
        match (nal[0] >> 1) & 0x3f {
            32 => vps = Some(nal),
            33 => sps = Some(nal),
            34 => pps = Some(nal),
            35 => {} // access unit delimiter
            _ => push_nal(&mut sample, nal),
        }
    }
    let config = match (vps, sps, pps) {
        (Some(v), Some(s), Some(p)) => hvcc(v, s, p),
        _ => None,
    };
    Parsed { sample, config }
}

/// Removes emulation prevention bytes.
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1 << zeros) - 1 + self.bits(zeros)?)
    }
}

fn hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    let r = rbsp(sps);
    let mut b = Bits { data: r.get(2..)?, pos: 0 };
    b.skip(4); // sps_video_parameter_set_id
    let sub_layers = b.bits(3)?;
    let nesting = b.bit()?;
    // general_profile_tier_level is 12 bytes and starts byte aligned here.
    let ptl_at = b.pos / 8;
    let ptl = b.data.get(ptl_at..ptl_at + 12)?.to_vec();
    b.skip(96);
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((b.bit()?, b.bit()?));
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers as usize));
    }
    for (profile, level) in present {
        b.skip(88 * profile as usize + 8 * level as usize);
    }
    b.ue()?; // sps_seq_parameter_set_id
    let chroma = b.ue()?;
    if chroma == 3 {
        b.skip(1);
    }
    b.ue()?; // width
    b.ue()?; // height
    if b.bit()? == 1 {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    let depth_luma = b.ue()?;
    let depth_chroma = b.ue()?;

    let mut c = vec![1];
    c.extend_from_slice(&ptl);
    c.extend_from_slice(&[0xf0, 0x00, 0xfc]);
    c.push(0xfc | chroma as u8);
    c.push(0xf8 | depth_luma as u8);
    c.push(0xf8 | depth_chroma as u8);
    c.extend_from_slice(&[0, 0]); // avgFrameRate
    // constantFrameRate 0, one temporal layer, nesting flag, 4-byte lengths.
    c.push(((sub_layers as u8 + 1) << 3) | ((nesting as u8) << 2) | 3);
    c.push(3);
    for (kind, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        c.push(0x80 | kind);
        c.extend_from_slice(&1u16.to_be_bytes());
        c.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        c.extend_from_slice(nal);
    }
    Some(c)
}

fn leb128(data: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for i in 0..8 {
        let b = *data.get(i)?;
        value |= ((b & 0x7f) as usize) << (i * 7);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

fn parse_av1(data: &[u8]) -> Parsed {
    const SEQUENCE_HEADER: u8 = 1;
    const TEMPORAL_DELIMITER: u8 = 2;
    let mut sample = Vec::with_capacity(data.len());
    let mut config = None;
    let mut i = 0;
    while i < data.len() {
        let header = data[i];
        let kind = (header >> 3) & 0xf;
        let ext = (header >> 2) & 1 == 1;
        let sized = (header >> 1) & 1 == 1;
        let head = 1 + ext as usize;
        let Some((size, size_len)) = (if sized {
            data.get(i + head..).and_then(leb128)
        } else {
            Some((data.len().saturating_sub(i + head), 0))
        }) else {
            break;
        };
        let body = i + head + size_len;
        let end = body + size;
        if end > data.len() {
            break;
        }
        if kind == SEQUENCE_HEADER {
            config = av1c(&data[i..end], &data[body..end]);
        }
        if kind != TEMPORAL_DELIMITER {
            sample.extend_from_slice(&data[i..end]);
        }
        i = end;
    }
    Parsed { sample, config }
}

/// NVENC writes 8-bit 4:2:0 Main profile, so the color fields are fixed and only the profile,
/// level and tier need reading from the sequence header.
fn av1c(obu: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    let mut b = Bits { data: body, pos: 0 };
    let profile = b.bits(3)?;
    b.skip(1); // still_picture
    let reduced = b.bit()?;
    let (level, tier);
    if reduced == 1 {
        level = b.bits(5)?;
        tier = 0;
    } else {
        let mut decoder_model = false;
        let mut buffer_delay_bits = 0;
        if b.bit()? == 1 {
            // timing_info
            b.skip(64);
            if b.bit()? == 1 {
                // num_ticks_per_picture_minus_1, uvlc: leading zeros, a one, then that many bits.
                let mut zeros = 0;
                while b.bit()? == 0 {
                    zeros += 1;
                }
                if zeros < 32 {
                    b.skip(zeros);
                }
            }
            decoder_model = b.bit()? == 1;
            if decoder_model {
                buffer_delay_bits = b.bits(5)? + 1;
                b.skip(32 + 5 + 5);
            }
        }
        let display_delay = b.bit()? == 1;
        let points = b.bits(5)? + 1;
        let (mut first_level, mut first_tier) = (0, 0);
        for i in 0..points {
            b.skip(12);
            let l = b.bits(5)?;
            let t = if l > 7 { b.bit()? } else { 0 };
            if i == 0 {
                first_level = l;
                first_tier = t;
            }
            if decoder_model && b.bit()? == 1 {
                b.skip(2 * buffer_delay_bits as usize + 1);
            }
            if display_delay && b.bit()? == 1 {
                b.skip(4);
            }
        }
        level = first_level;
        tier = first_tier;
    }
    let mut c = vec![
        0x81,
        (profile as u8) << 5 | level as u8,
        (tier as u8) << 7 | 0b0000_1100, // 8-bit, not monochrome, subsampled in x and y
        0,
    ];
    c.extend_from_slice(obu);
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nals_with_both_start_code_lengths() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
        let nals = nal_units(&data);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]);
    }

    #[test]
    fn strips_parameter_sets_and_builds_avcc() {
        let data = [
            0, 0, 1, 9, 0x10, // AUD
            0, 0, 1, 0x67, 100, 0, 40, 0xac, // SPS
            0, 0, 1, 0x68, 0xee, // PPS
            0, 0, 1, 0x65, 0x88, 0x84, // IDR
        ];
        let p = parse_frame(Codec::H264, &data);
        assert_eq!(p.sample, [0, 0, 0, 3, 0x65, 0x88, 0x84]);
        let c = p.config.unwrap();
        assert_eq!(&c[..6], &[1, 100, 0, 40, 0xff, 0xe1]);
    }

    #[test]
    fn frame_without_parameter_sets_has_no_config() {
        let p = parse_frame(Codec::H264, &[0, 0, 1, 0x41, 0x9a]);
        assert!(p.config.is_none());
        assert_eq!(p.sample, [0, 0, 0, 2, 0x41, 0x9a]);
    }

    #[test]
    fn removes_emulation_prevention() {
        assert_eq!(rbsp(&[1, 0, 0, 3, 1, 0, 0, 3, 0]), [1, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn drops_av1_temporal_delimiter() {
        let data = [0x12, 0, 0x32, 2, 0xaa, 0xbb]; // TD, then a frame OBU
        assert_eq!(parse_frame(Codec::Av1, &data).sample, [0x32, 2, 0xaa, 0xbb]);
    }

    /// Packs "0101 1..." (spaces ignored) into bytes, padding the last with zeros.
    fn packed(bits: &str) -> Vec<u8> {
        let bits: Vec<u8> = bits.bytes().filter(|b| *b != b' ').map(|b| b - b'0').collect();
        bits.chunks(8).map(|c| c.iter().enumerate().fold(0, |byte, (i, b)| byte | b << (7 - i))).collect()
    }

    #[test]
    fn av1_level_and_tier_after_timing_info() {
        let body = packed(concat!(
            "000 0 0",  // profile 0, not a still picture, full header
            "1",        // timing_info_present_flag
            "00000000000000000000000000000001", // num_units_in_display_tick
            "00000000000000000000000000111100", // time_scale
            "1 010",    // equal_picture_interval, num_ticks_per_picture_minus_1 = 1 as uvlc
            "0 0",      // no decoder model, no initial display delay
            "00000",    // one operating point
            "000000000000", // operating_point_idc
            "01000 1",  // seq_level_idx 8 (level 4.0), tier 1
            "0000000",
        ));
        let mut obu = vec![0x0a, body.len() as u8];
        obu.extend_from_slice(&body);
        let config = parse_frame(Codec::Av1, &obu).config.unwrap();
        assert_eq!(&config[..4], [0x81, 0x08, 0x8c, 0]);
        assert_eq!(&config[4..], obu);
    }

    #[test]
    fn truncated_input_does_not_panic() {
        for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
            for data in [&[][..], &[0, 0, 1][..], &[0x0a][..], &[0x0a, 0x80, 0x80][..], &[0, 0, 1, 0x42][..]] {
                let _ = parse_frame(codec, data);
            }
        }
    }
}
