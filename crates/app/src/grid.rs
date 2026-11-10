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

/// Smallest tile width the responsive grid will use, in CSS pixels.
pub const TILE_MIN_WIDTH: f64 = 220.0;
/// Horizontal gap between tiles.
pub const COLUMN_GAP: f64 = 16.0;
/// Vertical gap between rows.
pub const ROW_GAP: f64 = 16.0;
/// Horizontal padding inside the scroll area.
pub const H_PADDING: f64 = 32.0;
/// Height / width of the capsule art (Steam's 231×87 header art).
pub const ART_RATIO: f64 = 87.0 / 231.0;
/// Everything below the art on a card: title, status, meta line, buttons and
/// the card's own padding. Deliberately generous so a row never clips its
/// contents; the card is drawn at exactly this height so rows stay uniform.
pub const TILE_CHROME: f64 = 132.0;
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
}

impl GridGeometry {
    /// Fit as many [`TILE_MIN_WIDTH`] columns as possible into `viewport_width`,
    /// then grow the tiles to use the remaining space.
    ///
    /// A zero or unknown width falls back to a single column so the first paint
    /// (before the resize observer reports) still shows something.
    pub fn for_width(viewport_width: f64) -> Self {
        let available = (viewport_width - 2.0 * H_PADDING).max(TILE_MIN_WIDTH);
        let columns =
            (((available + COLUMN_GAP) / (TILE_MIN_WIDTH + COLUMN_GAP)).floor() as usize).max(1);
        let tile_width = (available - (columns as f64 - 1.0) * COLUMN_GAP) / columns as f64;
        let row_height = tile_width * ART_RATIO + TILE_CHROME + ROW_GAP;
        Self {
            columns,
            tile_width,
            row_height,
        }
    }

    /// Height of the capsule art on a tile, for the card's `aspect-ratio`.
    pub fn art_height(&self) -> f64 {
        self.tile_width * ART_RATIO
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
}
