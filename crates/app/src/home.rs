//! The console home: the Continue hero and the shelves — framework-free and
//! unit-tested.
//!
//! The plan's console home (plan §7, *Console mode*) is a full-bleed **Continue**
//! hero — the game the player last ran — over a stack of horizontally-scrolled
//! shelves: Recently played, Installed, then one row per store. This module owns
//! the *shape* of that screen: which library rows land on which shelf, which
//! cell is the hero, and the focus graph over the ragged layout. The renderer
//! (`crate::home_view`) stays thin, and the arithmetic is testable without a
//! webview, exactly like [`crate::grid`].
//!
//! Shelves are virtualized **horizontally**: a shelf with a thousand owned games
//! must not mount a thousand tiles into the WebKit process. [`shelf_window`]
//! mirrors [`crate::grid::window`] for one row that scrolls sideways, and
//! [`shelf_scroll_into_view`] mirrors [`crate::grid::scroll_into_view`].

use std::cmp::Reverse;

use pitagames_core::Store;
use pitagames_input::{Band, FocusGraph, FocusId};

use crate::library::Library;

/// The width of one shelf tile, before the UI scale, in CSS pixels.
pub const SHELF_TILE_WIDTH: f64 = 240.0;
/// The gap between two shelf tiles.
pub const SHELF_GAP: f64 = 16.0;
/// How many tiles are drawn on either side of a shelf's viewport.
pub const SHELF_OVERSCAN: usize = 3;
/// How many games the "Recently played" shelf keeps before it stops being
/// recent and starts being a second Installed row.
pub const RECENT_LIMIT: usize = 20;

/// Which kind of row a shelf is, so the renderer can style it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShelfKind {
    RecentlyPlayed,
    Installed,
    Store(Store),
}

/// One horizontally-scrolled row of the home.
#[derive(Debug, Clone, PartialEq)]
pub struct Shelf {
    /// The heading printed above the row.
    pub title: String,
    pub kind: ShelfKind,
    /// Indices into [`Library::games`], in the order they draw.
    pub games: Vec<usize>,
}

/// One focusable cell of the home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeCell {
    /// The Continue hero, always row 0.
    Hero,
    /// A tile on `shelf` at position `item`.
    Tile { shelf: usize, item: usize },
}

/// The home's layout: the hero game and the shelves under it.
#[derive(Debug, Clone, PartialEq)]
pub struct Home {
    /// Index into [`Library::games`] the hero shows; `None` on an empty library.
    pub hero: Option<usize>,
    pub shelves: Vec<Shelf>,
    /// The focusable cells in layout order. A cell's id is its index here, so a
    /// [`ScreenFocus`](crate::focus::ScreenFocus) can store it like a grid index.
    pub cells: Vec<HomeCell>,
}

impl Home {
    /// Build the home from a library.
    ///
    /// The hero is the most recently played installed game, then the most
    /// played, then the first installed game, then the first game at all — so
    /// the couch always has something to press Play on. The shelves are
    /// Recently played, Installed, then one per store in product order; empty
    /// rows are dropped rather than drawn as a heading with nothing under it.
    pub fn build(library: &Library) -> Self {
        let hero = hero_index(library);
        let shelves = shelves(library);

        let mut cells = Vec::new();
        if hero.is_some() {
            cells.push(HomeCell::Hero);
            for (shelf, row) in shelves.iter().enumerate() {
                for item in 0..row.games.len() {
                    cells.push(HomeCell::Tile { shelf, item });
                }
            }
        }

        Self {
            hero,
            shelves,
            cells,
        }
    }

    /// Whether the home has anything focusable (an empty library does not).
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The cell a focus id names, if it still exists.
    pub fn cell(&self, id: FocusId) -> Option<HomeCell> {
        self.cells.get(id.get() as usize).copied()
    }

    /// The focus id a cell draws at, if it is in the layout.
    ///
    /// The inverse of [`Home::cell`]. A shelf's tiles are *not* numbered per
    /// shelf — the flat `cells` list is what the graph and the focus signal
    /// store — so the view must ask for the id rather than compare against a
    /// shelf-local index (two shelves would then light up together).
    pub fn id_of(&self, cell: HomeCell) -> Option<FocusId> {
        let index = self.cells.iter().position(|candidate| *candidate == cell)?;
        Some(FocusId::from(index))
    }

    /// The library index a cell draws, for the hero or a shelf tile.
    pub fn game(&self, cell: HomeCell) -> Option<usize> {
        match cell {
            HomeCell::Hero => self.hero,
            HomeCell::Tile { shelf, item } => self.shelves.get(shelf)?.games.get(item).copied(),
        }
    }

    /// The focused cell's shelf and column, when it is a tile.
    ///
    /// The hero has no shelf; a caller scrolls a shelf only for `Some((shelf,
    /// item))`, which is what the vertical scroller needs to leave the hero
    /// alone.
    pub fn tile_of(&self, id: FocusId) -> Option<(usize, usize)> {
        match self.cell(id)? {
            HomeCell::Tile { shelf, item } => Some((shelf, item)),
            HomeCell::Hero => None,
        }
    }

    /// The focus graph over the ragged layout: the hero on row 0, one row per
    /// shelf. The resolver's diagonal fallback and per-axis wrap keep a short
    /// shelf reachable from a long one with no trap.
    ///
    /// The layout is the shared [`FocusGraph::ragged`] builder — the hero is a
    /// one-cell band and each shelf is a single-row band — so the home and the
    /// console library walk the same graph code.
    pub fn graph(&self) -> FocusGraph {
        if self.is_empty() {
            return FocusGraph::new();
        }
        let mut bands = Vec::with_capacity(self.shelves.len() + 1);
        bands.push(Band::new(1, 1)); // the Continue hero
        for shelf in &self.shelves {
            bands.push(Band::new(shelf.games.len(), shelf.games.len().max(1)));
        }
        FocusGraph::ragged(&bands)
    }
}

/// The hero game's index in the library, if there is one.
fn hero_index(library: &Library) -> Option<usize> {
    let installed: Vec<usize> = library
        .games
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.game.installed)
        .map(|(index, _)| index)
        .collect();

    // Newest last-played wins; the lowest index breaks a tie deterministically.
    let newest = installed
        .iter()
        .copied()
        .filter(|index| library.games[*index].game.last_played.is_some())
        .max_by_key(|index| (library.games[*index].game.last_played, Reverse(*index)));
    if newest.is_some() {
        return newest;
    }

    // Then the most played, again preferring the first row on a tie. The
    // fallback chain ends with the first game at all, so a couch always has
    // something to press Play on.
    installed
        .iter()
        .copied()
        .max_by_key(|index| (library.games[*index].playtime(), Reverse(*index)))
        .or_else(|| installed.first().copied())
        .or_else(|| (!library.games.is_empty()).then_some(0))
}

/// The shelves, in draw order.
fn shelves(library: &Library) -> Vec<Shelf> {
    let mut shelves = Vec::new();

    let mut recent: Vec<usize> = library
        .games
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.game.installed && entry.game.last_played.is_some())
        .map(|(index, _)| index)
        .collect();
    recent.sort_by_key(|index| Reverse((library.games[*index].game.last_played, *index)));
    recent.truncate(RECENT_LIMIT);
    if !recent.is_empty() {
        shelves.push(Shelf {
            title: "Recently played".to_string(),
            kind: ShelfKind::RecentlyPlayed,
            games: recent,
        });
    }

    let installed: Vec<usize> = library
        .games
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.game.installed)
        .map(|(index, _)| index)
        .collect();
    if !installed.is_empty() {
        shelves.push(Shelf {
            title: "Installed".to_string(),
            kind: ShelfKind::Installed,
            games: installed,
        });
    }

    for store in [
        Store::Steam,
        Store::Epic,
        Store::Amazon,
        Store::Gog,
        Store::Local,
    ] {
        let games: Vec<usize> = library
            .games
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.store() == store)
            .map(|(index, _)| index)
            .collect();
        if !games.is_empty() {
            shelves.push(Shelf {
                title: store_label(store).to_string(),
                kind: ShelfKind::Store(store),
                games,
            });
        }
    }

    shelves
}

/// The display name of a store shelf.
pub fn store_label(store: Store) -> &'static str {
    match store {
        Store::Steam => "Steam",
        Store::Gog => "GOG",
        Store::Epic => "Epic",
        Store::Amazon => "Amazon",
        Store::Local => "Local",
    }
}

/// The slice of a shelf to draw, and how to place it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShelfWindow {
    /// First drawn tile, inclusive.
    pub item_start: usize,
    /// Last drawn tile, exclusive.
    pub item_end: usize,
    /// `left` of the drawn block inside the sizer, in CSS pixels.
    pub offset_x: f64,
    /// Total width of the sizer, so the horizontal scrollbar stays honest.
    pub total_width: f64,
}

impl ShelfWindow {
    /// The drawable slice of a shelf, or an empty slice when out of range.
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

/// Compute the horizontal window for one shelf.
///
/// `scroll_left` is the shelf's horizontal scroll offset, `viewport_width` its
/// client width and `overscan` how many off-screen tiles to keep on either
/// side. Items come back as a half-open `[item_start, item_end)` range so the
/// caller can slice the shelf directly. A zero or unknown width draws only the
/// first `overscan` tiles, which is enough for the first paint.
pub fn shelf_window(
    item_count: usize,
    tile_width: f64,
    gap: f64,
    scroll_left: f64,
    viewport_width: f64,
    overscan: usize,
) -> ShelfWindow {
    let pitch = tile_width + gap;
    let total_width = if item_count == 0 {
        0.0
    } else {
        item_count as f64 * tile_width + (item_count as f64 - 1.0) * gap
    };

    if item_count == 0 || tile_width <= 0.0 {
        return ShelfWindow {
            item_start: 0,
            item_end: 0,
            offset_x: 0.0,
            total_width,
        };
    }

    let scroll_left = scroll_left.max(0.0);
    let viewport_width = viewport_width.max(0.0);

    let first_visible = (scroll_left / pitch).floor() as usize;
    let last_visible = ((scroll_left + viewport_width) / pitch).ceil() as usize;

    let item_start = first_visible.saturating_sub(overscan).min(item_count - 1);
    let item_end = (last_visible + overscan).min(item_count).max(item_start);

    ShelfWindow {
        item_start,
        item_end,
        offset_x: item_start as f64 * pitch,
        total_width,
    }
}

/// The horizontal scroll that keeps tile `index` inside the shelf.
///
/// The mirror of [`crate::grid::scroll_into_view`]: it moves as little as
/// possible, so walking along a shelf does not jiggle the row, and `margin`
/// keeps the focus ring off the edge. Without a measured viewport it returns the
/// offset unchanged; a negative offset is clamped to the start.
pub fn shelf_scroll_into_view(
    scroll_left: f64,
    viewport_width: f64,
    tile_width: f64,
    gap: f64,
    index: usize,
    margin: f64,
) -> f64 {
    let scroll_left = scroll_left.max(0.0);
    let pitch = tile_width + gap;
    if tile_width <= 0.0 || viewport_width <= 0.0 {
        return scroll_left;
    }
    let margin = margin.max(0.0).min(viewport_width / 2.0);
    let left = index as f64 * pitch;
    let right = left + tile_width;
    if left - margin < scroll_left {
        (left - margin).max(0.0)
    } else if right + margin > scroll_left + viewport_width {
        (right + margin - viewport_width).max(0.0)
    } else {
        scroll_left
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    use pitagames_core::{Game, GameId};
    use pitagames_stores::InstallPhase;

    use crate::library::GameEntry;

    fn entry(store: Store, id: &str, title: &str, installed: bool) -> GameEntry {
        let mut game = Game::new(GameId::new(store, id), title);
        game.installed = installed;
        GameEntry {
            game,
            phase: if installed {
                InstallPhase::Installed
            } else {
                InstallPhase::NotInstalled
            },
            metadata: None,
            owned: false,
        }
    }

    fn library() -> Library {
        let mut dota = entry(Store::Steam, "570", "Dota 2", true);
        dota.game.playtime = Duration::from_secs(120 * 3600);
        dota.game.last_played =
            Some(time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap());

        let mut portal = entry(Store::Steam, "620", "portal", true);
        portal.game.last_played =
            Some(time::OffsetDateTime::from_unix_timestamp(1_700_100_000).unwrap());

        let gog = entry(Store::Gog, "1207658930", "A GOG Game", true);

        let mut epic = entry(Store::Epic, "Gizmo", "Gizmo Deluxe", false);
        epic.owned = true;

        Library {
            games: vec![
                entry(Store::Steam, "440", "Half-Life", false),
                dota,
                portal,
                gog,
                epic,
            ],
            warnings: Vec::new(),
            metadata_warnings: Vec::new(),
            metadata_pending: false,
            steam_root: None,
            libraries: Vec::<PathBuf>::new(),
            owned: Default::default(),
        }
    }

    #[test]
    fn the_hero_is_the_most_recently_played_game() {
        let library = library();
        let home = Home::build(&library);
        // `portal` was played later than `Dota 2`, even though Dota has more hours.
        assert_eq!(home.hero, Some(2));
        assert_eq!(library.games[2].title(), "portal");
    }

    #[test]
    fn the_hero_falls_back_to_playtime_then_the_first_install() {
        let mut library = library();
        for game in &mut library.games {
            game.game.last_played = None;
        }
        let home = Home::build(&library);
        assert_eq!(home.hero, Some(1), "Dota 2 has the most hours");
        assert_eq!(library.games[1].title(), "Dota 2");

        let mut library = Library::default();
        library
            .games
            .push(entry(Store::Steam, "1", "Only Game", false));
        assert_eq!(Home::build(&library).hero, Some(0), "nothing installed");
    }

    #[test]
    fn an_empty_library_has_an_empty_home() {
        let home = Home::build(&Library::default());
        assert!(home.is_empty());
        assert_eq!(home.hero, None);
        assert!(home.shelves.is_empty());
        assert!(home.graph().is_empty());
    }

    #[test]
    fn the_shelves_are_recent_installed_then_one_per_store() {
        let library = library();
        let home = Home::build(&library);

        let kinds: Vec<ShelfKind> = home.shelves.iter().map(|shelf| shelf.kind).collect();
        assert_eq!(
            kinds,
            [
                ShelfKind::RecentlyPlayed,
                ShelfKind::Installed,
                ShelfKind::Store(Store::Steam),
                ShelfKind::Store(Store::Epic),
                ShelfKind::Store(Store::Gog),
            ]
        );

        // Recently played is newest first: portal, then Dota 2.
        let recent: Vec<&str> = home.shelves[0]
            .games
            .iter()
            .map(|index| library.games[*index].title())
            .collect();
        assert_eq!(recent, ["portal", "Dota 2"]);

        // Installed holds only the installed games, in library order.
        let installed: Vec<&str> = home.shelves[1]
            .games
            .iter()
            .map(|index| library.games[*index].title())
            .collect();
        assert_eq!(installed, ["Dota 2", "portal", "A GOG Game"]);

        // "Half-Life" and "Gizmo Deluxe" are not installed, so they are absent
        // from Installed but still appear on their own store shelves.
        assert_eq!(
            home.shelves[2].games.len(),
            3,
            "all three Steam rows, installed or not"
        );
        assert_eq!(
            home.shelves[4].games.len(),
            1,
            "the GOG shelf holds its one row"
        );
    }

    #[test]
    fn the_cells_lay_the_hero_over_ragged_shelf_rows() {
        let library = library();
        let home = Home::build(&library);

        // Hero, then one cell per shelf tile in shelf order.
        assert_eq!(home.cells[0], HomeCell::Hero);
        assert_eq!(home.cells[1], HomeCell::Tile { shelf: 0, item: 0 });
        assert_eq!(home.cells[2], HomeCell::Tile { shelf: 0, item: 1 });
        assert_eq!(home.cells[3], HomeCell::Tile { shelf: 1, item: 0 });

        // Every cell maps back to a real game.
        for (index, cell) in home.cells.iter().enumerate() {
            assert!(
                home.game(*cell).is_some(),
                "cell {index} ({cell:?}) names no game"
            );
        }

        // The ids are the indices into `cells`, so `cell` resolves them.
        assert_eq!(
            home.cell(FocusId::from(1usize)),
            Some(HomeCell::Tile { shelf: 0, item: 0 })
        );
        assert_eq!(home.cell(FocusId::from(999usize)), None);

        // `id_of` is the inverse: a shelf's first tile is *not* id 0, so two
        // shelves cannot light the same ring from a shelf-local index.
        assert_eq!(
            home.id_of(HomeCell::Tile { shelf: 0, item: 0 }),
            Some(FocusId::from(1usize))
        );
        assert_eq!(home.id_of(HomeCell::Hero), Some(FocusId::from(0usize)));
        assert_eq!(
            home.id_of(HomeCell::Tile { shelf: 1, item: 0 }),
            Some(FocusId::from(3usize))
        );
        assert_eq!(home.id_of(HomeCell::Tile { shelf: 9, item: 0 }), None);
    }

    #[test]
    fn the_graph_puts_the_hero_on_row_zero_and_each_shelf_below_it() {
        let library = library();
        let home = Home::build(&library);
        let graph = home.graph();

        assert_eq!(graph.len(), home.cells.len());
        let hero = graph.nodes()[0];
        assert_eq!((hero.row, hero.col), (0, 0), "the hero is row 0");

        // The first shelf's tiles are row 1; the item index is the column.
        let first = graph.nodes()[1];
        assert_eq!((first.row, first.col), (1, 0));
    }

    #[test]
    fn a_tile_focus_resolves_its_shelf_and_column() {
        let library = library();
        let home = Home::build(&library);
        assert_eq!(
            home.tile_of(FocusId::from(0usize)),
            None,
            "the hero has no shelf"
        );
        assert_eq!(home.tile_of(FocusId::from(3usize)), Some((1, 0)));
        assert_eq!(home.tile_of(FocusId::from(4usize)), Some((1, 1)));
    }

    #[test]
    fn a_shelf_window_draws_the_visible_tiles_plus_overscan() {
        let window = shelf_window(100, 240.0, 16.0, 0.0, 1_280.0, SHELF_OVERSCAN);
        assert_eq!(window.item_start, 0);
        // 1280 / 256 = 5 visible columns, plus 3 of overscan.
        assert_eq!(window.item_end, 8);
        assert_eq!(window.offset_x, 0.0);
    }

    #[test]
    fn a_scrolled_shelf_offsets_the_drawn_block() {
        let window = shelf_window(
            100,
            240.0,
            16.0,
            256.0 * 10.0 + 4.0,
            1_280.0,
            SHELF_OVERSCAN,
        );
        assert_eq!(window.item_start, 7, "10 minus 3 of overscan");
        // Tiles 10..16 start inside the viewport (tile 15 begins at 3840, just
        // before the 3844 edge), plus 3 of overscan on the right.
        assert_eq!(window.item_end, 19, "tiles 10..16 visible, plus overscan");
        assert_eq!(window.offset_x, 7.0 * 256.0);
    }

    #[test]
    fn a_shelf_window_clamps_at_the_end_and_keeps_the_full_width() {
        let window = shelf_window(6, 240.0, 16.0, 1_000_000.0, 1_280.0, SHELF_OVERSCAN);
        assert_eq!(window.item_end, 6);
        assert!(window.item_start < 6);
        assert_eq!(window.total_width, 6.0 * 240.0 + 5.0 * 16.0);
    }

    #[test]
    fn an_empty_shelf_draws_nothing() {
        let window = shelf_window(0, 240.0, 16.0, 0.0, 1_280.0, SHELF_OVERSCAN);
        assert_eq!(window.item_start, window.item_end);
        assert_eq!(window.total_width, 0.0);
        assert!(window.slice(&[] as &[u8]).is_empty());
    }

    #[test]
    fn a_tile_already_in_view_leaves_the_offset_alone() {
        let scroll = 256.0 * 3.0;
        assert_eq!(
            shelf_scroll_into_view(scroll, 1_280.0, 240.0, 16.0, 4, 36.0),
            scroll
        );
    }

    #[test]
    fn a_tile_past_the_right_edge_scrolls_just_enough() {
        let target = shelf_scroll_into_view(0.0, 1_280.0, 240.0, 16.0, 6, 36.0);
        let right = 6.0 * 256.0 + 240.0;
        assert!((target - (right + 36.0 - 1_280.0)).abs() < 1e-9);
        assert!(target > 0.0);
    }

    #[test]
    fn a_tile_before_the_left_edge_scrolls_back_with_its_margin() {
        let scroll = 256.0 * 10.0;
        let target = shelf_scroll_into_view(scroll, 1_280.0, 240.0, 16.0, 8, 36.0);
        assert!((target - (8.0 * 256.0 - 36.0)).abs() < 1e-9);
        assert!(target < scroll);
    }

    #[test]
    fn scrolling_never_goes_negative_or_moves_on_an_unmeasured_shelf() {
        assert_eq!(
            shelf_scroll_into_view(0.0, 1_280.0, 240.0, 16.0, 0, 36.0),
            0.0
        );
        assert_eq!(
            shelf_scroll_into_view(-40.0, 1_280.0, 240.0, 16.0, 0, 36.0),
            0.0
        );
        assert_eq!(
            shelf_scroll_into_view(24.0, 0.0, 240.0, 16.0, 9, 36.0),
            24.0
        );
    }
}
