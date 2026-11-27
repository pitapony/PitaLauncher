//! The console shell's system overlay.
//!
//! The pad's `Start` (or the keyboard's `M`) opens a Steam-style system menu over
//! whatever the launcher is showing: the plan's console *Overlay* row
//! (`§7 Console mode`). It is the M2.5 surface where the couch user finds the
//! things that do not belong to a single screen — resume, the running game's
//! details and store page, the shell switch, and the (v1.1) power menu.
//!
//! This module is framework-free and unit-tested, like [`crate::handoff`],
//! [`crate::home`] and [`crate::grid`]. [`crate::App`] owns the signal that says
//! whether the overlay is open, the pads and batteries it names, and the window
//! calls a choice makes; the model only answers *what rows exist* and *which one
//! is focused*.
//!
//! Two deliberate choices:
//!
//! - The rows are built from an [`OverlayContext`] each time the overlay opens,
//!   so a row is never offered for a game that is not there: no running or open
//!   game, no "Game details"; a non-Steam game, no "Store page".
//! - The power rows are enabled **only in the console session** (`v1.1`): the
//!   session is where a couch user controls the machine, and the action goes to
//!   logind through [`pitagames_platform::power`]. Off the session they stay
//!   visible but disabled, so a desktop user sees the shape without a press that
//!   cannot land.

use pitagames_core::GameId;
use pitagames_core::i18n::t;
use pitagames_input::{Action, FocusGraph};
use pitagames_platform::power::PowerAction;

/// One thing the overlay can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayChoice {
    /// Close the overlay and return to the launcher.
    Close,
    /// Open the focused/running game's detail page.
    Details,
    /// Ask Steam to open the game's store page.
    StorePage,
    /// Switch between the console and desktop shells.
    ToggleShell,
    /// Ask logind to perform a power action (session mode only).
    Power(PowerAction),
}

/// The game the overlay is about: the running one, else the last page opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayGame {
    pub id: GameId,
    pub title: String,
    /// Whether the store page action is meaningful (Steam owns a store page).
    pub steam: bool,
}

/// What the overlay offers, given what the shell is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayContext {
    /// Whether the console shell is showing, which names the shell switch.
    pub console: bool,
    /// Whether this run is the console session (`PITAGAMES_SESSION=1`), which
    /// enables the power rows.
    pub session: bool,
    /// The game the overlay is about, if any.
    pub game: Option<OverlayGame>,
}

/// One row of the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayEntry {
    /// The row's short name.
    pub label: String,
    /// A one-line hint under the name.
    pub detail: String,
    /// `None` for a row that is shown but cannot be chosen yet.
    pub choice: Option<OverlayChoice>,
}

impl OverlayEntry {
    fn enabled(choice: OverlayChoice, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            choice: Some(choice),
        }
    }

    fn disabled(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            choice: None,
        }
    }

    /// Whether the row can be chosen (a disabled row is drawn greyed).
    pub fn is_enabled(&self) -> bool {
        self.choice.is_some()
    }
}

/// The open overlay: its rows and the focused one.
///
/// Focus starts on the first row (`Resume`), which always exists, so a controller
/// that opens the menu has a ring to move before it presses anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    entries: Vec<OverlayEntry>,
    focus: Option<usize>,
}

impl Overlay {
    /// Build the rows the current context offers.
    pub fn build(context: &OverlayContext) -> Self {
        let mut entries = Vec::new();
        entries.push(OverlayEntry::enabled(
            OverlayChoice::Close,
            t("overlay.resume"),
            t("overlay.resume_detail"),
        ));

        if let Some(game) = &context.game {
            entries.push(OverlayEntry::enabled(
                OverlayChoice::Details,
                t("overlay.details"),
                format!("{} · {}", game.title, t("overlay.details_hint")),
            ));
            if game.steam {
                entries.push(OverlayEntry::enabled(
                    OverlayChoice::StorePage,
                    t("overlay.store_page"),
                    t("overlay.store_page_detail"),
                ));
            }
        }

        entries.push(OverlayEntry::enabled(
            OverlayChoice::ToggleShell,
            if context.console {
                t("overlay.exit_console")
            } else {
                t("overlay.enter_console")
            },
            t("overlay.shell_detail"),
        ));

        // Power is the console session's (v1.1, over logind). It is offered only
        // where it can work; off the session the rows stay visible but disabled,
        // so a desktop user sees the shape without a press that cannot land.
        for (action, label, detail) in power_rows() {
            if context.session {
                entries.push(OverlayEntry::enabled(
                    OverlayChoice::Power(action),
                    label,
                    detail,
                ));
            } else {
                entries.push(OverlayEntry::disabled(label, t("overlay.power_disabled")));
            }
        }

        let focus = (!entries.is_empty()).then_some(0);
        Self { entries, focus }
    }

    /// The rows, top to bottom.
    pub fn entries(&self) -> &[OverlayEntry] {
        &self.entries
    }

    /// The focused row index, if any.
    pub fn focused(&self) -> Option<usize> {
        self.focus
    }

    /// The focused row, if any.
    pub fn focused_entry(&self) -> Option<&OverlayEntry> {
        self.entries.get(self.focus?)
    }

    /// What pressing Confirm on the focused row does, or `None` for a disabled
    /// row.
    pub fn focused_choice(&self) -> Option<OverlayChoice> {
        self.focused_entry().and_then(|entry| entry.choice)
    }

    /// Move the ring over the rows; returns whether focus moved.
    ///
    /// A single column with per-axis wrap, so `Down` past the last row returns to
    /// `Resume` and `Up` from the top lands on the last power row — a menu is a
    /// loop, never a trap.
    pub fn move_focus(&mut self, action: Action) -> bool {
        if !action.is_navigation() || self.entries.is_empty() {
            return false;
        }
        let mut graph = FocusGraph::grid(self.entries.len(), 1);
        graph.focus_index(self.focus);
        graph.move_focus(action);
        let next = graph.focused_index();
        if next != self.focus {
            self.focus = next;
            return true;
        }
        false
    }
}

/// The console session's power rows, in menu order: `(action, label, detail)`.
///
/// The label and detail come from literal `t(..)` calls — not a key returned by a
/// helper — because `crates/core`'s i18n source scan only sees a key when it sits
/// directly inside a `t` call. A test ties the actions to [`PowerAction::MENU`] so
/// this list and the platform's cannot drift.
fn power_rows() -> [(PowerAction, &'static str, &'static str); 3] {
    [
        (
            PowerAction::Suspend,
            t("overlay.power_suspend"),
            t("overlay.power_suspend_detail"),
        ),
        (
            PowerAction::Restart,
            t("overlay.power_restart"),
            t("overlay.power_restart_detail"),
        ),
        (
            PowerAction::Shutdown,
            t("overlay.power_shutdown"),
            t("overlay.power_shutdown_detail"),
        ),
    ]
}

/// The overlay's wall clock, as `HH:MM`.
///
/// One formatter so the view and a test cannot disagree about padding.
pub fn format_clock(now: time::OffsetDateTime) -> String {
    format!("{:02}:{:02}", now.hour(), now.minute())
}

/// The local wall-clock time, for the overlay's clock.
///
/// The offset comes from `localtime_r` rather than `time`'s `local-offset`
/// feature: the app is multithreaded, where that feature is unsound, while
/// `localtime_r` only *reads* `TZ` and `/etc/localtime` and writes nothing. It is
/// called per tick and cached in nothing, so a timezone change is picked up.
pub fn now_local() -> time::OffsetDateTime {
    let offset = local_offset().unwrap_or(time::UtcOffset::UTC);
    time::OffsetDateTime::now_utc().to_offset(offset)
}

/// The local UTC offset, or `None` when the system cannot say.
fn local_offset() -> Option<time::UtcOffset> {
    // SAFETY: `localtime_r` is the thread-safe variant (it writes only the `tm`
    // it is handed) and only reads the environment; the launcher never calls
    // `setenv`/`tzset`, so there is no concurrent timezone mutation to race.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return None;
        }
        time::UtcOffset::from_whole_seconds(tm.tm_gmtoff as i32).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::Store;

    fn game(store: Store) -> OverlayGame {
        OverlayGame {
            id: GameId::new(store, "620"),
            title: "Portal 2".to_string(),
            steam: store == Store::Steam,
        }
    }

    fn console() -> OverlayContext {
        OverlayContext {
            console: true,
            session: false,
            game: None,
        }
    }

    fn session() -> OverlayContext {
        OverlayContext {
            console: true,
            session: true,
            game: None,
        }
    }

    #[test]
    fn resume_is_always_first_and_focused() {
        let overlay = Overlay::build(&console());
        assert_eq!(overlay.focused(), Some(0));
        assert_eq!(overlay.entries()[0].choice, Some(OverlayChoice::Close));
        assert!(overlay.entries()[0].is_enabled());
    }

    #[test]
    fn a_steam_game_adds_details_and_a_store_page() {
        let context = OverlayContext {
            console: true,
            session: false,
            game: Some(game(Store::Steam)),
        };
        let overlay = Overlay::build(&context);
        let choices: Vec<Option<OverlayChoice>> =
            overlay.entries().iter().map(|entry| entry.choice).collect();
        assert_eq!(
            choices,
            [
                Some(OverlayChoice::Close),
                Some(OverlayChoice::Details),
                Some(OverlayChoice::StorePage),
                Some(OverlayChoice::ToggleShell),
                None,
                None,
                None,
            ]
        );
        assert!(overlay.entries()[1].detail.contains("Portal 2"));
    }

    #[test]
    fn a_non_steam_game_has_no_store_page() {
        let context = OverlayContext {
            console: true,
            session: false,
            game: Some(game(Store::Epic)),
        };
        let overlay = Overlay::build(&context);
        assert!(
            overlay
                .entries()
                .iter()
                .all(|entry| entry.choice != Some(OverlayChoice::StorePage)),
            "only Steam has a store page to hand off to"
        );
    }

    #[test]
    fn a_game_less_overlay_has_no_details() {
        let overlay = Overlay::build(&console());
        assert!(
            overlay
                .entries()
                .iter()
                .all(|entry| entry.choice != Some(OverlayChoice::Details))
        );
    }

    #[test]
    fn the_shell_row_follows_the_shell() {
        let console = Overlay::build(&console());
        let row = console
            .entries()
            .iter()
            .find(|entry| entry.choice == Some(OverlayChoice::ToggleShell))
            .unwrap();
        assert_eq!(row.label, "Exit console mode");

        let desktop = Overlay::build(&OverlayContext::default());
        let row = desktop
            .entries()
            .iter()
            .find(|entry| entry.choice == Some(OverlayChoice::ToggleShell))
            .unwrap();
        assert_eq!(row.label, "Enter console mode");
    }

    #[test]
    fn the_power_rows_are_enabled_in_the_session() {
        let overlay = Overlay::build(&session());
        let power: Vec<Option<OverlayChoice>> = overlay
            .entries()
            .iter()
            .filter(|entry| matches!(entry.choice, Some(OverlayChoice::Power(_))))
            .map(|entry| entry.choice)
            .collect();
        assert_eq!(
            power,
            PowerAction::MENU
                .iter()
                .map(|action| Some(OverlayChoice::Power(*action)))
                .collect::<Vec<_>>(),
            "the menu carries every power action, in order"
        );
        for (action, label, detail) in power_rows() {
            let row = overlay
                .entries()
                .iter()
                .find(|entry| entry.choice == Some(OverlayChoice::Power(action)))
                .unwrap_or_else(|| panic!("{action} is missing"));
            assert!(row.is_enabled(), "{action} must be live in the session");
            assert_eq!(row.label, label);
            assert_eq!(row.detail, detail);
        }
    }

    #[test]
    fn the_power_rows_are_disabled_off_the_session() {
        let overlay = Overlay::build(&console());
        let disabled: Vec<(&str, &str)> = overlay
            .entries()
            .iter()
            .filter(|entry| !entry.is_enabled())
            .map(|entry| (entry.label.as_str(), entry.detail.as_str()))
            .collect();
        let expected: Vec<(&str, &str)> = power_rows()
            .into_iter()
            .map(|(_, label, _)| (label, t("overlay.power_disabled")))
            .collect();
        assert_eq!(
            disabled, expected,
            "off the session the three power rows are the only disabled rows"
        );
        for (_, label, _) in power_rows() {
            let row = overlay
                .entries()
                .iter()
                .find(|entry| entry.label == label)
                .unwrap_or_else(|| panic!("{label} is missing"));
            assert!(row.choice.is_none(), "{label} must offer no choice");
            assert!(
                !row.detail.contains("arrives with the console session"),
                "{label} still carries the old placeholder copy"
            );
        }
    }

    #[test]
    fn the_power_rows_track_the_platform_menu() {
        let actions: Vec<PowerAction> = power_rows().into_iter().map(|(action, _, _)| action).collect();
        assert_eq!(actions, PowerAction::MENU);
    }

    #[test]
    fn navigation_moves_the_ring_and_wraps() {
        let mut overlay = Overlay::build(&console());
        let last = overlay.entries().len() - 1;

        assert!(overlay.move_focus(Action::NavigateDown));
        assert_eq!(overlay.focused(), Some(1));
        // A non-navigation action never moves focus.
        assert!(!overlay.move_focus(Action::Confirm));
        assert!(!overlay.move_focus(Action::Back));
        assert_eq!(overlay.focused(), Some(1));

        // Up from the first row wraps to the last (a loop, not a trap).
        overlay.focus = Some(0);
        assert!(overlay.move_focus(Action::NavigateUp));
        assert_eq!(overlay.focused(), Some(last));

        // Down past the last row returns to the top.
        assert!(overlay.move_focus(Action::NavigateDown));
        assert_eq!(overlay.focused(), Some(0));
    }

    #[test]
    fn a_disabled_row_offers_no_choice() {
        let mut overlay = Overlay::build(&console());
        // Walk to the first power row (right after the shell row).
        let power = overlay
            .entries()
            .iter()
            .position(|entry| !entry.is_enabled())
            .unwrap();
        overlay.focus = Some(power);
        assert_eq!(overlay.focused_choice(), None);
        assert!(overlay.focused_entry().is_some());
    }

    #[test]
    fn the_clock_is_padded() {
        let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap()
            .to_offset(time::UtcOffset::UTC);
        assert_eq!(format_clock(now), "22:13");
    }
}
