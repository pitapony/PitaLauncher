//! The abstract actions the UI reacts to.
//!
//! A glyph is not an action: the same [`Action`] can come from `A`, from `Enter`
//! and from a controller's south button, and the focus graph only ever sees the
//! action. Keeping the set small and closed is what lets the console shell and
//! the desktop shell share one navigation model.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An action a shell can perform, independent of the physical control bound to
/// it.
///
/// The set is fixed, not extensible: a screen reacts to these and nothing else,
/// which is what keeps a controller-only shell completable and the remap UI
/// finite. `Ord` is derived so a `BTreeMap<Action, _>` (the binding table) has a
/// stable order in a config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Move focus one cell up.
    NavigateUp,
    /// Move focus one cell down.
    NavigateDown,
    /// Move focus one cell left.
    NavigateLeft,
    /// Move focus one cell right.
    NavigateRight,
    /// Activate the focused item.
    Confirm,
    /// Leave the current screen or dialog.
    Back,
    /// Open the focused item's context menu.
    Context,
    /// Open search.
    Search,
    /// Open the shell menu or overlay.
    Menu,
    /// Open the system overlay (the console shell's `Guide`).
    Guide,
    /// Switch to the next tab or shelf.
    TabNext,
    /// Switch to the previous tab or shelf.
    TabPrev,
}

impl Action {
    /// Every action, in a stable order.
    ///
    /// The remap UI and the config file both iterate this, so a new action can
    /// never be forgotten in one place.
    pub const ALL: [Action; 12] = [
        Action::NavigateUp,
        Action::NavigateDown,
        Action::NavigateLeft,
        Action::NavigateRight,
        Action::Confirm,
        Action::Back,
        Action::Context,
        Action::Search,
        Action::Menu,
        Action::Guide,
        Action::TabNext,
        Action::TabPrev,
    ];

    /// The stable wire name, used as the key in `input.toml`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NavigateUp => "navigate_up",
            Self::NavigateDown => "navigate_down",
            Self::NavigateLeft => "navigate_left",
            Self::NavigateRight => "navigate_right",
            Self::Confirm => "confirm",
            Self::Back => "back",
            Self::Context => "context",
            Self::Search => "search",
            Self::Menu => "menu",
            Self::Guide => "guide",
            Self::TabNext => "tab_next",
            Self::TabPrev => "tab_prev",
        }
    }

    /// Whether holding this action should repeat it.
    ///
    /// Only movement repeats: holding `Back` or `Confirm` must stay one action,
    /// or a stuck button would walk through a stack of screens. The repeat
    /// timings themselves live on the device's [`InputMap`](crate::InputMap).
    pub const fn repeats(self) -> bool {
        matches!(
            self,
            Self::NavigateUp
                | Self::NavigateDown
                | Self::NavigateLeft
                | Self::NavigateRight
                | Self::TabNext
                | Self::TabPrev
        )
    }

    /// Whether this action moves focus.
    ///
    /// The focus graph only resolves these four; every other action is for the
    /// screen the focused cell belongs to.
    pub const fn is_navigation(self) -> bool {
        matches!(
            self,
            Self::NavigateUp | Self::NavigateDown | Self::NavigateLeft | Self::NavigateRight
        )
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Action {
    type Err = UnknownAction;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Action::ALL
            .into_iter()
            .find(|action| action.as_str() == value)
            .ok_or_else(|| UnknownAction(value.to_string()))
    }
}

/// An action name in a config file is not one we know.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown action {0:?}")]
pub struct UnknownAction(pub String);

/// Whether an [`ActionEvent`] began or ended a hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// The action became active.
    Pressed,
    /// The action stopped being active.
    Released,
}

/// One action transition, as produced by [`Input`](crate::Input).
///
/// `repeat` is true only for a `Pressed` a repeat timer generated while the
/// control stayed held; the first press is never a repeat, and a `Released` is
/// never a repeat, so a caller that only cares about discrete presses can filter
/// `repeat` out and not care about the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionEvent {
    pub action: Action,
    pub kind: ActionKind,
    pub repeat: bool,
}

impl ActionEvent {
    /// A discrete transition (a fresh press or a release).
    pub const fn new(action: Action, kind: ActionKind) -> Self {
        Self {
            action,
            kind,
            repeat: false,
        }
    }

    /// A repeated press from a held direction.
    pub const fn repeated(action: Action) -> Self {
        Self {
            action,
            kind: ActionKind::Pressed,
            repeat: true,
        }
    }

    /// Whether this transition starts the action (a repeat included).
    pub const fn is_press(&self) -> bool {
        matches!(self.kind, ActionKind::Pressed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_round_trips_through_its_wire_name() {
        for action in Action::ALL {
            assert_eq!(Action::from_str(action.as_str()).unwrap(), action);
        }
        assert_eq!(
            Action::from_str("teleport").unwrap_err(),
            UnknownAction("teleport".into())
        );
    }

    #[test]
    fn only_movement_actions_repeat() {
        for action in Action::ALL {
            let movement = matches!(
                action,
                Action::NavigateUp
                    | Action::NavigateDown
                    | Action::NavigateLeft
                    | Action::NavigateRight
                    | Action::TabNext
                    | Action::TabPrev
            );
            assert_eq!(action.repeats(), movement, "{action}");
        }
        assert!(!Action::Confirm.repeats());
        assert!(!Action::Guide.repeats());
    }

    #[test]
    fn wire_names_are_snake_case_and_join_cleanly() {
        // The names are TOML keys, so they must not need quoting.
        for action in Action::ALL {
            assert!(
                action
                    .as_str()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{}",
                action.as_str()
            );
        }
        assert_eq!(Action::NavigateUp.to_string(), "navigate_up");
        assert_eq!(Action::TabPrev.to_string(), "tab_prev");
    }
}
