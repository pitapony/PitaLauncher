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
}

/// The actions a footer names, in the order they read.
///
/// A screen that hides some of them (a dialog with no `Menu`) filters this list
/// rather than inventing its own order.
pub const HINT_ACTIONS: [Action; 5] = [
    Action::Confirm,
    Action::Back,
    Action::Search,
    Action::Menu,
    Action::Guide,
];

/// The hint row for a map: each action's first binding, labelled.
///
/// An action the map leaves unbound is skipped, so a row never claims a key a
/// remap removed. A binding with no short label (none in the built-in layouts)
/// is skipped too.
pub fn hints(map: &InputMap) -> Vec<Hint> {
    HINT_ACTIONS
        .iter()
        .filter_map(|&action| {
            map.binding(action)
                .first()
                .and_then(binding_label)
                .map(|label| Hint { action, label })
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
    fn the_keyboard_hint_row_skips_what_it_does_not_bind() {
        let map = InputMap::keyboard();
        let row = hints(&map);
        // The keyboard layout binds Confirm, Back, Search and Guide, but not
        // Menu, so that one is absent rather than invented.
        assert_eq!(
            row.iter().map(|hint| hint.action).collect::<Vec<_>>(),
            [Action::Confirm, Action::Back, Action::Search, Action::Guide]
        );
        assert_eq!(row[0].label, "Enter");
        assert_eq!(row[1].label, "Esc");
        assert_eq!(row[2].label, "F");
        assert_eq!(row[3].label, "F11");
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
}
