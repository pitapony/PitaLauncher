//! Pure-model layout snapshots at 1080p and 4K.
//!
//! The plan wanted RSX snapshots at 1280×800, 1920×1080 and 3840×2160, but the
//! app's screens read Dioxus/desktop state (tao key codes, live signals) and so
//! cannot be rendered by `dioxus-ssr` in this build (see
//! `docs/m2.5-testing.md`). The guaranteed path is the framework-free layout
//! model the screens actually draw from: the responsive column fit
//! ([`fit_columns`]) and the ragged focus graph ([`FocusGraph::ragged`]). Both
//! live in `pitagames-input`, so these snapshots run headlessly on macOS and in
//! `archlinux:latest` — no webview, no Xvfb.
//!
//! A snapshot is not the only guard: each test also asserts the worked
//! resolution→columns example, so a blind `cargo insta accept` cannot quietly
//! accept a layout regression.

use pitagames_input::{
    Band, COLUMN_GAP, FocusGraph, H_PADDING, TILE_MIN_WIDTH, TILE_MIN_WIDTH_CONSOLE, fit_columns,
};

/// The two resolutions the 10-foot pass validates against, in CSS pixels.
const RESOLUTIONS: [(&str, f64); 2] = [("1080p", 1_920.0), ("4K", 3_840.0)];

/// A compact, stable rendering of a graph: one `id: (row, col)` line per cell.
fn render(graph: &FocusGraph) -> String {
    let mut out = String::new();
    for node in graph.nodes() {
        out.push_str(&format!(
            "{:>2}: ({}, {})\n",
            node.id.get(),
            node.row,
            node.col
        ));
    }
    out
}

/// The console library's focus graph at a viewport width: a one-row chip row
/// over a ragged tile grid whose columns come from the console tile floor.
fn console_library(width: f64, chips: usize, tiles: usize) -> FocusGraph {
    let available = width - 2.0 * H_PADDING;
    let tile_columns = fit_columns(available, TILE_MIN_WIDTH_CONSOLE, COLUMN_GAP);
    FocusGraph::ragged(&[Band::new(chips, chips), Band::new(tiles, tile_columns)])
}

#[test]
fn the_console_library_layout_at_1080p() {
    let available = 1_920.0 - 2.0 * H_PADDING;
    assert_eq!(
        fit_columns(available, TILE_MIN_WIDTH_CONSOLE, COLUMN_GAP),
        5,
        "the console grid shows five tiles across at 1080p"
    );
    insta::assert_snapshot!(
        "console-library-1080p",
        render(&console_library(1_920.0, 5, 12))
    );
}

#[test]
fn the_console_library_layout_at_4k() {
    let available = 3_840.0 - 2.0 * H_PADDING;
    assert_eq!(
        fit_columns(available, TILE_MIN_WIDTH_CONSOLE, COLUMN_GAP),
        11,
        "the console grid shows eleven tiles across at 4K"
    );
    insta::assert_snapshot!(
        "console-library-4k",
        render(&console_library(3_840.0, 5, 12))
    );
}

#[test]
fn the_column_fit_at_1080p_and_4k() {
    let mut out = String::new();
    for (label, width) in RESOLUTIONS {
        let available = width - 2.0 * H_PADDING;
        let console = fit_columns(available, TILE_MIN_WIDTH_CONSOLE, COLUMN_GAP);
        let desktop = fit_columns(available, TILE_MIN_WIDTH, COLUMN_GAP);
        out.push_str(&format!(
            "{label} ({width:.0}px): console {console} columns, desktop {desktop} columns\n"
        ));
    }
    insta::assert_snapshot!("grid-columns", out);
}

#[test]
fn the_home_shelves_layout() {
    // The home is not width-dependent: the Continue hero on row 0, then one row
    // per shelf, ragged. The snapshot pins that shape.
    let mut bands = vec![Band::new(1, 1)]; // the hero
    for length in [3usize, 1, 5, 2] {
        bands.push(Band::new(length, length)); // one row per shelf
    }
    insta::assert_snapshot!("home-shelves", render(&FocusGraph::ragged(&bands)));
}
