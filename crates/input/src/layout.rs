//! The pure responsive-layout arithmetic the library grid shares with the
//! focus graph.
//!
//! The windowed desktop grid and the 10-foot console grid are the same model
//! with different numbers (a tile floor, a padding, a gap), and both feed a
//! [`FocusGraph`](crate::FocusGraph) whose columns come from this fit. The
//! constants live here — framework-free — so a headless test can compute the
//! grid's column count at a resolution without a webview, and so the console
//! library's layout snapshots at 1080p and 4K exercise the same function the
//! grid renders with.

/// The horizontal padding a grid keeps inside its viewport, in CSS pixels.
pub const H_PADDING: f64 = 32.0;
/// The smallest tile width the desktop (windowed) grid uses, in CSS pixels.
pub const TILE_MIN_WIDTH: f64 = 220.0;
/// The smallest tile width the console (10-foot) grid uses, in CSS pixels.
///
/// Larger than [`TILE_MIN_WIDTH`] so a couch user gets fewer, bigger tiles at
/// 1080p rather than the desktop's dense grid (plan §7, *Console mode*).
pub const TILE_MIN_WIDTH_CONSOLE: f64 = 320.0;
/// The horizontal gap between two tiles, in CSS pixels.
pub const COLUMN_GAP: f64 = 16.0;

/// Fit as many `min_width` columns as the `available` content width allows.
///
/// `available` is the viewport width minus the horizontal padding; `min_width`
/// and `gap` are the tile floor and the horizontal gap. The result is at least
/// one column, so an unmeasured or tiny area still draws a single column rather
/// than none — the first paint before the resize observer reports still shows
/// something.
pub fn fit_columns(available: f64, min_width: f64, gap: f64) -> usize {
    if min_width <= 0.0 {
        return 1;
    }
    (((available + gap) / (min_width + gap)).floor() as usize).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fit_never_bottoms_out_at_zero() {
        assert_eq!(fit_columns(0.0, TILE_MIN_WIDTH, COLUMN_GAP), 1);
        assert_eq!(fit_columns(-500.0, TILE_MIN_WIDTH, COLUMN_GAP), 1);
        assert_eq!(
            fit_columns(100.0, 0.0, COLUMN_GAP),
            1,
            "no width, one column"
        );
    }

    #[test]
    fn the_fit_grows_with_the_content_width() {
        // Two tiles need two floors plus one gap.
        let two = 2.0 * TILE_MIN_WIDTH + COLUMN_GAP;
        assert_eq!(fit_columns(two - 1.0, TILE_MIN_WIDTH, COLUMN_GAP), 1);
        assert_eq!(fit_columns(two, TILE_MIN_WIDTH, COLUMN_GAP), 2);
        assert!(fit_columns(4_000.0, TILE_MIN_WIDTH, COLUMN_GAP) > 2);
    }

    #[test]
    fn the_console_floor_fits_fewer_columns_than_the_desktop_one() {
        // A 1080p content area, minus the padding both shells share.
        let available = 1_920.0 - 2.0 * H_PADDING;
        let console = fit_columns(available, TILE_MIN_WIDTH_CONSOLE, COLUMN_GAP);
        let desktop = fit_columns(available, TILE_MIN_WIDTH, COLUMN_GAP);
        assert!(console < desktop, "console {console} vs desktop {desktop}");
    }
}
