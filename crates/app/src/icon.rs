//! The launcher's icon, painted in code.
//!
//! The packaged icon is `packaging/pitagames.svg` (a rounded dark tile with a
//! play triangle). The window and the tray need RGBA pixels, and there is no SVG
//! rasteriser in the dependency set — so the same mark is painted here at
//! whatever size is asked for. Both representations share the geometry and the
//! three colours below; change them together.
//!
//! Painting beats shipping a PNG: no asset bytes, no decoder, and a test can
//! assert what the pixels are.

/// The size used for the window and tray icons.
pub const DEFAULT_SIZE: u32 = 64;

/// Tile gradient, top and bottom (matches the SVG's `#2e3a56` → `#1a1f2e`).
const TILE_TOP: [u8; 3] = [0x2e, 0x3a, 0x56];
const TILE_BOTTOM: [u8; 3] = [0x1a, 0x1f, 0x2e];

/// The play triangle (`#8fb0e8`).
const ACCENT: [u8; 3] = [0x8f, 0xb0, 0xe8];

/// The mark's geometry in the SVG's 256-unit space.
const TRIANGLE: [(f64, f64); 3] = [(100.0, 74.0), (100.0, 182.0), (186.0, 128.0)];
const CORNER_RADIUS: f64 = 52.0;
const INSET: f64 = 8.0;

/// RGBA8 pixels, row-major, `size × size`, transparent outside the rounded tile.
pub fn rgba(size: u32) -> Vec<u8> {
    let side = size as f64;
    let scale = side / 256.0;
    let inset = INSET * scale;
    let radius = CORNER_RADIUS * scale;

    let left = inset;
    let top = inset;
    let right = side - inset;
    let bottom = side - inset;

    let triangle = TRIANGLE.map(|(x, y)| (x * scale, y * scale));

    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        let py = y as f64 + 0.5;
        for x in 0..size {
            let px = x as f64 + 0.5;

            if !inside_rounded_rect(px, py, left, top, right, bottom, radius) {
                pixels.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }

            let color = if inside_triangle(px, py, triangle) {
                ACCENT
            } else {
                let t = ((py - top) / (bottom - top)).clamp(0.0, 1.0);
                lerp(TILE_TOP, TILE_BOTTOM, t)
            };
            pixels.extend_from_slice(&[color[0], color[1], color[2], 0xff]);
        }
    }

    pixels
}

/// The icon as tao's window icon, or `None` if the buffer is refused.
pub fn window_icon(size: u32) -> Option<dioxus::desktop::tao::window::Icon> {
    dioxus::desktop::tao::window::Icon::from_rgba(rgba(size), size, size).ok()
}

/// The icon as the tray's icon type.
pub fn tray_icon(size: u32) -> Option<dioxus::desktop::trayicon::Icon> {
    dioxus::desktop::trayicon::Icon::from_rgba(rgba(size), size, size).ok()
}

fn lerp(from: [u8; 3], to: [u8; 3], t: f64) -> [u8; 3] {
    let mix = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
    [
        mix(from[0], to[0]),
        mix(from[1], to[1]),
        mix(from[2], to[2]),
    ]
}

/// Inside the axis-aligned rectangle with rounded corners.
fn inside_rounded_rect(
    x: f64,
    y: f64,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    radius: f64,
) -> bool {
    if x < left || x > right || y < top || y > bottom {
        return false;
    }

    // The corner circles' centres, pulled in by the radius on each side.
    let nearest_x = x.clamp(left + radius, right - radius);
    let nearest_y = y.clamp(top + radius, bottom - radius);
    let dx = x - nearest_x;
    let dy = y - nearest_y;

    dx * dx + dy * dy <= radius * radius
}

/// Point-in-triangle by three half-plane tests (either winding).
fn inside_triangle(x: f64, y: f64, vertices: [(f64, f64); 3]) -> bool {
    let sign = |(ax, ay): (f64, f64), (bx, by): (f64, f64), (cx, cy): (f64, f64)| {
        (ax - cx) * (by - cy) - (bx - cx) * (ay - cy)
    };

    let d1 = sign((x, y), vertices[0], vertices[1]);
    let d2 = sign((x, y), vertices[1], vertices[2]);
    let d3 = sign((x, y), vertices[2], vertices[0]);

    let has_negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;

    !(has_negative && has_positive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixels: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let index = ((y * size + x) * 4) as usize;
        [
            pixels[index],
            pixels[index + 1],
            pixels[index + 2],
            pixels[index + 3],
        ]
    }

    #[test]
    fn the_buffer_is_square_rgba() {
        for size in [16, 32, 64] {
            assert_eq!(rgba(size).len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn the_corners_are_transparent() {
        let size = DEFAULT_SIZE;
        let pixels = rgba(size);
        for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
            assert_eq!(pixel(&pixels, size, x, y)[3], 0, "corner ({x}, {y})");
        }
    }

    #[test]
    fn the_tile_is_opaque_above_and_below_the_triangle() {
        let size = DEFAULT_SIZE;
        let pixels = rgba(size);
        // Top centre: tile, above the triangle.
        let top = pixel(&pixels, size, size / 2, 6);
        assert_eq!(top[3], 0xff);
        assert!(top[2] > top[0], "the tile is blue-ish: {top:?}");
        // The tile is darker at the bottom than at the top.
        let bottom = pixel(&pixels, size, 6, size - 6);
        assert_eq!(bottom[3], 0xff);
        assert!(bottom[0] < top[0], "gradient runs dark: {bottom:?}");
    }

    #[test]
    fn the_triangle_is_the_accent_colour() {
        let size = DEFAULT_SIZE;
        let pixels = rgba(size);
        // The centroid of the triangle in the 256-space, scaled.
        let scale = f64::from(size) / 256.0;
        let cx = ((100.0 + 100.0 + 186.0) / 3.0 * scale) as u32;
        let cy = ((74.0 + 182.0 + 128.0) / 3.0 * scale) as u32;
        assert_eq!(
            pixel(&pixels, size, cx, cy),
            [ACCENT[0], ACCENT[1], ACCENT[2], 0xff]
        );
    }

    #[test]
    fn the_same_mark_scales() {
        // A 32 px icon's centre is still the triangle, not the tile.
        let pixels = rgba(32);
        let centre = pixel(&pixels, 32, 16, 16);
        assert_eq!(&centre[..3], &ACCENT);
    }
}
