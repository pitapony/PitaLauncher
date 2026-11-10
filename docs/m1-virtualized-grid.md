# M1 — virtualized library grid

The library grid used to render one card per game. That is fine for a dozen
installed titles and wrong for the M1 library: with owned games merged in, an
account can show a thousand tiles, and the plan is explicit that the webview
must never be handed a 500-item grid ("Grid virtualization is hand-rolled
(windowed rows) — do not rely on `content-visibility` alone in WebKitGTK").

This slice replaces the full render with a windowed one: a full-height sizer
preserves the scrollbar, and only the rows inside the viewport (plus two rows
of overscan) are mounted.

## What ships

### `crates/app/src/grid.rs` — the arithmetic, no Dioxus

Framework-free and unit-testable without a webview.

- `GridGeometry::for_width(viewport_width)` fits as many `TILE_MIN_WIDTH`
  (220 px) columns as the content width allows, then grows the tiles to fill
  the leftovers. It returns `columns`, `tile_width` and `row_height`
  (tile height + `ROW_GAP`). A zero/unknown width falls back to one column so
  the first paint still draws something.
- `window(item_count, geometry, scroll_top, viewport_height, overscan)`
  returns a `GridWindow`:
  - `first_row` / `last_row` (half-open),
  - `offset_y` — the `top` of the drawn block inside the sizer,
  - `total_height` — `total_rows × row_height`, so the scrollbar is accurate,
  - `item_start` / `item_end` — a half-open range into the filtered list.
- `GridWindow::slice(items)` takes that range (clamped) and `row_count()` /
  `is_empty()` help the caller.

Constants (CSS pixels):

| Name | Value | Meaning |
|---|---|---|
| `TILE_MIN_WIDTH` | 220 | smallest tile before a new column |
| `COLUMN_GAP` / `ROW_GAP` | 16 | gaps between tiles/rows |
| `H_PADDING` | 32 | horizontal padding inside the scroll area |
| `ART_RATIO` | 87 / 231 | Steam capsule art height ÷ width |
| `TILE_CHROME` | 132 | space below the art (title, status, meta, buttons, padding) |
| `OVERSCAN_ROWS` | 2 | rows drawn above and below the viewport |

At the default 1280 px window the grid is 5 columns of ≈230 px tiles, so the
row pitch is ≈235 px.

Invariants the tests pin down:

- never draw past `[0, item_count)`;
- `total_height` always covers every row;
- a negative scroll (WebKit rubber-banding) is clamped to the top;
- over-scrolling still draws the last row instead of an empty band;
- a window that extends past the list slices only what exists.

### `crates/app/src/main.rs` — `LibraryGrid`

The grid is now a child component (`LibraryGridProps`) rather than markup
inlined in `App`. It owns three private signals — `scroll_top`,
`viewport_width`, `viewport_height` — so a scroll tick re-renders the grid
alone. `App` keeps the header, nav, subtitle and detail routing but does not
read the scroll signals, so scrolling no longer clones the whole library or
rebuilds the window chrome.

The renderer:

1. borrows the library (`Library::filtered` returns references) and slices the
   filtered list with the computed window;
2. draws a scroll container (`flex:1; min-height:0; overflow-y:auto`) whose
   `onscroll` records `scrollTop` and refreshes `clientWidth`/`clientHeight`;
3. measures the first paint through `onresize` (a WebKit `ResizeObserver`,
   content box — the same box as `clientWidth`/`clientHeight`);
4. lays out a `position:relative` sizer at `total_height` with an absolutely
   positioned grid at `top: offset_y`, using
   `repeat(columns, minmax(0, 1fr))` and the shared gaps/padding;
5. gives every card a fixed `box-sizing:border-box` height so rows stay
   uniform and `offset_y` is exact.

The whole window is `height:100vh; overflow:hidden`; the Detail, Downloads and
Account views sit inside their own `overflow-y:auto` wrapper so they keep
scrolling as before.

## Behaviour kept from the old grid

- Scan/metadata warnings still render above the grid.
- "No installed Steam games found." and `Nothing matches "<query>".` empty
  states still appear; the filter remains a case-insensitive title substring.
- Play/Install/Update buttons, the `in downloads` badge, placeholders and art
  URLs are unchanged.
- Owned-but-uninstalled games still appear with Install.

## Verification

- `grid.rs` carries 10 unit tests (geometry, window clamping, overscan, tail,
  negative scroll, slicing). They have no dependencies and were also run
  standalone with `rustc --edition 2021 --test`.
- `crates/app` builds, is clippy-clean (`-D warnings`), fmt-clean and passes its
  **56** tests in `archlinux:latest` (the 10 `grid` tests included; the previous
  total was 46).
- Xvfb smoke run against a fixture Steam tree with **120** games: 5 columns
  render, the top/middle/bottom screenshots show the correct rows (Games
  1/10/100… at the top, 22–35 mid-scroll, 86–99 at the end), the scrollbar spans
  the whole library, and the tail row is present. Screenshots
  `target/shot-grid-{top,scrolled,bottom}.png`.

## Known limits

- Pointer/wheel scrolling only for now; the controller focus graph
  (`crates/input`, M2.5) will drive the same window by scrolling the container.
- The scroll offset is not restored when leaving and returning to the Library
  view (the component remounts at the top).
- Card height is a fixed estimate (`TILE_CHROME`); a very large UI scale could
  clip the bottom row, which `overflow:hidden` hides rather than spills. The
  console shell's 10-foot type pass (M2.5) revisits the metric.
- One vertical column of scroll; horizontal overflow is hidden.
- Not run against a live Steam library on the RX 9070 / RX 5700 XT yet.
