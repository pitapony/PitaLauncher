//! The app's navigation model: where the window is and what it opened.
//!
//! Framework-free on purpose (ADR-0027): the launcher's screen state is plain
//! data with plain methods, so it is testable without a display and the Dioxus
//! seam only has to hold it in a `Signal`. `View` moved here from `main.rs` so
//! the model owns the closed set of screens, not the root component.

use pitagames_core::GameId;

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
    query: String,
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
            query: String::new(),
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

    pub(crate) fn query(&self) -> String {
        self.query.clone()
    }

    pub(crate) fn text_focused(&self) -> bool {
        self.text_focused
    }

    pub(crate) fn set_view(&mut self, view: View) {
        self.view = view;
    }

    pub(crate) fn set_query(&mut self, query: String) {
        self.query = query;
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
        assert_eq!(nav.query(), "");
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
    fn the_query_and_text_focus_round_trip() {
        let mut nav = Navigation::new(View::Library);
        nav.set_query("half".to_string());
        nav.set_text_focused(true);
        assert_eq!(nav.query(), "half");
        assert!(nav.text_focused());
    }
}
