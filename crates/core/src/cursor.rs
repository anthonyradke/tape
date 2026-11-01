//! Desktop Duplication leaves the mouse pointer out of captured frames and hands over its shape
//! in one of three legacy formats. These turn each into layers the GPU can blend.

pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// BGRA with straight alpha, drawn with normal alpha blending.
    pub color: Vec<u8>,
    /// White where the pointer inverts what is under it, as text I-beams do. `None` if it never does.
    pub invert: Option<Vec<u8>>,
}

fn layers(width: u32, height: u32, mut pixel: impl FnMut(u32, u32) -> ([u8; 4], bool)) -> CursorImage {
    let mut color = Vec::with_capacity((width * height * 4) as usize);
    let mut invert = vec![0u8; (width * height * 4) as usize];
    let mut any = false;
    for y in 0..height {
        for x in 0..width {
            let (bgra, inv) = pixel(x, y);
            color.extend_from_slice(&bgra);
            if inv {
                any = true;
                invert[((y * width + x) * 4) as usize..][..4].fill(255);
            }
        }
    }
    CursorImage { width, height, color, invert: any.then_some(invert) }
}

/// Two 1-bit masks stacked vertically: AND on top, XOR below. `height` is the doubled height.
pub fn monochrome(width: u32, height: u32, pitch: u32, data: &[u8]) -> Option<CursorImage> {
    let h = height / 2;
    if (pitch * height) as usize > data.len() {
        return None;
    }
    let bit = |x: u32, y: u32| data[(y * pitch + x / 8) as usize] >> (7 - x % 8) & 1;
    Some(layers(width, h, |x, y| match (bit(x, y), bit(x, y + h)) {
        (0, 0) => ([0, 0, 0, 255], false),
        (0, _) => ([255, 255, 255, 255], false),
        (_, 0) => ([0, 0, 0, 0], false),
        _ => ([0, 0, 0, 0], true),
    }))
}

pub fn color(width: u32, height: u32, pitch: u32, data: &[u8]) -> Option<CursorImage> {
    if (pitch * height) as usize > data.len() {
        return None;
    }
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let row = &data[(y * pitch) as usize..][..(width * 4) as usize];
        out.extend_from_slice(row);
    }
    Some(CursorImage { width, height, color: out, invert: None })
}

/// BGRA where alpha is a flag: 0 replaces the pixel, 0xFF XORs it. XOR with a color has no blend
/// equivalent, so any non-black XOR is drawn as a full invert, which is how these pointers look
/// on most backgrounds.
pub fn masked(width: u32, height: u32, pitch: u32, data: &[u8]) -> Option<CursorImage> {
    if (pitch * height) as usize > data.len() {
        return None;
    }
    Some(layers(width, height, |x, y| {
        let p = &data[(y * pitch + x * 4) as usize..][..4];
        if p[3] == 0 {
            ([p[0], p[1], p[2], 255], false)
        } else if p[0] | p[1] | p[2] != 0 {
            ([0, 0, 0, 0], true)
        } else {
            ([0, 0, 0, 0], false)
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome_truth_table() {
        // 4 wide, 1 tall, doubled to 2: AND row then XOR row.
        let data = [0b0011_0000, 0b0101_0000];
        let c = monochrome(4, 2, 1, &data).unwrap();
        assert_eq!((c.width, c.height), (4, 1));
        assert_eq!(&c.color[0..4], [0, 0, 0, 255]); // and 0 xor 0: black
        assert_eq!(&c.color[4..8], [255, 255, 255, 255]); // and 0 xor 1: white
        assert_eq!(&c.color[8..12], [0, 0, 0, 0]); // and 1 xor 0: see-through
        assert_eq!(&c.color[12..16], [0, 0, 0, 0]);
        let inv = c.invert.unwrap();
        assert_eq!(&inv[8..12], [0, 0, 0, 0]);
        assert_eq!(&inv[12..16], [255, 255, 255, 255]); // and 1 xor 1: invert
    }

    #[test]
    fn color_respects_pitch() {
        let data = [1, 2, 3, 4, 9, 9, 9, 9, 5, 6, 7, 8, 9, 9, 9, 9];
        let c = color(1, 2, 8, &data).unwrap();
        assert_eq!(c.color, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(c.invert.is_none());
    }

    #[test]
    fn masked_uses_alpha_as_a_flag() {
        let data = [10, 20, 30, 0, 1, 1, 1, 255, 0, 0, 0, 255];
        let c = masked(3, 1, 12, &data).unwrap();
        assert_eq!(&c.color[0..4], [10, 20, 30, 255]);
        assert_eq!(&c.color[4..8], [0, 0, 0, 0]);
        let inv = c.invert.unwrap();
        assert_eq!(&inv[4..8], [255; 4]);
        assert_eq!(&inv[8..12], [0; 4]);
    }

    #[test]
    fn short_buffers_are_rejected() {
        assert!(monochrome(8, 8, 1, &[0; 3]).is_none());
        assert!(color(8, 8, 32, &[0; 3]).is_none());
        assert!(masked(8, 8, 32, &[0; 3]).is_none());
    }
}
