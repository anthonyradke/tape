//! Desktop Duplication hands over a rotated monitor's image as the panel scans it out, not as the
//! desktop is laid out: a portrait monitor gives a landscape image, and the pointer position comes
//! in desktop coordinates. This is the arithmetic between the two, kept apart from the Windows code
//! so it can be tested anywhere.

/// How far the desktop is turned clockwise from the image Duplication gives, in quarter turns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Turn {
    #[default]
    None,
    Cw90,
    Cw180,
    Cw270,
}

impl Turn {
    pub fn quarters(self) -> u32 {
        self as u32
    }

    pub fn degrees(self) -> u32 {
        self.quarters() * 90
    }

    /// The desktop's size, from the size of the image as captured.
    pub fn desktop_size(self, image: (u32, u32)) -> (u32, u32) {
        match self {
            Turn::None | Turn::Cw180 => image,
            Turn::Cw90 | Turn::Cw270 => (image.1, image.0),
        }
    }

    /// Where a `shape`-sized rectangle whose top left is at `at` on the desktop lands in the
    /// captured image of size `image`, as (left, top, right, bottom). The pointer's shape is upright
    /// on the desktop, so drawn into the image it's turned back the other way; the overlay's shader
    /// does that with the same `quarters`.
    pub fn desktop_rect_in_image(self, at: (i32, i32), shape: (u32, u32), image: (u32, u32)) -> (i32, i32, i32, i32) {
        let (x0, y0) = at;
        let (x1, y1) = (x0 + shape.0 as i32, y0 + shape.1 as i32);
        let (w, h) = (image.0 as i32, image.1 as i32);
        match self {
            Turn::None => (x0, y0, x1, y1),
            // Desktop (x, y) is image (y, h - x) a quarter turn on: the desktop is as wide as the image is tall.
            Turn::Cw90 => (y0, h - x1, y1, h - x0),
            Turn::Cw180 => (w - x1, h - y1, w - x0, h - y0),
            Turn::Cw270 => (w - y1, x0, w - y0, x1),
        }
    }
}

/// The same mapping applied to texture coordinates, which is what the overlay's vertex shader does
/// on the GPU: `corner` is a corner of the rectangle in the image, the result is where in the
/// upright shape it samples.
#[cfg(test)]
fn shape_uv(turn: Turn, corner: (f32, f32)) -> (f32, f32) {
    let (x, y) = corner;
    match turn {
        Turn::None => (x, y),
        Turn::Cw90 => (1.0 - y, x),
        Turn::Cw180 => (1.0 - x, 1.0 - y),
        Turn::Cw270 => (y, 1.0 - x),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Image pixel for a desktop pixel, written the other way round from the code above: rotating
    /// the image clockwise by the turn gives the desktop.
    fn desktop_to_image(turn: Turn, d: (f32, f32), image: (f32, f32)) -> (f32, f32) {
        let (w, h) = image;
        match turn {
            Turn::None => d,
            Turn::Cw90 => (d.1, h - d.0),
            Turn::Cw180 => (w - d.0, h - d.1),
            Turn::Cw270 => (w - d.1, d.0),
        }
    }

    const ALL: [Turn; 4] = [Turn::None, Turn::Cw90, Turn::Cw180, Turn::Cw270];

    #[test]
    fn a_portrait_monitor_captures_landscape() {
        assert_eq!(Turn::Cw90.desktop_size((2560, 1440)), (1440, 2560));
        assert_eq!(Turn::Cw180.desktop_size((2560, 1440)), (2560, 1440));
        assert_eq!(Turn::Cw90.degrees(), 90);
    }

    #[test]
    fn the_pointer_lands_where_the_desktop_has_it() {
        // A portrait desktop 1440 wide, 2560 tall; the image is 2560x1440.
        let image = (2560, 1440);
        // Top left of the desktop is the image's bottom left, a quarter turn on.
        assert_eq!(Turn::Cw90.desktop_rect_in_image((0, 0), (32, 48), image), (0, 1440 - 32, 48, 1440));
        // Bottom right of the desktop is the image's top right.
        assert_eq!(Turn::Cw90.desktop_rect_in_image((1440 - 32, 2560 - 48), (32, 48), image), (2560 - 48, 0, 2560, 32));
        assert_eq!(Turn::Cw270.desktop_rect_in_image((0, 0), (32, 48), image), (2560 - 48, 0, 2560, 32));
        assert_eq!(Turn::None.desktop_rect_in_image((10, 20), (32, 48), image), (10, 20, 42, 68));
        assert_eq!(Turn::Cw180.desktop_rect_in_image((0, 0), (32, 48), image), (2560 - 32, 1440 - 48, 2560, 1440));
    }

    #[test]
    fn each_corner_of_the_shape_samples_the_matching_desktop_point() {
        let image = (300.0, 200.0);
        let (at, shape) = ((40.0, 70.0), (20.0, 10.0));
        for turn in ALL {
            let (l, t, r, b) = turn.desktop_rect_in_image((at.0 as i32, at.1 as i32), (shape.0 as u32, shape.1 as u32), (300, 200));
            for corner in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                let uv = shape_uv(turn, corner);
                let desktop = (at.0 + uv.0 * shape.0, at.1 + uv.1 * shape.1);
                let expected = desktop_to_image(turn, desktop, image);
                let drawn = (l as f32 + corner.0 * (r - l) as f32, t as f32 + corner.1 * (b - t) as f32);
                assert_eq!(drawn, expected, "{turn:?} corner {corner:?}");
            }
        }
    }
}
