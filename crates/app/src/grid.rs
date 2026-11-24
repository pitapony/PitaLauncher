//! Windowed (virtualized) grid geometry — framework-free and unit-tested.
//!
//! The library can hold a thousand owned-but-uninstalled games. Rendering one
//! `<img>` card per game makes the WebKitGTK process allocate a DOM node and a
//! decoded bitmap for every row, which is exactly the jank the plan warns
//! about ("never base64-inline a 500-item grid"). Instead the renderer keeps a
//! single tall sizer and draws only the rows inside the viewport (plus a small
//! overscan), repositioning that window as the user scrolls.
//!
//! This module owns the arithmetic so it can be tested without a webview: given
//! the viewport size, the scroll offset and the item count it returns which
//! items to draw and where the drawn block sits inside the sizer.
//!
//! The layout is per-shell ([`GridGeometry::for_shell`]): the console shell is a
//! 10-foot grid and shows fewer, larger tiles than the windowed desktop grid,
//! from the same geometry model so the two shells cannot drift.

use pitagames_core::ui::Shell;

/// Smallest tile width the windowed (desktop) grid will use, in CSS pixels.
pub const TILE_MIN_WIDTH: f64 = 220.0;
/// Smallest tile width the console (10-foot) grid will use, in CSS pixels.
///
/// Larger than [`TILE_MIN_WIDTH`] so a couch user gets fewer, bigger tiles at
/// 1080p rather than the desktop's dense grid (plan §7, *Console mode*).
pub const TILE_MIN_WIDTH_CONSOLE: f64 = 320.0;
/// Horizontal gap between tiles.
pub const COLUMN_GAP: f64 = 16.0;
/// Vertical gap between rows.
pub const ROW_GAP: f64 = 16.0;
/// Horizontal padding inside the scroll area.
pub const H_PADDING: f64 = 32.0;
/// Height / width of the capsule art (Steam's 231×87 header art).
pub const ART_RATIO: f64 = 87.0 / 231.0;
/// Everything below the art on a desktop card: title, status, meta line,
/// buttons and the card's own padding. Deliberately generous so a row never
/// clips its contents; the card is drawn at exactly this height so rows stay
/// uniform.
pub const TILE_CHROME: f64 = 132.0;
/// Everything below the art on a console card, in CSS pixels.
///
/// Taller than [`TILE_CHROME`]: the console type floor makes the title, the
/// store/install status and the meta line each a line taller, and the buttons
/// carry the console hit floor, so the desktop height would clip them.
pub const TILE_CHROME_CONSOLE: f64 = 176.0;
/// Rows drawn above and below the viewport so a fast scroll does not flash an
/// empty band.
pub const OVERSCAN_ROWS: usize = 2;

/// The responsive layout: how many columns fit and how tall a row is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridGeometry {
    pub columns: usize,
    /// Width of one tile (the art is this wide), in CSS pixels.
    pub tile_width: f64,
    /// Vertical pitch of a row: tile height plus [`ROW_GAP`].
    pub row_height: f64,
    /// Everything below the art on a card, in CSS pixels: the per-shell
    /// [`TILE_CHROME`] / [`TILE_CHROME_CONSOLE`] the card height is built from.
    pub chrome: f64,
}

impl GridGeometry {
    /// Fit as many [`TILE_MIN_WIDTH`] columns as possible into `viewport_width`
    /// (the desktop shell's tile floor).
    ///
    /// A zero or unknown width falls back to a single column so the first paint
    /// (before the resize observer reports) still shows something.
    pub fn for_width(viewport_width: f64) -> Self {
        Self::with_floor(viewport_width, TILE_MIN_WIDTH, TILE_CHROME)
    }

    /// Fit as many tiles as possible, using the shell's own tile floor.
    ///
    /// The console shell is a 10-foot grid: it uses
    /// [`TILE_MIN_WIDTH_CONSOLE`] instead of [`TILE_MIN_WIDTH`], so at 1080p it
    /// shows fewer, larger tiles than the desktop grid. Both shells share this
    /// one model, so the scroll arithmetic cannot drift between them.
    pub fn for_shell(shell: Shell, viewport_width: f64) -> Self {
        if shell.is_console() {
            Self::with_floor(viewport_width, TILE_MIN_WIDTH_CONSOLE, TILE_CHROME_CONSOLE)
        } else {
            Self::for_width(viewport_width)
        }
    }

    /// Fit as many `min_tile_width` columns as possible into `viewport_width`,
    /// then grow the tiles to use the remaining space.
    ///
    /// A zero or unknown width falls back to a single column so the first paint
    /// (before the resize observer reports) still shows something.
    pub fn with_floor(viewport_width: f64, min_tile_width: f64, chrome: f64) -> Self {
        let available = (viewport_width - 2.0 * H_PADDING).max(min_tile_width);
        let columns =
            (((available + COLUMN_GAP) / (min_tile_width + COLUMN_GAP)).floor() as usize).max(1);
        let tile_width = (available - (columns as f64 - 1.0) * COLUMN_GAP) / columns as f64;
        let row_height = tile_width * ART_RATIO + chrome + ROW_GAP;
        Self {
            columns,
            tile_width,
            row_height,
            chrome,
        }
    }

    /// Height of the capsule art on a tile, for the card's `aspect-ratio`.
    pub fn art_height(&self) -> f64 {
        self.tile_width * ART_RATIO
    }

    /// The row a linear item index occupies.
    pub fn row_of(&self, index: usize) -> usize {
        index / self.columns.max(1)
    }
}

/// The slice of items to draw, and the geometry to place it with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridWindow {
    /// First drawn row, inclusive.
    pub first_row: usize,
    /// Last drawn row, exclusive.
    pub last_row: usize,
    /// `top` of the drawn block inside the sizer, in CSS pixels.
    pub offset_y: f64,
    /// Total height of the sizer, so the scrollbar reflects every row.
    pub total_height: f64,
    /// First item drawn, inclusive.
    pub item_start: usize,
    /// Last item drawn, exclusive.
    pub item_end: usize,
}

impl GridWindow {
    /// Whether the window draws anything.
    pub fn is_empty(&self) -> bool {
        self.item_start >= self.item_end
    }

    /// The drawable slice of a list, or an empty slice when out of range.
    pub fn slice<'a, T>(&self, items: &'a [T]) -> &'a [T] {
        let start = self.item_start.min(items.len());
        let end = self.item_end.min(items.len());
        if start >= end {
            &[]
        } else {
            &items[start..end]
        }
    }
}

/// Compute the window for `item_count` items laid out by `geometry`.
///
/// `scroll_top` is the scroll container's offset (negative values are clamped,
/// as WebKit reports them during rubber-banding) and `viewport_height` its
/// client height. Items come back as a half-open `[item_start, item_end)` range
/// so the caller can slice the filtered list directly.
pub fn window(
    item_count: usize,
    geometry: GridGeometry,
    scroll_top: f64,
    viewport_height: f64,
    overscan: usize,
) -> GridWindow {
    let columns = geometry.columns.max(1);
    let row_height = geometry.row_height;
    let total_rows = item_count.div_ceil(columns);
    let total_height = total_rows as f64 * row_height;

    if total_rows == 0 || row_height <= 0.0 {
        return GridWindow {
            first_row: 0,
            last_row: 0,
            offset_y: 0.0,
            total_height,
            item_start: 0,
            item_end: 0,
        };
    }

    let scroll_top = scroll_top.max(0.0);
    let viewport_height = viewport_height.max(0.0);

    // Rows overlapping [scroll_top, scroll_top + viewport_height).
    let first_visible = (scroll_top / row_height).floor() as usize;
    let last_visible = ((scroll_top + viewport_height) / row_height).ceil() as usize;

    let first_row = first_visible.saturating_sub(overscan).min(total_rows - 1);
    let last_row = (last_visible + overscan).min(total_rows).max(first_row);

    let item_start = first_row * columns;
    let item_end = (last_row * columns).min(item_count);

    GridWindow {
        first_row,
        last_row,
        offset_y: first_row as f64 * row_height,
        total_height,
        item_start,
        item_end,
    }
}

/// Scroll offset that keeps `row` inside the viewport.
///
/// The focus graph moves by cells, but only a window of rows is drawn; the
/// caller applies the returned offset to the scroll container so the focused
/// tile exists on the next render. The offset moves as little as possible: a
/// row already fully visible leaves it untouched, so moving along a row never
/// jiggles the page. `margin` is breathing room above and below (the focus
/// ring's outline must not be clipped); the result is clamped at zero, matching
/// WebKit's rubber-banding clamp in [`window`].
pub fn scroll_into_view(
    scroll_top: f64,
    viewport_height: f64,
    geometry: GridGeometry,
    row: usize,
    margin: f64,
) -> f64 {
    let scroll_top = scroll_top.max(0.0);
    let row_height = geometry.row_height;
    if row_height <= 0.0 || viewport_height <= 0.0 {
        return scroll_top;
    }
    let margin = margin.max(0.0).min(viewport_height / 2.0);
    let top = row as f64 * row_height;
    // The row pitch includes the gap below it; the card ends before that.
    let bottom = top + row_height - ROW_GAP;
    if top - margin < scroll_top {
        (top - margin).max(0.0)
    } else if bottom + margin > scroll_top + viewport_height {
        (bottom + margin - viewport_height).max(0.0)
    } else {
        scroll_top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> GridGeometry {
        GridGeometry::for_width(1280.0)
    }

    #[test]
    fn columns_grow_with_the_viewport_and_never_bottom_out_at_zero() {
        assert_eq!(GridGeometry::for_width(0.0).columns, 1);
        assert_eq!(GridGeometry::for_width(200.0).columns, 1);
        assert_eq!(GridGeometry::for_width(220.0 + 64.0).columns, 1);

        let two = GridGeometry::for_width(2.0 * 220.0 + COLUMN_GAP + 64.0);
        assert_eq!(two.columns, 2);
        assert_eq!(GridGeometry::for_width(1280.0).columns, 5);
    }

    #[test]
    fn tiles_fill_the_available_width() {
        let geometry = geometry();
        let used = geometry.tile_width * geometry.columns as f64
            + COLUMN_GAP * (geometry.columns as f64 - 1.0);
        assert!(
            (used - (1280.0 - 2.0 * H_PADDING)).abs() < 1e-6,
            "columns + gaps cover the content width, got {used}"
        );
        assert!(geometry.art_height() > 0.0);
        assert!(geometry.row_height > geometry.art_height());
    }

    #[test]
    fn fewer_columns_means_wider_taller_tiles() {
        let narrow = GridGeometry::for_width(960.0);
        let wide = GridGeometry::for_width(1920.0);
        assert!(narrow.columns < wide.columns);
        assert!(narrow.tile_width > wide.tile_width);
        assert!(narrow.row_height > wide.row_height);
    }

    #[test]
    fn for_width_is_the_desktop_shell() {
        // The windowed grid keeps its exact behaviour; the console shell is a
        // separate floor, not a change to the desktop one.
        assert_eq!(
            GridGeometry::for_width(1280.0),
            GridGeometry::for_shell(Shell::Desktop, 1280.0)
        );
        assert_eq!(
            GridGeometry::for_width(0.0).columns,
            GridGeometry::for_shell(Shell::Desktop, 0.0).columns
        );
    }

    #[test]
    fn the_console_grid_shows_fewer_larger_tiles_than_the_desktop_at_1080p_and_4k() {
        // The two widths the 10-foot pass validates against: a 1080p and a 4K
        // monitor, in CSS pixels.
        for width in [1920.0, 3840.0] {
            let desktop = GridGeometry::for_shell(Shell::Desktop, width);
            let console = GridGeometry::for_shell(Shell::Console, width);
            assert!(
                console.columns < desktop.columns,
                "at {width}px the console shows {} columns, the desktop {}",
                console.columns,
                desktop.columns
            );
            assert!(
                console.tile_width > desktop.tile_width,
                "at {width}px the console tile ({}) is not larger than the desktop's ({})",
                console.tile_width,
                desktop.tile_width
            );
            // Each shell respects its own floor.
            assert!(desktop.tile_width >= TILE_MIN_WIDTH - 1e-6);
            assert!(console.tile_width >= TILE_MIN_WIDTH_CONSOLE - 1e-6);
            // The console chrome is taller so the console type floor fits.
            assert!(console.chrome > desktop.chrome);
        }
    }

    #[test]
    fn the_per_shell_column_counts_are_worked_examples() {
        // 1920 = 1080p, 3840 = 4K, both minus the 2×32px padding.
        assert_eq!(GridGeometry::for_shell(Shell::Console, 1920.0).columns, 5);
        assert_eq!(GridGeometry::for_shell(Shell::Desktop, 1920.0).columns, 7);
        assert_eq!(GridGeometry::for_shell(Shell::Console, 3840.0).columns, 11);
        assert_eq!(GridGeometry::for_shell(Shell::Desktop, 3840.0).columns, 16);
    }

    #[test]
    fn an_empty_list_has_no_window_and_no_height() {
        let window = window(0, geometry(), 0.0, 600.0, OVERSCAN_ROWS);
        assert!(window.is_empty());
        assert_eq!(window.total_height, 0.0);
        assert_eq!(window.first_row, 0);
        assert_eq!(window.last_row, 0);
    }

    #[test]
    fn the_top_of_the_list_draws_the_first_rows_plus_overscan() {
        let geometry = geometry();
        // 3 rows tall viewport.
        let window = window(200, geometry, 0.0, geometry.row_height * 3.0, OVERSCAN_ROWS);
        assert_eq!(window.first_row, 0, "nothing above the first row");
        assert_eq!(
            window.last_row, 5,
            "3 visible rows plus 2 of overscan below"
        );
        assert_eq!(window.item_start, 0);
        assert_eq!(window.item_end, 5 * geometry.columns);
        assert_eq!(window.offset_y, 0.0);
    }

    #[test]
    fn scrolling_moves_the_window_and_offsets_the_block() {
        let geometry = geometry();
        let scroll_top = geometry.row_height * 10.0 + 5.0; // inside row 10
        let window = window(
            200,
            geometry,
            scroll_top,
            geometry.row_height * 3.0,
            OVERSCAN_ROWS,
        );

        assert_eq!(window.first_row, 8, "10 minus 2 rows of overscan");
        assert_eq!(
            window.last_row, 16,
            "rows 10..14 are visible, plus 2 rows of overscan"
        );
        assert_eq!(window.offset_y, 8.0 * geometry.row_height);
        assert_eq!(window.item_start, 8 * geometry.columns);
    }

    #[test]
    fn the_tail_clamps_and_keeps_the_scrollbar_height() {
        let geometry = geometry();
        let columns = geometry.columns;
        // 200 items → 40 rows at 5 columns; scroll far past the end.
        let total_rows = 200usize.div_ceil(columns);
        let window = window(200, geometry, 1_000_000.0, 900.0, OVERSCAN_ROWS);

        assert_eq!(window.last_row, total_rows, "never past the last row");
        assert!(window.first_row < total_rows);
        assert_eq!(window.item_end, 200);
        assert_eq!(
            window.total_height,
            total_rows as f64 * geometry.row_height,
            "the sizer always covers every row"
        );
    }

    #[test]
    fn a_negative_scroll_is_clamped_to_the_top() {
        let geometry = geometry();
        let top = window(50, geometry, 0.0, 900.0, OVERSCAN_ROWS);
        let rubber = window(50, geometry, -120.0, 900.0, OVERSCAN_ROWS);
        assert_eq!(top, rubber);
    }

    #[test]
    fn the_window_never_draws_more_than_exists() {
        let geometry = geometry();
        // Two rows of items, huge overscan: the slice stops at the last item.
        let count = geometry.columns + 1;
        let window = window(count, geometry, 0.0, 5000.0, 50);
        assert_eq!(window.item_end, count);
        assert_eq!(window.slice(&(0..count).collect::<Vec<_>>()).len(), count);
    }

    #[test]
    fn slice_handles_an_out_of_range_window() {
        let items = vec![1, 2, 3];
        let window = GridWindow {
            first_row: 0,
            last_row: 1,
            offset_y: 0.0,
            total_height: 0.0,
            item_start: 2,
            item_end: 99,
        };
        assert_eq!(window.slice(&items), &[3]);
        assert_eq!(window.last_row - window.first_row, 1);
    }

    #[test]
    fn row_of_is_row_major_and_clamps_the_column_count() {
        let geometry = GridGeometry::for_width(1280.0);
        let columns = geometry.columns;
        assert_eq!(geometry.row_of(0), 0);
        assert_eq!(geometry.row_of(columns), 1);
        assert_eq!(geometry.row_of(columns + 1), 1);
        assert_eq!(geometry.row_of(columns * 3 + 2), 3);

        let single = GridGeometry::for_width(0.0);
        assert_eq!(single.row_of(2), 2);
    }

    #[test]
    fn a_row_already_in_view_leaves_the_offset_alone() {
        let geometry = geometry();
        let viewport = geometry.row_height * 3.0;
        let scroll_top = geometry.row_height * 4.0;
        // Row 5 is fully inside rows 4..7.
        assert_eq!(
            scroll_into_view(scroll_top, viewport, geometry, 5, 8.0),
            scroll_top
        );
    }

    #[test]
    fn a_row_below_the_viewport_scrolls_just_enough_to_show_it() {
        let geometry = geometry();
        let viewport = geometry.row_height * 3.0;
        let margin = 8.0;
        let target = scroll_into_view(0.0, viewport, geometry, 4, margin);
        let bottom = 4.0 * geometry.row_height + geometry.row_height - ROW_GAP;
        assert!((target - (bottom + margin - viewport)).abs() < 1e-9);
        assert!(target > 0.0);
    }

    #[test]
    fn a_row_above_the_viewport_scrolls_up_with_its_margin() {
        let geometry = geometry();
        let viewport = geometry.row_height * 3.0;
        let scroll_top = geometry.row_height * 10.0;
        let target = scroll_into_view(scroll_top, viewport, geometry, 8, 8.0);
        assert!((target - (8.0 * geometry.row_height - 8.0)).abs() < 1e-9);
        assert!(target < scroll_top);
    }

    #[test]
    fn scrolling_never_goes_negative_or_moves_on_an_unmeasured_viewport() {
        let geometry = geometry();
        assert_eq!(scroll_into_view(0.0, 900.0, geometry, 0, 8.0), 0.0);
        assert_eq!(scroll_into_view(-40.0, 900.0, geometry, 0, 8.0), 0.0);
        assert_eq!(scroll_into_view(12.0, 0.0, geometry, 9, 8.0), 12.0);
    }
}
