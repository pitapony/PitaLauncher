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
use pitagames_stores::InstallPhase;
use pitagames_stores::status::updates;

use crate::library::{GameEntry, Library};

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

    #[test]
    fn apply_returns_indices_into_the_library() {
        let library = library();
        let filter = LibraryFilter {
            query: "dota".to_string(),
            ..LibraryFilter::default()
        };
        assert_eq!(filter.apply(&library), vec![0], "Dota 2 is the first entry");
    }
}
