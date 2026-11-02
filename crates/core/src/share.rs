//! What the copy made for a shared link looks like. Small enough to upload quickly and load on a
//! phone, in a format everything plays.

/// The short edge of a shared copy is at most this, so 4K and 1440p clips come down to 1080p.
pub const MAX_SHORT_EDGE: u32 = 1080;
pub const MAX_FPS: u32 = 60;
/// Bits per pixel per frame. Gives about 10 Mbps at 1080p60, plenty for H.264 at that size.
const BITS_PER_PIXEL: f64 = 0.08;
const MIN_BITRATE: u32 = 2_000_000;
const MAX_BITRATE: u32 = 16_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareTarget {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate: u32,
    /// The source is over `MAX_FPS`, so frames get dropped to reach it.
    pub drop_frames: bool,
}

/// From the clip's size and frame rate. Never upscales, keeps the shape, even dimensions.
pub fn share_size(width: u32, height: u32, fps_num: u32, fps_den: u32) -> ShareTarget {
    let (width, height) = (width.max(2), height.max(2));
    let short = width.min(height);
    let (w, h) = if short > MAX_SHORT_EDGE {
        let scale = |edge: u32| ((edge as u64 * MAX_SHORT_EDGE as u64 + short as u64 / 2) / short as u64) as u32;
        (scale(width), scale(height))
    } else {
        (width, height)
    };
    let (w, h) = (w & !1, h & !1);
    let (fps_num, fps_den) = if fps_num == 0 || fps_den == 0 { (60, 1) } else { (fps_num, fps_den) };
    let drop_frames = fps_num as u64 > MAX_FPS as u64 * fps_den as u64;
    let (fps_num, fps_den) = if drop_frames { (MAX_FPS, 1) } else { (fps_num, fps_den) };
    let fps = fps_num as f64 / fps_den as f64;
    let bitrate = ((w as f64 * h as f64 * fps * BITS_PER_PIXEL) as u32).clamp(MIN_BITRATE, MAX_BITRATE);
    ShareTarget { width: w, height: h, fps_num, fps_den, bitrate, drop_frames }
}

/// Roughly how big the copy of a clip this long will be, for refusing before converting.
pub fn estimated_bytes(target: &ShareTarget, seconds: f64) -> u64 {
    const AUDIO: f64 = 160_000.0;
    ((target.bitrate as f64 + AUDIO) / 8.0 * seconds * 1.1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_1080p60_alone_at_about_10_mbps() {
        let t = share_size(1920, 1080, 60, 1);
        assert_eq!((t.width, t.height, t.fps_num, t.fps_den, t.drop_frames), (1920, 1080, 60, 1, false));
        assert!((9_000_000..11_000_000).contains(&t.bitrate));
    }

    #[test]
    fn brings_4k_and_1440p_down_to_1080p() {
        assert_eq!((share_size(3840, 2160, 60, 1).width, share_size(3840, 2160, 60, 1).height), (1920, 1080));
        assert_eq!((share_size(2560, 1440, 60, 1).width, share_size(2560, 1440, 60, 1).height), (1920, 1080));
        // Ultrawide keeps its shape, and a portrait monitor goes by its short edge too.
        assert_eq!((share_size(3440, 1440, 60, 1).width, share_size(3440, 1440, 60, 1).height), (2580, 1080));
        assert_eq!((share_size(1440, 2560, 60, 1).width, share_size(1440, 2560, 60, 1).height), (1080, 1920));
    }

    #[test]
    fn never_upscales_and_keeps_sizes_even() {
        let t = share_size(1279, 721, 30, 1);
        assert_eq!((t.width, t.height), (1278, 720));
        assert_eq!(t.bitrate, MIN_BITRATE.max(t.bitrate));
    }

    #[test]
    fn caps_high_frame_rates_at_60() {
        let t = share_size(1920, 1080, 144, 1);
        assert_eq!((t.fps_num, t.fps_den, t.drop_frames), (60, 1, true));
        let ntsc = share_size(1920, 1080, 60000, 1001);
        assert_eq!((ntsc.fps_num, ntsc.fps_den, ntsc.drop_frames), (60000, 1001, false));
        assert_eq!(share_size(1920, 1080, 0, 0).fps_num, 60);
    }

    #[test]
    fn estimates_about_75_mb_a_minute_at_1080p60() {
        let mb = estimated_bytes(&share_size(1920, 1080, 60, 1), 60.0) / 1_000_000;
        assert!((65..95).contains(&mb), "{mb}");
    }
}
