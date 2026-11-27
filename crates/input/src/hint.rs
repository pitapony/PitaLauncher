//! Glyph hints: the short labels a footer prints for the actions on the device
//! that was last used.
//!
//! A hint is not a glyph and not an action: it is the *display* of an action's
//! first binding. The label comes from the binding itself (`A` for a gamepad's
//! south button, `Enter` for the keyboard's Enter key), so a remap changes the
//! hint without a second table to keep in sync. The console shell has no hover,
//! so every action it offers must be reachable from a visible hint; the desktop
//! shell shows the same row when the pointer is idle.
//!
//! The module is deliberately pure: it takes an [`InputMap`] and returns text.
//! Choosing *which* map (keyboard or gamepad) is the app's job, because only the
//! app knows the last device.

use crate::{Action, Axis, Binding, Button, Direction, InputMap, Key, Stick};

/// One action's display label on the current device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    /// The action the hint names.
    pub action: Action,
    /// The short label printed before the action's name, e.g. `A` or `Enter`.
    pub label: String,
    /// The word the footer prints for the action *on this screen*.
    ///
    /// The default row uses the action's own name (`Confirm`); a screen that
    /// renames an action for what it does there (the game page's `Confirm` is
    /// "Play") supplies its own via [`ScreenHint`].
    pub name: String,
}

/// One action a screen advertises, with the word that screen gives it.
///
/// The action's own name (`Confirm`) is the generic one; a screen names what the
/// action *does there* — the console game page's `Confirm` is "Play" and its
/// `Menu` is "Overlay". Keeping the name beside the action means a screen's row
/// and the action it names cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenHint {
    /// The action the screen exposes.
    pub action: Action,
    /// The word the footer prints for it on this screen.
    pub name: &'static str,
}

impl ScreenHint {
    /// A screen's name for one action.
    pub const fn new(action: Action, name: &'static str) -> Self {
        Self { action, name }
    }
}

/// The actions a footer names, in the order they read.
///
/// A screen that hides some of them (a dialog with no `Menu`) filters this list
/// rather than inventing its own order; [`hints_for`] takes a screen's own
/// subset.
pub const HINT_ACTIONS: [Action; 5] = [
    Action::Confirm,
    Action::Back,
    Action::Search,
    Action::Menu,
    Action::Guide,
];

/// The console game page's cheat-sheet: Play, Graphics, Overlay, Back.
///
/// The page's `Confirm` starts (or installs) the game, `Context` (the pad's
/// `West`) opens its Graphics profile, and `Menu` opens the overlay, so it names
/// each for what it does here rather than by the action's generic word.
pub const GAME_PAGE_HINTS: [ScreenHint; 4] = [
    ScreenHint::new(Action::Confirm, "Play"),
    ScreenHint::new(Action::Context, "Graphics"),
    ScreenHint::new(Action::Menu, "Overlay"),
    ScreenHint::new(Action::Back, "Back"),
];

/// The Graphics screen's cheat-sheet: Choose, Back.
///
/// The screen's `Confirm` presses the focused row; the global `Menu`/`Guide`
/// actions are the shell's, so they are not repeated here.
pub const GRAPHICS_HINTS: [ScreenHint; 2] = [
    ScreenHint::new(Action::Confirm, "Choose"),
    ScreenHint::new(Action::Back, "Back"),
];

/// The default hint row for a map: each action's first binding, labelled.
///
/// An action the map leaves unbound is skipped, so a row never claims a key a
/// remap removed. A binding with no short label (none in the built-in layouts)
/// is skipped too.
pub fn hints(map: &InputMap) -> Vec<Hint> {
    let default: Vec<ScreenHint> = HINT_ACTIONS
        .iter()
        .map(|&action| ScreenHint::new(action, action_label(action)))
        .collect();
    hints_for(map, &default)
}

/// The hint row for a screen's own subset of actions.
///
/// The screen names the actions it exposes, in the order they read, and the word
/// it prints for each. Every row is built the same way as [`hints`]: the action's
/// *first* binding supplies the glyph, an action the map leaves unbound is
/// skipped, and a binding with no short label is skipped. So a remap changes the
/// glyph with no second table, and a screen can advertise `[Play, Overlay, Back]`
/// without the default row's `Search`/`Shell`.
pub fn hints_for(map: &InputMap, screen: &[ScreenHint]) -> Vec<Hint> {
    screen
        .iter()
        .filter_map(|request| {
            map.binding(request.action)
                .first()
                .and_then(binding_label)
                .map(|label| Hint {
                    action: request.action,
                    label,
                    name: request.name.to_string(),
                })
        })
        .collect()
}

/// The human-readable name of an action, as the hint row prints it.
pub fn action_label(action: Action) -> &'static str {
    match action {
        Action::NavigateUp => "Move up",
        Action::NavigateDown => "Move down",
        Action::NavigateLeft => "Move left",
        Action::NavigateRight => "Move right",
        Action::Confirm => "Confirm",
        Action::Back => "Back",
        Action::Context => "Context",
        Action::Search => "Search",
        Action::Menu => "Menu",
        Action::Guide => "Shell",
        Action::TabNext => "Next tab",
        Action::TabPrev => "Previous tab",
    }
}

/// The short label for a binding, or `None` when it has no compact form.
pub fn binding_label(binding: &Binding) -> Option<String> {
    match binding {
        Binding::Button { button } => Some(button_label(*button).to_string()),
        Binding::Key { key } => Some(key_label(*key).to_string()),
        Binding::Axis { axis, positive } => Some(axis_label(*axis, *positive)),
        Binding::Stick { stick, direction } => Some(stick_label(*stick, *direction)),
    }
}

/// The label for a gamepad button.
pub fn button_label(button: Button) -> &'static str {
    match button {
        Button::South => "A",
        Button::East => "B",
        Button::West => "X",
        Button::North => "Y",
        Button::LeftBumper => "LB",
        Button::RightBumper => "RB",
        Button::LeftTrigger => "LT",
        Button::RightTrigger => "RT",
        Button::Select => "Back",
        Button::Start => "Start",
        Button::Mode => "Guide",
        Button::LeftThumb => "L3",
        Button::RightThumb => "R3",
        Button::DPadUp => "↑",
        Button::DPadDown => "↓",
        Button::DPadLeft => "←",
        Button::DPadRight => "→",
    }
}

/// The label for a keyboard key.
pub fn key_label(key: Key) -> &'static str {
    match key {
        Key::ArrowUp => "↑",
        Key::ArrowDown => "↓",
        Key::ArrowLeft => "←",
        Key::ArrowRight => "→",
        Key::Enter => "Enter",
        Key::Escape => "Esc",
        Key::Tab => "Tab",
        Key::Space => "Space",
        Key::W => "W",
        Key::A => "A",
        Key::S => "S",
        Key::D => "D",
        Key::Q => "Q",
        Key::E => "E",
        Key::F => "F",
        Key::M => "M",
        Key::F11 => "F11",
    }
}

/// The label for an axis binding, with its direction.
///
/// An analog trigger reads positive only; the d-pad's axes read both ways and
/// print the matching arrow.
pub fn axis_label(axis: Axis, positive: bool) -> String {
    match axis {
        Axis::LeftZ => "LT".to_string(),
        Axis::RightZ => "RT".to_string(),
        Axis::DpadX => if positive { "→" } else { "←" }.to_string(),
        Axis::DpadY => if positive { "↓" } else { "↑" }.to_string(),
        Axis::LeftStickX | Axis::RightStickX => if positive { "→" } else { "←" }.to_string(),
        Axis::LeftStickY | Axis::RightStickY => if positive { "↓" } else { "↑" }.to_string(),
    }
}

/// The label for a stick direction, e.g. `LS→`.
pub fn stick_label(stick: Stick, direction: Direction) -> String {
    let prefix = match stick {
        Stick::Left => "LS",
        Stick::Right => "RS",
    };
    let arrow = match direction {
        Direction::Up => "↑",
        Direction::Down => "↓",
        Direction::Left => "←",
        Direction::Right => "→",
    };
    format!("{prefix}{arrow}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gamepad_hint_row_names_the_face_buttons() {
        let map = InputMap::gamepad("pad");
        let row = hints(&map);
        let labels: Vec<(&str, &str)> = row
            .iter()
            .map(|hint| (hint.label.as_str(), action_label(hint.action)))
            .collect();
        assert_eq!(
            labels,
            [
                ("A", "Confirm"),
                ("B", "Back"),
                ("Y", "Search"),
                ("Start", "Menu"),
                ("Guide", "Shell"),
            ]
        );
    }

    #[test]
    fn the_keyboard_hint_row_names_the_menu_and_shell_keys() {
        let map = InputMap::keyboard();
        let row = hints(&map);
        // The keyboard layout binds Confirm, Back, Search, Menu and Guide; each
        // is named from its own key.
        assert_eq!(
            row.iter().map(|hint| hint.action).collect::<Vec<_>>(),
            [
                Action::Confirm,
                Action::Back,
                Action::Search,
                Action::Menu,
                Action::Guide
            ]
        );
        assert_eq!(row[0].label, "Enter");
        assert_eq!(row[1].label, "Esc");
        assert_eq!(row[2].label, "F");
        assert_eq!(row[3].label, "M");
        assert_eq!(row[4].label, "F11");
    }

    #[test]
    fn a_stick_binding_prints_its_direction() {
        let binding = Binding::stick(Stick::Left, Direction::Right);
        assert_eq!(binding_label(&binding).as_deref(), Some("LS→"));
    }

    #[test]
    fn an_axis_binding_prints_its_arrow() {
        assert_eq!(axis_label(Axis::DpadX, false), "←");
        assert_eq!(axis_label(Axis::DpadX, true), "→");
        assert_eq!(axis_label(Axis::LeftZ, true), "LT");
    }

    #[test]
    fn every_action_has_a_label() {
        for action in Action::ALL {
            assert!(!action_label(action).is_empty(), "{action:?}");
        }
    }

    #[test]
    fn an_unbound_action_is_skipped() {
        let mut map = InputMap::new("empty");
        map.bind(Action::Confirm, Binding::key(Key::Enter));
        let row = hints(&map);
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].action, Action::Confirm);
    }

    // -- The per-screen subset --------------------------------------------

    #[test]
    fn the_default_row_names_each_action_by_its_own_label() {
        let map = InputMap::gamepad("pad");
        for hint in hints(&map) {
            assert_eq!(hint.name, action_label(hint.action), "{hint:?}");
        }
    }

    #[test]
    fn the_game_page_row_names_play_graphics_overlay_and_back() {
        let map = InputMap::gamepad("pad");
        let row = hints_for(&map, &GAME_PAGE_HINTS);
        let named: Vec<(&str, &str)> = row
            .iter()
            .map(|hint| (hint.label.as_str(), hint.name.as_str()))
            .collect();
        assert_eq!(
            named,
            [
                ("A", "Play"),
                ("X", "Graphics"),
                ("Start", "Overlay"),
                ("B", "Back")
            ],
            "the page's own words, in the order it reads them"
        );
    }

    #[test]
    fn the_graphics_row_names_choose_and_back() {
        let map = InputMap::gamepad("pad");
        let row = hints_for(&map, &GRAPHICS_HINTS);
        let named: Vec<(&str, &str)> = row
            .iter()
            .map(|hint| (hint.label.as_str(), hint.name.as_str()))
            .collect();
        assert_eq!(named, [("A", "Choose"), ("B", "Back")]);

        // The keyboard row names the same two actions with its own keys.
        let keyboard = InputMap::keyboard();
        assert_eq!(
            hints_for(&keyboard, &GRAPHICS_HINTS)
                .iter()
                .map(|hint| hint.label.as_str())
                .collect::<Vec<_>>(),
            ["Enter", "Esc"]
        );
    }

    #[test]
    fn a_screen_subset_drops_the_actions_it_does_not_expose() {
        let map = InputMap::keyboard();
        let row = hints_for(&map, &GAME_PAGE_HINTS);
        assert_eq!(
            row.iter().map(|hint| hint.action).collect::<Vec<_>>(),
            [Action::Confirm, Action::Menu, Action::Back]
        );
        // The default row names Search and the shell key; the game page does not.
        assert!(!row.iter().any(|hint| hint.action == Action::Search));
        assert!(!row.iter().any(|hint| hint.action == Action::Guide));
    }

    #[test]
    fn a_subset_skips_an_action_the_map_leaves_unbound() {
        let mut map = InputMap::new("empty");
        map.bind(Action::Confirm, Binding::key(Key::Enter));
        let row = hints_for(&map, &GAME_PAGE_HINTS);
        assert_eq!(row.len(), 1, "only the one bound action is named");
        assert_eq!(row[0].name, "Play");
        assert_eq!(row[0].label, "Enter");
    }

    #[test]
    fn a_subset_follows_a_remap() {
        let mut map = InputMap::gamepad("pad");
        map.bind(Action::Confirm, Binding::button(Button::East));
        let row = hints_for(&map, &GAME_PAGE_HINTS);
        let play = row
            .iter()
            .find(|hint| hint.action == Action::Confirm)
            .expect("Play is still bound");
        assert_eq!(play.label, "B", "the remapped glyph, not the old one");
        assert_eq!(play.name, "Play", "the screen's word is unchanged");
    }
}
