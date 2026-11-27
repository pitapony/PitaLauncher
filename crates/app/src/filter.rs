//! The library filter: query, store chips, installed/updates toggles and sort —
//! framework-free and unit-tested.
//!
//! The console library (plan §7, *Console mode*) narrows one [`Library`] through
//! a d-pad-driven chip row: a store subset, an Installed toggle, an Updates
//! toggle and a Sort mode, on top of the text query the search box already
//! carried. This module owns that filter value and the pure
//! [`LibraryFilter::apply`] that turns it into the filtered, ordered indices the
//! grid draws — the same result the desktop search box produced, so the two
//! shells cannot drift. The chip row and its focus graph are added by later
//! tickets; the model lands here first.
//!
//! `apply` returns **indices into [`Library::games`]**, not references, so the
//! grid keeps using its windowed geometry over one flat list.

use std::collections::{HashMap, HashSet};

use pitagames_core::{Game, GameId, Store};
use pitagames_input::{Band, FocusGraph, FocusId};
use pitagames_stores::InstallPhase;
use pitagames_stores::status::updates;

use crate::library::{GameEntry, Library};

/// The stores a store chip can name, in the fixed order the row draws them —
/// the same product order the home shelves use.
pub const STORE_ORDER: [Store; 5] = [
    Store::Steam,
    Store::Epic,
    Store::Amazon,
    Store::Gog,
    Store::Local,
];

/// Smallest width one chip needs before the row wraps, in CSS pixels.
pub const CHIP_MIN_WIDTH: f64 = 140.0;

/// How the filtered library is ordered.
///
/// The direction is fixed per mode — Title is A–Z, Playtime most-first, Recent
/// newest-first — so there is no separate direction field that could fall out of
/// step with the Sort chip's label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    /// Case-insensitive title, A–Z.
    Title,
    /// Most played first.
    Playtime,
    /// Most recently played first; never-played games last.
    Recent,
}

impl Sort {
    /// Every mode, in the order the Sort chip cycles them.
    pub const ALL: [Sort; 3] = [Sort::Title, Sort::Playtime, Sort::Recent];

    /// The label the Sort chip shows for this mode.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Playtime => "Playtime",
            Self::Recent => "Recent",
        }
    }

    /// The next mode the Sort chip advances to; the cycle closes.
    pub const fn next(self) -> Self {
        match self {
            Self::Title => Self::Playtime,
            Self::Playtime => Self::Recent,
            Self::Recent => Self::Title,
        }
    }
}

impl Default for Sort {
    fn default() -> Self {
        Sort::ALL[0]
    }
}

/// The library's filter: what to keep and in what order.
///
/// One value lives on [`Navigation`](crate::navigation::Navigation) and is
/// reached through the session handle, so a screen and the header search box
/// read and write the same filter. An empty query, no stores selected, both
/// toggles off and [`Sort::Title`] is the neutral filter that shows the whole
/// library, in the order the scan already sorted it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryFilter {
    /// Case-insensitive title substring; empty keeps everything.
    pub query: String,
    /// Stores to keep; an empty set means every store.
    pub stores: Vec<Store>,
    /// Keep only games whose files are on disk.
    pub installed_only: bool,
    /// Keep only games that need an update or verification (the same list
    /// [`pitagames_stores::status::updates`] reports).
    pub needs_attention: bool,
    /// The order the grid draws in.
    pub sort: Sort,
}

impl LibraryFilter {
    /// Whether the filter hides any game.
    ///
    /// A query, a store subset or a toggle narrows the grid; [`Sort`] only
    /// reorders it, so a sort on its own never empties the grid and is not a
    /// filter to clear.
    pub fn narrows(&self) -> bool {
        !self.query.trim().is_empty()
            || !self.stores.is_empty()
            || self.installed_only
            || self.needs_attention
    }

    /// Reset to the neutral filter that shows the whole library, in scan order.
    ///
    /// `Back` on the Library and the empty grid's "press Back" message both end
    /// here, so there is one definition of "the filters are cleared".
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The filtered, ordered indices of `library.games` this filter keeps.
    ///
    /// Pure: it reads the library and returns indices, so the grid, the focus
    /// count and the Confirm handler all derive from one answer. The order is
    /// always fully determined (every mode breaks ties by title then id), so two
    /// calls cannot disagree.
    pub fn apply(&self, library: &Library) -> Vec<usize> {
        let needle = self.query.trim().to_lowercase();
        let attention = if self.needs_attention {
            Some(attention_ids(library))
        } else {
            None
        };

        let mut indices: Vec<usize> = library
            .games
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.matches(entry, &needle, attention.as_ref()))
            .map(|(index, _)| index)
            .collect();

        self.order(&mut indices, library);
        indices
    }

    /// Whether one entry survives the query, store, installed and updates
    /// filters. (`sort` is an ordering, not a predicate.)
    fn matches(
        &self,
        entry: &GameEntry,
        needle: &str,
        attention: Option<&HashSet<GameId>>,
    ) -> bool {
        if !needle.is_empty() && !entry.title().to_lowercase().contains(needle) {
            return false;
        }
        if !self.stores.is_empty() && !self.stores.contains(&entry.store()) {
            return false;
        }
        if self.installed_only && !entry.game.installed {
            return false;
        }
        if let Some(attention) = attention
            && !attention.contains(&entry.game.id)
        {
            return false;
        }
        true
    }

    /// Sort the kept indices in place, by the active mode.
    fn order(&self, indices: &mut [usize], library: &Library) {
        indices.sort_by(|left, right| {
            let left = &library.games[*left];
            let right = &library.games[*right];
            match self.sort {
                Sort::Title => title_cmp(left, right),
                Sort::Playtime => right
                    .game
                    .playtime
                    .cmp(&left.game.playtime)
                    .then_with(|| title_cmp(left, right)),
                Sort::Recent => right
                    .game
                    .last_played
                    .cmp(&left.game.last_played)
                    .then_with(|| title_cmp(left, right)),
            }
        });
    }
}

/// One control in the filter chip row.
///
/// The row is a fixed order — Installed, Updates, one chip per non-empty store,
/// then Sort — so a screen never has to guess where a chip goes. Each chip
/// knows its own label and what a press does, which keeps the renderer a
/// mapping from the model to buttons and nothing more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    /// Keep only the games whose files are on disk.
    Installed,
    /// Keep only the games that need an update or verification.
    Updates,
    /// Keep only this store; store chips multi-select.
    Store(Store),
    /// Cycle the sort mode.
    Sort,
}

impl Chip {
    /// The label the chip draws; the Sort chip names the mode it is on.
    pub fn label(self, filter: &LibraryFilter) -> String {
        match self {
            Self::Installed => "Installed".to_string(),
            Self::Updates => "Updates".to_string(),
            Self::Store(store) => crate::home::store_label(store).to_string(),
            Self::Sort => format!("Sort: {}", filter.sort.label()),
        }
    }

    /// Whether the chip's filter is on. The Sort chip is a cycle, not a toggle,
    /// so it is never "active".
    pub fn is_active(self, filter: &LibraryFilter) -> bool {
        match self {
            Self::Installed => filter.installed_only,
            Self::Updates => filter.needs_attention,
            Self::Store(store) => filter.stores.contains(&store),
            Self::Sort => false,
        }
    }

    /// Apply one press of this chip to the filter.
    ///
    /// Installed/Updates flip in place; a store chip joins or leaves the
    /// selected set, where an empty set means every store; Sort advances the
    /// mode. Pure, so a chip and a test agree on what a press does.
    pub fn toggle(self, filter: &mut LibraryFilter) {
        match self {
            Self::Installed => filter.installed_only = !filter.installed_only,
            Self::Updates => filter.needs_attention = !filter.needs_attention,
            Self::Store(store) => {
                if let Some(index) = filter.stores.iter().position(|known| *known == store) {
                    filter.stores.remove(index);
                } else {
                    filter.stores.push(store);
                }
            }
            Self::Sort => filter.sort = filter.sort.next(),
        }
    }
}

/// The chips a library shows, in the fixed row order.
///
/// Installed and Updates are always present; then one store chip per store that
/// has at least one row, in [`STORE_ORDER`]; then Sort. A store with no games
/// contributes no chip, so the row never offers a filter that would empty the
/// grid for a store the user does not own anything from.
pub fn chips(library: &Library) -> Vec<Chip> {
    let mut chips = vec![Chip::Installed, Chip::Updates];
    for store in STORE_ORDER {
        if library.games.iter().any(|entry| entry.store() == store) {
            chips.push(Chip::Store(store));
        }
    }
    chips.push(Chip::Sort);
    chips
}

/// What `Back` does on the Library, before it leaves the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackAction {
    /// The filter narrows the grid: clear it and stay put.
    ClearFilters,
    /// Nothing to clear: the caller leaves the screen.
    Leave,
}

/// The Library's `Back` rule: clear a narrowing filter before leaving.
///
/// A couch player who filtered down and pressed `Back` means "undo the filter",
/// not "leave the screen", so the filter is cleared first; a second `Back` (with
/// nothing left to clear) leaves. A sort on its own does not count: it reorders
/// the whole library rather than narrowing it, so `Back` leaves without
/// disturbing the order.
pub fn back_action(filter: &LibraryFilter) -> BackAction {
    if filter.narrows() {
        BackAction::ClearFilters
    } else {
        BackAction::Leave
    }
}

/// The message a filtered-empty grid shows.
///
/// It names the query when there is one and always teaches `Back` to clear, so
/// a couch player is never stuck on a blank grid with no way out.
pub fn empty_message(filter: &LibraryFilter) -> String {
    let needle = filter.query.trim();
    if needle.is_empty() {
        "Nothing matches the current filters. Press Back to clear them.".to_string()
    } else {
        format!("Nothing matches \"{needle}\". Press Back to clear the filters.")
    }
}

/// The filter chip row: its chips and the ragged graph over the chips and the
/// tiles below them.
///
/// The chips are a fixed list, but the row wraps when it overflows, so the row
/// owns the wrap count (`columns`) as well as the chips. The graph is the one
/// place that knows a chip id and a tile id share one id space, so a
/// [`ScreenFocus`](crate::focus::ScreenFocus) can store the flat id like a grid
/// index and the renderer can ask which side a focused id is on.
#[derive(Clone, Debug, PartialEq)]
pub struct ChipRow {
    chips: Vec<Chip>,
    columns: usize,
}

impl ChipRow {
    /// Build the row for a library, wrapping the chips into as many per line as
    /// `available` CSS pixels fit (an unmeasured width puts them all on one).
    pub fn build(library: &Library, available: f64) -> Self {
        let chips = chips(library);
        let columns = chip_columns(chips.len(), available);
        Self {
            chips,
            columns: columns.max(1),
        }
    }

    /// The chips, in row order.
    pub fn chips(&self) -> &[Chip] {
        &self.chips
    }

    /// How many chips fit on one line.
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// The chip a focus id names, when the id is a chip rather than a tile.
    pub fn chip(&self, id: FocusId) -> Option<Chip> {
        self.chip_index(id)
            .and_then(|index| self.chips.get(index).copied())
    }

    /// The chip index a focus id names.
    pub fn chip_index(&self, id: FocusId) -> Option<usize> {
        let index = usize::try_from(id.get()).ok()?;
        (index < self.chips.len()).then_some(index)
    }

    /// The tile index a focus id names (tiles follow the chips in the id space).
    pub fn tile_index(&self, id: FocusId) -> Option<usize> {
        let index = usize::try_from(id.get()).ok()?;
        (index >= self.chips.len()).then(|| index - self.chips.len())
    }

    /// The focus id of a tile by its index into the filtered list.
    pub fn tile_id(&self, index: usize) -> FocusId {
        FocusId::from(self.chips.len() + index)
    }

    /// The ragged graph: the chips on row 0 (wrapping), then the tiles.
    pub fn graph(&self, tile_count: usize, tile_columns: usize) -> FocusGraph {
        ragged_graph(self.chips.len(), self.columns, tile_count, tile_columns)
    }
}

/// Build the ragged chip-over-tiles focus graph.
///
/// The chips are laid out row-major `chip_columns` to a line, so a row that does
/// not fit wraps onto the next; the tiles begin on the row after the last chip
/// row, row-major in `tile_columns`. A chip's id is its index in the row and a
/// tile's id is `chip_count + index`, so `Up` from the top tile row reaches the
/// chips and the two sets never share an id. A zero column count is treated as
/// one column.
///
/// The layout itself is the shared [`FocusGraph::ragged`] builder, so the console
/// library's graph and its property tests cannot drift.
pub fn ragged_graph(
    chip_count: usize,
    chip_columns: usize,
    tile_count: usize,
    tile_columns: usize,
) -> FocusGraph {
    FocusGraph::ragged(&[
        Band::new(chip_count, chip_columns),
        Band::new(tile_count, tile_columns),
    ])
}

/// How many chips fit on one line of `available` CSS pixels.
///
/// An unmeasured (or zero) width puts every chip on one line, so the first paint
/// is not a one-chip-wide column; otherwise the row wraps at [`CHIP_MIN_WIDTH`]
/// per chip and never returns zero.
pub fn chip_columns(chip_count: usize, available: f64) -> usize {
    if chip_count == 0 {
        return 0;
    }
    if available <= 0.0 {
        return chip_count;
    }
    let fit = (available / CHIP_MIN_WIDTH).floor() as usize;
    fit.clamp(1, chip_count)
}

/// Title order, case-insensitively, with the id as a stable tie-break.
fn title_cmp(left: &GameEntry, right: &GameEntry) -> std::cmp::Ordering {
    left.title()
        .to_lowercase()
        .cmp(&right.title().to_lowercase())
        .then_with(|| left.game.id.cmp(&right.game.id))
}

/// The ids [`updates`] reports, so "needs attention" cannot drift from the
/// Updates list the store-status screen shows.
fn attention_ids(library: &Library) -> HashSet<GameId> {
    let games: Vec<Game> = library
        .games
        .iter()
        .filter(|entry| entry.game.installed)
        .map(|entry| entry.game.clone())
        .collect();
    let phases: HashMap<String, InstallPhase> = library
        .games
        .iter()
        .map(|entry| (entry.game.id.store_id.clone(), entry.phase))
        .collect();

    updates(&games, &phases)
        .into_iter()
        .map(|update| update.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pitagames_stores::status::UPDATE_TAG;
    use time::OffsetDateTime;

    use super::*;
    use pitagames_input::Action;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

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

    /// Five games, deliberately stored out of title order so every sort is a
    /// real assertion, spanning four stores, a Steam update and a store flag.
    fn library() -> Library {
        let mut dota = entry(Store::Steam, "570", "Dota 2", true);
        dota.game.playtime = Duration::from_secs(10 * 3600);
        dota.game.last_played = Some(at(200));

        let mut portal = entry(Store::Steam, "620", "Portal", true);
        portal.phase = InstallPhase::UpdateAvailable;
        portal.game.playtime = Duration::from_secs(2 * 3600);
        portal.game.last_played = Some(at(100));

        let gizmo = entry(Store::Epic, "gizmo", "Gizmo Deluxe", false);

        let mut aardvark = entry(Store::Gog, "120", "Aardvark", true);
        aardvark.game.playtime = Duration::from_secs(5 * 3600);
        aardvark.game.tags = vec![UPDATE_TAG.to_string()];

        let mut zelda = entry(Store::Local, "local-1", "Zelda", true);
        zelda.game.playtime = Duration::from_secs(100 * 3600);

        Library {
            games: vec![dota, portal, gizmo, aardvark, zelda],
            ..Library::default()
        }
    }

    /// The titles `filter` keeps, in draw order — the external behaviour.
    fn titles(filter: &LibraryFilter) -> Vec<String> {
        let library = library();
        filter
            .apply(&library)
            .into_iter()
            .map(|index| library.games[index].title().to_string())
            .collect()
    }

    #[test]
    fn the_neutral_filter_keeps_every_game_in_title_order() {
        assert_eq!(
            titles(&LibraryFilter::default()),
            ["Aardvark", "Dota 2", "Gizmo Deluxe", "Portal", "Zelda"],
        );
    }

    #[test]
    fn the_query_narrows_by_case_insensitive_title_substring() {
        let filter = LibraryFilter {
            query: "port".to_string(),
            ..LibraryFilter::default()
        };
        assert_eq!(titles(&filter), ["Portal"]);

        let filter = LibraryFilter {
            query: "  DOTA ".to_string(),
            ..LibraryFilter::default()
        };
        assert_eq!(titles(&filter), ["Dota 2"]);
    }

    #[test]
    fn a_query_that_matches_nothing_keeps_nothing() {
        let filter = LibraryFilter {
            query: "half-life".to_string(),
            ..LibraryFilter::default()
        };
        assert!(titles(&filter).is_empty());
    }

    #[test]
    fn store_chips_keep_only_the_selected_stores() {
        let filter = LibraryFilter {
            stores: vec![Store::Steam],
            ..LibraryFilter::default()
        };
        assert_eq!(titles(&filter), ["Dota 2", "Portal"]);

        let filter = LibraryFilter {
            stores: vec![Store::Gog, Store::Epic],
            ..LibraryFilter::default()
        };
        assert_eq!(titles(&filter), ["Aardvark", "Gizmo Deluxe"]);
    }

    #[test]
    fn installed_only_drops_the_rows_with_no_files() {
        let filter = LibraryFilter {
            installed_only: true,
            ..LibraryFilter::default()
        };
        assert_eq!(titles(&filter), ["Aardvark", "Dota 2", "Portal", "Zelda"]);
    }

    #[test]
    fn needs_attention_keeps_only_the_update_and_verification_rows() {
        let filter = LibraryFilter {
            needs_attention: true,
            ..LibraryFilter::default()
        };
        assert_eq!(titles(&filter), ["Aardvark", "Portal"]);
    }

    #[test]
    fn sort_by_playtime_is_most_first() {
        let filter = LibraryFilter {
            sort: Sort::Playtime,
            ..LibraryFilter::default()
        };
        assert_eq!(
            titles(&filter),
            ["Zelda", "Dota 2", "Aardvark", "Portal", "Gizmo Deluxe"],
        );
    }

    #[test]
    fn sort_by_recent_is_newest_first_and_never_played_last() {
        let filter = LibraryFilter {
            sort: Sort::Recent,
            ..LibraryFilter::default()
        };
        assert_eq!(
            titles(&filter),
            ["Dota 2", "Portal", "Aardvark", "Gizmo Deluxe", "Zelda"],
        );
    }

    #[test]
    fn the_filters_combine() {
        // Epic + GOG only, installed only, most-played first.
        let filter = LibraryFilter {
            stores: vec![Store::Epic, Store::Gog],
            installed_only: true,
            sort: Sort::Playtime,
            ..LibraryFilter::default()
        };
        assert_eq!(titles(&filter), ["Aardvark"]);

        // The same filter plus a query that excludes the survivor.
        let filter = LibraryFilter {
            query: "gizmo".to_string(),
            stores: vec![Store::Epic, Store::Gog],
            installed_only: true,
            ..LibraryFilter::default()
        };
        assert!(titles(&filter).is_empty(), "Gizmo is not installed");

        // Query + store + sort, together.
        let filter = LibraryFilter {
            query: "o".to_string(),
            stores: vec![Store::Steam],
            sort: Sort::Playtime,
            ..LibraryFilter::default()
        };
        assert_eq!(titles(&filter), ["Dota 2", "Portal"]);
    }

    #[test]
    fn the_sort_modes_are_the_closed_set_the_chip_cycles() {
        assert_eq!(Sort::ALL.len(), 3);
        assert_eq!(Sort::default(), Sort::Title);
    }

    // -- The Back rule and the empty state ---------------------------------

    #[test]
    fn only_a_narrowing_filter_counts_as_engaged() {
        assert!(!LibraryFilter::default().narrows());
        // A sort alone reorders the whole library; it hides nothing.
        assert!(
            !LibraryFilter {
                sort: Sort::Playtime,
                ..LibraryFilter::default()
            }
            .narrows()
        );

        for filter in [
            LibraryFilter {
                query: "p".to_string(),
                ..LibraryFilter::default()
            },
            LibraryFilter {
                stores: vec![Store::Steam],
                ..LibraryFilter::default()
            },
            LibraryFilter {
                installed_only: true,
                ..LibraryFilter::default()
            },
            LibraryFilter {
                needs_attention: true,
                ..LibraryFilter::default()
            },
        ] {
            assert!(filter.narrows(), "{filter:?} narrows the grid");
        }
    }

    #[test]
    fn clearing_restores_the_neutral_filter() {
        let mut filter = LibraryFilter {
            query: "port".to_string(),
            stores: vec![Store::Steam],
            installed_only: true,
            needs_attention: true,
            sort: Sort::Recent,
        };
        filter.clear();
        assert_eq!(filter, LibraryFilter::default());
    }

    #[test]
    fn back_clears_a_narrowing_filter_before_it_leaves() {
        assert_eq!(back_action(&LibraryFilter::default()), BackAction::Leave);
        assert_eq!(
            back_action(&LibraryFilter {
                sort: Sort::Recent,
                ..LibraryFilter::default()
            }),
            BackAction::Leave,
            "a sort alone leaves the filter to clear"
        );

        for filter in [
            LibraryFilter {
                query: "portal".to_string(),
                ..LibraryFilter::default()
            },
            LibraryFilter {
                stores: vec![Store::Epic],
                ..LibraryFilter::default()
            },
            LibraryFilter {
                installed_only: true,
                ..LibraryFilter::default()
            },
            LibraryFilter {
                needs_attention: true,
                ..LibraryFilter::default()
            },
        ] {
            assert_eq!(
                back_action(&filter),
                BackAction::ClearFilters,
                "{filter:?} is cleared first"
            );
        }
    }

    #[test]
    fn the_empty_message_names_the_query_and_teaches_back() {
        assert_eq!(
            empty_message(&LibraryFilter::default()),
            "Nothing matches the current filters. Press Back to clear them."
        );
        assert_eq!(
            empty_message(&LibraryFilter {
                query: "half-life".to_string(),
                ..LibraryFilter::default()
            }),
            "Nothing matches \"half-life\". Press Back to clear the filters."
        );
        // A whitespace query reads as no query at all.
        assert_eq!(
            empty_message(&LibraryFilter {
                query: "   ".to_string(),
                ..LibraryFilter::default()
            }),
            "Nothing matches the current filters. Press Back to clear them."
        );
    }

    #[test]
    fn apply_returns_indices_into_the_library() {
        let library = library();
        let filter = LibraryFilter {
            query: "dota".to_string(),
            ..LibraryFilter::default()
        };
        assert_eq!(filter.apply(&library), vec![0], "Dota 2 is the first entry");
    }

    // -- The chip row ------------------------------------------------------

    #[test]
    fn the_chip_row_is_installed_updates_then_the_non_empty_stores_then_sort() {
        // The fixture spans Steam, Epic, GOG and Local, so Amazon has no chip.
        let library = library();
        let row = ChipRow::build(&library, 1_000.0);
        assert_eq!(
            row.chips(),
            &[
                Chip::Installed,
                Chip::Updates,
                Chip::Store(Store::Steam),
                Chip::Store(Store::Epic),
                Chip::Store(Store::Gog),
                Chip::Store(Store::Local),
                Chip::Sort,
            ]
        );
    }

    #[test]
    fn a_store_with_no_games_contributes_no_chip() {
        let mut library = Library::default();
        library
            .games
            .push(entry(Store::Steam, "1", "Only Game", true));
        let row = ChipRow::build(&library, 1_000.0);
        assert_eq!(
            row.chips(),
            &[
                Chip::Installed,
                Chip::Updates,
                Chip::Store(Store::Steam),
                Chip::Sort,
            ],
            "no Epic/Amazon/GOG/Local chip without a row from that store"
        );
    }

    #[test]
    fn the_chip_labels_name_the_sort_mode() {
        let library = library();
        let row = ChipRow::build(&library, 1_000.0);
        let mut filter = LibraryFilter::default();
        let labels: Vec<String> = row.chips().iter().map(|chip| chip.label(&filter)).collect();
        assert_eq!(
            labels,
            [
                "Installed",
                "Updates",
                "Steam",
                "Epic",
                "GOG",
                "Local",
                "Sort: Title",
            ]
        );

        filter.sort = Sort::Playtime;
        assert_eq!(Chip::Sort.label(&filter), "Sort: Playtime");
        filter.sort = Sort::Recent;
        assert_eq!(Chip::Sort.label(&filter), "Sort: Recent");
    }

    #[test]
    fn toggling_installed_and_updates_refilters_the_grid() {
        let mut filter = LibraryFilter::default();
        assert_eq!(titles(&filter).len(), 5, "the neutral filter shows all");

        Chip::Installed.toggle(&mut filter);
        assert!(Chip::Installed.is_active(&filter));
        assert_eq!(titles(&filter), ["Aardvark", "Dota 2", "Portal", "Zelda"]);

        Chip::Installed.toggle(&mut filter);
        assert!(!Chip::Installed.is_active(&filter));
        assert_eq!(titles(&filter).len(), 5);

        Chip::Updates.toggle(&mut filter);
        assert!(Chip::Updates.is_active(&filter));
        assert_eq!(titles(&filter), ["Aardvark", "Portal"]);
    }

    #[test]
    fn store_chips_multiselect_and_none_means_every_store() {
        let mut filter = LibraryFilter::default();

        // None selected means all stores.
        assert!(filter.stores.is_empty());
        assert_eq!(titles(&filter).len(), 5);

        Chip::Store(Store::Epic).toggle(&mut filter);
        assert_eq!(titles(&filter), ["Gizmo Deluxe"]);

        // A second store joins the selection.
        Chip::Store(Store::Gog).toggle(&mut filter);
        assert_eq!(titles(&filter), ["Aardvark", "Gizmo Deluxe"]);
        assert!(Chip::Store(Store::Epic).is_active(&filter));
        assert!(Chip::Store(Store::Gog).is_active(&filter));
        assert!(!Chip::Store(Store::Steam).is_active(&filter));

        // Toggling one off leaves the other.
        Chip::Store(Store::Epic).toggle(&mut filter);
        assert_eq!(titles(&filter), ["Aardvark"]);

        // Toggling the last one off returns to every store.
        Chip::Store(Store::Gog).toggle(&mut filter);
        assert!(filter.stores.is_empty());
        assert_eq!(titles(&filter).len(), 5);
    }

    #[test]
    fn the_sort_chip_cycles_title_playtime_recent() {
        let mut filter = LibraryFilter::default();
        assert_eq!(filter.sort, Sort::Title);
        assert!(!Chip::Sort.is_active(&filter), "a cycle is never 'on'");

        Chip::Sort.toggle(&mut filter);
        assert_eq!(filter.sort, Sort::Playtime);
        assert_eq!(
            titles(&filter),
            ["Zelda", "Dota 2", "Aardvark", "Portal", "Gizmo Deluxe"]
        );

        Chip::Sort.toggle(&mut filter);
        assert_eq!(filter.sort, Sort::Recent);

        Chip::Sort.toggle(&mut filter);
        assert_eq!(filter.sort, Sort::Title, "the cycle closes");
    }

    // -- The ragged chip-over-tiles graph ----------------------------------

    fn cell(graph: &FocusGraph, id: usize) -> (usize, usize) {
        let node = graph
            .nodes()
            .iter()
            .find(|node| node.id == FocusId::from(id))
            .unwrap_or_else(|| panic!("cell {id} is in the graph"));
        (node.row, node.col)
    }

    #[test]
    fn the_graph_lays_the_chips_on_row_zero_and_the_tiles_below() {
        let graph = ragged_graph(3, 3, 4, 2);

        assert_eq!(cell(&graph, 0), (0, 0));
        assert_eq!(cell(&graph, 1), (0, 1));
        assert_eq!(cell(&graph, 2), (0, 2));
        assert_eq!(cell(&graph, 3), (1, 0), "the first tile row follows");
        assert_eq!(cell(&graph, 4), (1, 1));
        assert_eq!(cell(&graph, 5), (2, 0));
        assert_eq!(cell(&graph, 6), (2, 1));
        assert_eq!(graph.len(), 7);
    }

    #[test]
    fn up_from_the_first_tile_row_reaches_a_chip() {
        let mut graph = ragged_graph(3, 3, 4, 2);
        graph.focus(3usize); // the first tile, at (1, 0)
        let next = graph.move_focus(Action::NavigateUp).expect("a chip above");
        assert_eq!(next, FocusId::from(0usize), "the chip directly above");
    }

    #[test]
    fn an_overflowing_chip_row_wraps_and_the_tiles_follow_the_last_chip_row() {
        // Six chips, four to a line: rows 0 and 1; the tiles start on row 2.
        let graph = ragged_graph(6, 4, 3, 3);
        assert_eq!(cell(&graph, 0), (0, 0));
        assert_eq!(cell(&graph, 3), (0, 3));
        assert_eq!(cell(&graph, 4), (1, 0), "the fifth chip wraps");
        assert_eq!(cell(&graph, 5), (1, 1));
        assert_eq!(
            cell(&graph, 6),
            (2, 0),
            "tiles begin after the last chip row"
        );
    }

    #[test]
    fn up_from_the_top_tile_reaches_the_wrapped_chip_row() {
        let mut graph = ragged_graph(6, 4, 3, 3);
        graph.focus(7usize); // a tile on row 2
        let next = graph.move_focus(Action::NavigateUp).expect("a chip above");
        assert!(next.get() < 6, "Up landed on a chip, got {next}");
    }

    #[test]
    fn every_cell_of_a_ragged_row_reaches_every_other() {
        let graph = ragged_graph(6, 4, 7, 3);
        let ids: Vec<FocusId> = graph.nodes().iter().map(|node| node.id).collect();
        for start in &ids {
            let mut seen = std::collections::BTreeSet::new();
            seen.insert(*start);
            let mut queue = vec![*start];
            while let Some(current) = queue.pop() {
                for action in [
                    Action::NavigateUp,
                    Action::NavigateDown,
                    Action::NavigateLeft,
                    Action::NavigateRight,
                ] {
                    let mut copy = graph.clone();
                    copy.focus(current);
                    if let Some(next) = copy.move_focus(action)
                        && seen.insert(next)
                    {
                        queue.push(next);
                    }
                }
            }
            assert_eq!(
                seen.len(),
                ids.len(),
                "from {start} only reached {} of {}",
                seen.len(),
                ids.len()
            );
        }
    }

    #[test]
    fn a_chip_row_builds_its_graph_with_the_chips_as_the_first_ids() {
        let library = library();
        let row = ChipRow::build(&library, 1_000.0);
        let graph = row.graph(4, 3);
        assert_eq!(graph.len(), row.chips().len() + 4);
        // The chip ids are the chip indices; the tiles follow.
        assert_eq!(row.chip(FocusId::from(0usize)), Some(Chip::Installed));
        assert_eq!(
            row.chip(FocusId::from(2usize)),
            Some(Chip::Store(Store::Steam))
        );
        assert_eq!(row.chip(row.tile_id(0)), None, "a tile id is not a chip");
        assert_eq!(row.tile_index(row.tile_id(2)), Some(2));
        assert_eq!(row.tile_index(FocusId::from(0usize)), None);
    }

    #[test]
    fn chip_columns_fit_as_many_as_the_width_allows_and_never_zero() {
        assert_eq!(chip_columns(0, 1_000.0), 0, "no chips, no columns");
        assert_eq!(chip_columns(8, 0.0), 8, "unmeasured: one row");
        assert_eq!(chip_columns(8, CHIP_MIN_WIDTH * 3.0), 3);
        assert_eq!(
            chip_columns(2, CHIP_MIN_WIDTH * 3.0),
            2,
            "never more columns than chips"
        );
        assert_eq!(chip_columns(8, 1.0), 1, "a narrow row holds at least one");
    }
}
