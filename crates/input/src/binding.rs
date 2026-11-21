//! Physical controls and the bindings that map them to actions.

use serde::{Deserialize, Serialize};

/// A gamepad button.
///
/// The names mirror `gilrs`'s `Button` one-for-one, but they are **our** type so
/// a config file is not tied to a `gilrs` release and so the mapping can be
/// tested without a device. `gilrs::Button::LeftTrigger` is SDL's
/// `leftshoulder` (the bumper) while `gilrs::Button::LeftTrigger2` is SDL's
/// `lefttrigger` (the analog trigger), which the names do not suggest — this enum
/// uses the SDL reading, so [`Button::LeftBumper`] is the shoulder button and
/// [`Button::LeftTrigger`] is the analog one, and the conversion in
/// [`crate::GilrsSource`] resolves the swap once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    /// The bottom face button (`A` on an Xbox pad, `Cross` on a DualSense).
    South,
    /// The right face button (`B`, `Circle`).
    East,
    /// The left face button (`X`, `Square`).
    West,
    /// The top face button (`Y`, `Triangle`).
    North,
    /// The left shoulder button (`LB`/`L1`).
    LeftBumper,
    /// The right shoulder button (`RB`/`R1`).
    RightBumper,
    /// The left analog trigger (`LT`/`L2`).
    LeftTrigger,
    /// The right analog trigger (`RT`/`R2`).
    RightTrigger,
    /// `Back`/`Share`/`Create`.
    Select,
    /// `Start`/`Options`.
    Start,
    /// The guide/home button.
    Mode,
    /// Pressing the left stick.
    LeftThumb,
    /// Pressing the right stick.
    RightThumb,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
}

impl Button {
    /// Every button, in a stable order, for the remap UI.
    pub const ALL: [Button; 17] = [
        Button::South,
        Button::East,
        Button::West,
        Button::North,
        Button::LeftBumper,
        Button::RightBumper,
        Button::LeftTrigger,
        Button::RightTrigger,
        Button::Select,
        Button::Start,
        Button::Mode,
        Button::LeftThumb,
        Button::RightThumb,
        Button::DPadUp,
        Button::DPadDown,
        Button::DPadLeft,
        Button::DPadRight,
    ];
}

/// A gamepad axis, mirroring `gilrs`'s `Axis`.
///
/// `LeftZ`/`RightZ` are the analog triggers (range `0.0..=1.0`); the stick and
/// d-pad axes are `-1.0..=1.0`. The sign convention is SDL's: positive `Y` is
/// *down*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    LeftStickX,
    LeftStickY,
    LeftZ,
    RightStickX,
    RightStickY,
    RightZ,
    DpadX,
    DpadY,
}

/// One of the two analog sticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stick {
    Left,
    Right,
}

/// A cardinal direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// A keyboard key the launcher can bind.
///
/// Deliberately small: the shell only uses navigation and a couple of shortcuts,
/// and a full key table would have to be kept in sync with whatever the webview
/// reports. The app translates a Dioxus key event into one of these, and an
/// unknown key becomes no action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Enter,
    Escape,
    Tab,
    Space,
    W,
    A,
    S,
    D,
    Q,
    E,
    F,
    /// Opens the shell's menu/overlay (the pad's `Start`).
    M,
    /// The keyboard's shell toggle. There is no `F11` gamepad equivalent; the
    /// pad's `Guide` button drives the same action.
    F11,
}

/// One physical control a binding can read.
///
/// A binding is not an action: several bindings can drive the same action (a
/// d-pad and a stick both move focus), which is why an [`InputMap`](crate::InputMap)
/// holds a *list* of bindings per action.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Binding {
    /// A button is held.
    Button { button: Button },
    /// A keyboard key is held.
    Key { key: Key },
    /// A single axis (a trigger, or a d-pad reported as axes) is pushed past the
    /// deadzone in one direction. The deadzone is the map's; a trigger usually
    /// wants a lower one, since its whole range is `0.0..=1.0`.
    Axis { axis: Axis, positive: bool },
    /// An analog stick is pushed toward one direction. The mapper applies the
    /// radial deadzone to the stick as a whole and then picks the dominant axis,
    /// so a diagonal push moves one cell rather than two.
    Stick { stick: Stick, direction: Direction },
}

impl Binding {
    pub const fn button(button: Button) -> Self {
        Self::Button { button }
    }

    pub const fn key(key: Key) -> Self {
        Self::Key { key }
    }

    pub const fn axis(axis: Axis, positive: bool) -> Self {
        Self::Axis { axis, positive }
    }

    pub const fn stick(stick: Stick, direction: Direction) -> Self {
        Self::Stick { stick, direction }
    }

    /// The button this binding reads, if it is a button.
    pub const fn as_button(&self) -> Option<Button> {
        match self {
            Self::Button { button } => Some(*button),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binding_round_trips_through_toml() {
        let binding = Binding::stick(Stick::Left, Direction::Up);
        let text = toml::to_string(&binding).unwrap();
        let parsed: Binding = toml::from_str(&text).unwrap();
        assert_eq!(parsed, binding);
        assert!(text.contains("kind = \"stick\""), "{text}");
        assert!(text.contains("stick = \"left\""), "{text}");
    }

    #[test]
    fn an_axis_binding_remembers_its_direction() {
        let left = Binding::axis(Axis::DpadX, false);
        let right = Binding::axis(Axis::DpadX, true);
        assert_ne!(left, right);
        assert_eq!(left.as_button(), None);
        assert_eq!(
            Binding::button(Button::South).as_button(),
            Some(Button::South)
        );
    }

    #[test]
    fn every_button_is_listed_once() {
        let mut seen = std::collections::BTreeSet::new();
        for button in Button::ALL {
            assert!(seen.insert(button), "{button:?} listed twice");
        }
        assert_eq!(seen.len(), Button::ALL.len());
    }
}
