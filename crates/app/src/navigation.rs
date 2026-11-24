//! The app's navigation model: where the window is and what it opened.
//!
//! Framework-free on purpose (ADR-0027): the launcher's screen state is plain
//! data with plain methods, so it is testable without a display and the Dioxus
//! seam only has to hold it in a `Signal`. `View` moved here from `main.rs` so
//! the model owns the closed set of screens, not the root component.

use pitagames_core::GameId;

use crate::filter::LibraryFilter;

/// Which top-level list the window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Home,
    Library,
    Downloads,
    History,
    Stores,
    Detail,
    Account,
    Controls,
    Logs,
}

/// The top-level destinations, in the desktop pill order.
///
/// `Detail` is deliberately absent: it is a page opened *from* a destination,
/// not a destination itself. The console top bar draws this strip and `Tab`
/// cycles it, so the two can never disagree about which screens exist.
pub(crate) const DESTINATIONS: [View; 8] = [
    View::Home,
    View::Library,
    View::Downloads,
    View::History,
    View::Stores,
    View::Account,
    View::Controls,
    View::Logs,
];

impl View {
    /// The label a nav pill or the console destination strip shows.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Library => "Library",
            Self::Downloads => "Downloads",
            Self::History => "History",
            Self::Stores => "Stores",
            // Not a destination, but the enum is closed and a label is cheap.
            Self::Detail => "Details",
            Self::Account => "Account",
            Self::Controls => "Controls",
            Self::Logs => "Logs",
        }
    }
}

/// The destination a `Tab` step lands on.
///
/// `forward` moves to the next destination in [`DESTINATIONS`] and wraps at the
/// end; `false` moves backwards and wraps at the start. A view that is not a
/// destination (the detail page) has no place in the ring, so forward lands on
/// the first destination and backward on the last — a defined answer either way,
/// so `Tab` is never a no-op.
pub(crate) fn cycle_destination(view: View, forward: bool) -> View {
    let current = DESTINATIONS
        .iter()
        .position(|destination| *destination == view);
    let index = match (current, forward) {
        (Some(index), true) => (index + 1) % DESTINATIONS.len(),
        (Some(index), false) => (index + DESTINATIONS.len() - 1) % DESTINATIONS.len(),
        (None, true) => 0,
        (None, false) => DESTINATIONS.len() - 1,
    };
    DESTINATIONS[index]
}

/// Where the window is, where Back returns to, and what it has open.
///
/// This is the concept every screen reaches for, so it lives in one module
/// instead of six signals owned by the root: a screen reads it and writes it
/// through the one [`Session`](crate::session::Session) handle.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Navigation {
    view: View,
    back: View,
    selected: Option<GameId>,
    filter: LibraryFilter,
    text_focused: bool,
}

impl Navigation {
    /// A fresh navigation on `view`, with Back pointing at the grid (the
    /// desktop shell's start screen) and nothing selected.
    pub(crate) fn new(view: View) -> Self {
        Self {
            view,
            back: View::Library,
            selected: None,
            filter: LibraryFilter::default(),
            text_focused: false,
        }
    }

    pub(crate) fn view(&self) -> View {
        self.view
    }

    pub(crate) fn back(&self) -> View {
        self.back
    }

    pub(crate) fn selected(&self) -> Option<GameId> {
        self.selected.clone()
    }

    pub(crate) fn filter(&self) -> &LibraryFilter {
        &self.filter
    }

    pub(crate) fn text_focused(&self) -> bool {
        self.text_focused
    }

    pub(crate) fn set_view(&mut self, view: View) {
        self.view = view;
    }

    pub(crate) fn filter_mut(&mut self) -> &mut LibraryFilter {
        &mut self.filter
    }

    /// Clear the library filter back to the neutral one.
    ///
    /// `Back` on the grid clears a narrowing filter before it leaves, so the
    /// rule lives on the model the screen already reaches through the session.
    pub(crate) fn clear_filter(&mut self) {
        self.filter.clear();
    }

    /// Commit a typed search and show the Library.
    ///
    /// A search can start on the console Home (`Action::Search`), so the commit
    /// both sets the query and lands on the screen that draws its matches; the
    /// Library's own search box and on-screen keyboard use the same path, so the
    /// two cannot drift.
    pub(crate) fn search(&mut self, query: String) {
        self.filter.query = query;
        self.selected = None;
        self.view = View::Library;
    }

    pub(crate) fn set_text_focused(&mut self, focused: bool) {
        self.text_focused = focused;
    }

    /// Leave to a top-level screen, clearing the selection.
    ///
    /// This is the navigation row's idiom (`selected_game.set(None)` then
    /// `view.set(view)`), named once so the nine rows cannot drift.
    pub(crate) fn go(&mut self, view: View) {
        self.view = view;
        self.selected = None;
    }

    /// Open a game's detail page, remembering where Back should return.
    pub(crate) fn open_detail(&mut self, id: GameId, from: View) {
        self.back = from;
        self.selected = Some(id);
        self.view = View::Detail;
    }

    /// Cycle the current view to the next (`forward`) or previous destination.
    ///
    /// `Tab`'s job, handled at the app level so it works on every screen without
    /// each one owning a copy of the ring. It clears the selection like [`go`],
    /// so tabbing off the detail page leaves nothing half-open.
    ///
    /// [`go`]: Navigation::go
    pub(crate) fn tab(&mut self, forward: bool) {
        self.go(cycle_destination(self.view, forward));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> GameId {
        GameId::new(pitagames_core::Store::Steam, name)
    }

    #[test]
    fn a_new_navigation_starts_on_the_given_view_with_nothing_open() {
        let nav = Navigation::new(View::Home);
        assert_eq!(nav.view(), View::Home);
        assert_eq!(nav.back(), View::Library);
        assert_eq!(nav.selected(), None);
        assert_eq!(nav.filter(), &LibraryFilter::default());
        assert!(!nav.text_focused());
    }

    #[test]
    fn go_clears_the_selection() {
        let mut nav = Navigation::new(View::Library);
        nav.open_detail(id("620"), View::Library);
        nav.go(View::Stores);
        assert_eq!(nav.view(), View::Stores);
        assert_eq!(nav.selected(), None);
    }

    #[test]
    fn open_detail_remembers_where_back_returns() {
        let mut nav = Navigation::new(View::Home);
        nav.open_detail(id("620"), View::Home);
        assert_eq!(nav.view(), View::Detail);
        assert_eq!(nav.back(), View::Home);
        assert_eq!(nav.selected(), Some(id("620")));
        // Back is where the page was opened from, not a fixed screen.
        nav.open_detail(id("620"), View::Stores);
        assert_eq!(nav.back(), View::Stores);
    }

    #[test]
    fn the_filter_and_text_focus_round_trip() {
        let mut nav = Navigation::new(View::Library);
        nav.filter_mut().query = "half".to_string();
        nav.filter_mut().installed_only = true;
        nav.set_text_focused(true);
        assert_eq!(nav.filter().query, "half");
        assert!(nav.filter().installed_only);
        assert!(nav.text_focused());
    }

    #[test]
    fn clearing_the_filter_leaves_the_neutral_one() {
        let mut nav = Navigation::new(View::Library);
        nav.filter_mut().query = "half".to_string();
        nav.filter_mut().installed_only = true;
        nav.clear_filter();
        assert_eq!(nav.filter(), &LibraryFilter::default());
    }

    #[test]
    fn a_search_commits_the_query_and_lands_on_the_library() {
        // The console Home's `Search` path: type on the Home, land on the
        // Library with the matches shown.
        let mut nav = Navigation::new(View::Home);
        nav.open_detail(id("620"), View::Home);
        nav.search("portal".to_string());
        assert_eq!(nav.view(), View::Library);
        assert_eq!(nav.filter().query, "portal");
        assert_eq!(nav.selected(), None);
    }

    // -- Tab destination cycling -------------------------------------------

    #[test]
    fn the_destinations_are_the_desktop_pill_order() {
        assert_eq!(
            DESTINATIONS,
            [
                View::Home,
                View::Library,
                View::Downloads,
                View::History,
                View::Stores,
                View::Account,
                View::Controls,
                View::Logs,
            ]
        );
        let labels: Vec<&str> = DESTINATIONS.iter().map(|view| view.label()).collect();
        assert_eq!(
            labels,
            [
                "Home",
                "Library",
                "Downloads",
                "History",
                "Stores",
                "Account",
                "Controls",
                "Logs",
            ]
        );
    }

    #[test]
    fn tab_next_cycles_through_every_destination_and_wraps() {
        // Walking TabNext from Home visits every destination in pill order and
        // comes back around, so nothing is stranded.
        let mut nav = Navigation::new(View::Home);
        let mut visited = vec![nav.view()];
        for _ in 0..DESTINATIONS.len() {
            nav.tab(true);
            visited.push(nav.view());
        }

        let mut expected = DESTINATIONS.to_vec();
        expected.push(View::Home);
        assert_eq!(visited, expected);
    }

    #[test]
    fn tab_prev_walks_the_destinations_backwards() {
        let mut nav = Navigation::new(View::Home);
        nav.tab(false);
        assert_eq!(
            nav.view(),
            View::Logs,
            "backwards from the first wraps to the last"
        );
        nav.tab(false);
        assert_eq!(nav.view(), View::Controls);
    }

    #[test]
    fn every_destination_reaches_every_other_with_tab() {
        for start in DESTINATIONS {
            let mut nav = Navigation::new(start);
            let mut seen = std::collections::BTreeSet::new();
            seen.insert(format!("{:?}", nav.view()));
            for _ in 0..DESTINATIONS.len() - 1 {
                nav.tab(true);
                seen.insert(format!("{:?}", nav.view()));
            }
            assert_eq!(
                seen.len(),
                DESTINATIONS.len(),
                "from {start:?} Tab reached only {} destinations",
                seen.len()
            );
        }
    }

    #[test]
    fn a_tab_step_off_the_detail_page_lands_on_a_destination() {
        // `Detail` is not a destination: forward lands on the first and
        // backward on the last, and either way the open selection is cleared.
        let mut nav = Navigation::new(View::Home);
        nav.open_detail(id("620"), View::Library);
        assert_eq!(nav.view(), View::Detail);
        nav.tab(true);
        assert_eq!(nav.view(), View::Home);
        assert_eq!(nav.selected(), None);

        nav.open_detail(id("620"), View::Library);
        nav.tab(false);
        assert_eq!(nav.view(), View::Logs);
        assert_eq!(nav.selected(), None);
    }
}
