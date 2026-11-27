//! Stepping the window aside while a game runs.
//!
//! The console shell is a couch UI: when a supervised (non-Steam) game starts,
//! the launcher has no business on screen and no business reading the pad — the
//! game owns both. On exit the launcher comes back and asks the compositor for
//! focus; a Wayland compositor may refuse (focus-stealing prevention), and a
//! window the user cannot steer is exactly the failure `--safe-mode` exists to
//! avoid. So the request has a deadline and a visible fallback: a badge saying
//! the game ended and to press Guide.
//!
//! This module is framework-free and unit-tested; [`crate::App`] owns the
//! window calls and the clock and asks in here *what* to do. The plan's console
//! hand-off (`§7 Console mode`) lives here:
//!
//! ```text
//!  PlayEvent::Started ──► Handoff::started ──► Hide  (console shell only)
//!  PlayEvent::Finished ─► Handoff::ended  ──► Show + ask for focus
//!                                     │
//!                    focus granted ───┴─── focus refused (timeout) ──► badge
//! ```
//!
//! Steam is deliberately absent: Steam starts its own games, so the launcher
//! never sees a start or an exit to hand off around.

use pitagames_core::GameId;
use pitagames_core::i18n::t;

/// How long the window waits for focus after a game ends before it shows the
/// "press Guide" fallback badge.
pub const FOCUS_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// What the window should do about a launch transition.
///
/// The model cannot touch a window, so it returns the decision and
/// [`crate::App`] applies it ([`crate::shell::hide_window`] /
/// [`crate::shell::show_and_focus`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    /// Hide the window while the game starts (console shell only).
    Hide,
    /// Show the window and ask the compositor for focus (game over).
    Show,
    /// Leave the window as it is.
    None,
}

/// The one supervised game the window stepped aside for, plus the state of the
/// return trip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Handoff {
    /// The game the launcher stepped aside for; `None` when the window is the
    /// user's.
    running: Option<GameId>,
    /// The window was actually hidden for this game (console shell only).
    hidden: bool,
    /// The window was shown again but focus has not been confirmed yet.
    awaiting_focus: bool,
    /// The badge to show if focus is refused; built when the game ends.
    pending_badge: Option<String>,
    /// The badge currently on screen.
    badge: Option<String>,
}

impl Handoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// A game started: hide the window in the console shell.
    ///
    /// Returns [`WindowAction::Hide`] only for the console shell; the desktop
    /// shell keeps its window so a pointer user can keep browsing while a game
    /// runs.
    pub fn started(&mut self, id: &GameId, console: bool) -> WindowAction {
        self.running = Some(id.clone());
        self.hidden = console;
        self.awaiting_focus = false;
        self.pending_badge = None;
        // A fresh launch clears whatever the previous game left behind.
        self.badge = None;
        if console {
            WindowAction::Hide
        } else {
            WindowAction::None
        }
    }

    /// A game ended: bring the window back and arm the focus fallback.
    ///
    /// Returns [`WindowAction::Show`] only if the window was hidden, so a
    /// desktop session (or an end without a start) leaves it alone. The badge
    /// text is built now and only promoted to [`Self::badge`] if focus never
    /// arrives.
    pub fn ended(&mut self, title: &str, summary: &str) -> WindowAction {
        if self.running.take().is_none() {
            return WindowAction::None;
        }
        let hid = std::mem::replace(&mut self.hidden, false);
        if !hid {
            return WindowAction::None;
        }
        self.awaiting_focus = true;
        self.pending_badge = Some(badge_text(title, summary));
        WindowAction::Show
    }

    /// The compositor handed focus to the window: nothing left to fall back to.
    ///
    /// Also clears a badge already on screen: the user got back to the window,
    /// so the "press Guide" prompt has done its job.
    pub fn focus_granted(&mut self) {
        self.awaiting_focus = false;
        self.pending_badge = None;
        self.badge = None;
    }

    /// Focus never arrived within [`FOCUS_GRACE`]: show the fallback badge.
    pub fn focus_timeout(&mut self) {
        if !self.awaiting_focus {
            return;
        }
        self.awaiting_focus = false;
        self.badge = self.pending_badge.take();
    }

    /// The player pressed Guide (or clicked Dismiss) to clear the badge.
    pub fn dismiss(&mut self) {
        self.badge = None;
    }

    /// Whether the window is hidden for a running game.
    ///
    /// While this is true the input pumps drop actions: the game owns the pad,
    /// and a hidden window must not accumulate focus moves behind it.
    pub fn hidden(&self) -> bool {
        self.hidden
    }

    /// Whether the window is waiting for the compositor to confirm focus.
    pub fn awaiting_focus(&self) -> bool {
        self.awaiting_focus
    }

    /// The "game ended — press Guide" badge, if one is showing.
    pub fn badge(&self) -> Option<&str> {
        self.badge.as_deref()
    }
}

/// The sentence the fallback badge shows.
///
/// One function so the text a controller shows and the text a test asserts
/// cannot drift.
pub fn badge_text(title: &str, summary: &str) -> String {
    let summary = summary.trim();
    if summary.is_empty() {
        t("handoff.ended").replace("{title}", title)
    } else {
        t("handoff.ended_summary")
            .replace("{title}", title)
            .replace("{summary}", summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::Store;

    fn id(name: &str) -> GameId {
        GameId::new(Store::Epic, name)
    }

    #[test]
    fn the_console_shell_hides_for_a_running_game() {
        let mut handoff = Handoff::new();
        assert_eq!(handoff.started(&id("Gizmo"), true), WindowAction::Hide);
        assert!(handoff.hidden());
        assert!(!handoff.awaiting_focus());
        assert_eq!(handoff.badge(), None);
    }

    #[test]
    fn the_desktop_shell_keeps_its_window() {
        let mut handoff = Handoff::new();
        assert_eq!(handoff.started(&id("Gizmo"), false), WindowAction::None);
        assert!(
            !handoff.hidden(),
            "the desktop shell keeps its window while a game runs"
        );
    }

    #[test]
    fn a_hidden_window_returns_when_the_game_ends() {
        let mut handoff = Handoff::new();
        handoff.started(&id("Gizmo"), true);

        assert_eq!(
            handoff.ended("Gizmo", "exited 0 after 3s"),
            WindowAction::Show
        );
        assert!(!handoff.hidden());
        assert!(handoff.awaiting_focus());
        assert_eq!(handoff.badge(), None, "the badge waits for the timeout");
    }

    #[test]
    fn a_desktop_session_does_not_show_or_badge() {
        let mut handoff = Handoff::new();
        handoff.started(&id("Gizmo"), false);

        assert_eq!(handoff.ended("Gizmo", "exited 0"), WindowAction::None);
        assert!(!handoff.awaiting_focus());
        assert_eq!(handoff.badge(), None);
    }

    #[test]
    fn an_end_without_a_start_is_ignored() {
        let mut handoff = Handoff::new();
        assert_eq!(handoff.ended("Ghost", "exited 0"), WindowAction::None);
        assert_eq!(handoff.badge(), None);
    }

    #[test]
    fn granted_focus_clears_the_fallback() {
        let mut handoff = Handoff::new();
        handoff.started(&id("Gizmo"), true);
        handoff.ended("Gizmo", "exited 0 after 3s");

        handoff.focus_granted();
        assert!(!handoff.awaiting_focus());

        // A later timeout must not resurrect a badge after focus arrived.
        handoff.focus_timeout();
        assert_eq!(handoff.badge(), None);
    }

    #[test]
    fn returning_to_the_window_clears_a_shown_badge() {
        let mut handoff = Handoff::new();
        handoff.started(&id("Gizmo"), true);
        handoff.ended("Gizmo", "exited 0");
        handoff.focus_timeout();
        assert!(handoff.badge().is_some());

        // The user clicked the window (or the compositor finally focused it).
        handoff.focus_granted();
        assert_eq!(handoff.badge(), None);
    }

    #[test]
    fn a_refused_focus_shows_the_badge() {
        let mut handoff = Handoff::new();
        handoff.started(&id("Gizmo"), true);
        handoff.ended("Gizmo", "exited 0 after 3s");

        handoff.focus_timeout();
        assert!(!handoff.awaiting_focus());
        assert_eq!(
            handoff.badge(),
            Some("Gizmo exited 0 after 3s — press Guide to return.")
        );
    }

    #[test]
    fn the_badge_names_the_verdict_and_survives_a_blank_one() {
        assert_eq!(
            badge_text("Gizmo", "stopped by signal 15 after 4s"),
            "Gizmo stopped by signal 15 after 4s — press Guide to return."
        );
        assert_eq!(
            badge_text("Gizmo", "  "),
            "Gizmo ended — press Guide to return."
        );
    }

    #[test]
    fn dismissing_clears_the_badge() {
        let mut handoff = Handoff::new();
        handoff.started(&id("Gizmo"), true);
        handoff.ended("Gizmo", "exited 0");
        handoff.focus_timeout();
        assert!(handoff.badge().is_some());

        handoff.dismiss();
        assert_eq!(handoff.badge(), None);
        assert!(!handoff.awaiting_focus());
    }

    #[test]
    fn a_new_launch_clears_the_previous_badge() {
        let mut handoff = Handoff::new();
        handoff.started(&id("Gizmo"), true);
        handoff.ended("Gizmo", "exited 0");
        handoff.focus_timeout();

        assert_eq!(handoff.started(&id("Other"), true), WindowAction::Hide);
        assert_eq!(handoff.badge(), None);
        assert!(!handoff.awaiting_focus());
    }

    #[test]
    fn a_timeout_without_a_pending_return_does_nothing() {
        let mut handoff = Handoff::new();
        handoff.started(&id("Gizmo"), true);
        handoff.focus_timeout();
        assert_eq!(handoff.badge(), None, "the game is still running");
        assert!(handoff.hidden());
    }
}
