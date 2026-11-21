//! Per-device bindings, the deadzone and the repeat timing, and their file.
//!
//! A map is keyed by the device's SDL GUID rather than its session id, so a
//! remap survives a reconnect, and by the literal `"keyboard"` for the keyboard.
//! Only *overrides* are stored: a device with no entry uses the built-in gamepad
//! (or keyboard) layout, which is why a fresh install has no `input.toml` at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::action::Action;
use crate::binding::{Binding, Button, Direction, Key, Stick};

/// The radial deadzone a stick must clear before it moves focus.
pub const DEFAULT_DEADZONE: f32 = 0.35;

/// How long a direction must be held before it starts repeating.
pub const DEFAULT_REPEAT_DELAY_MS: u32 = 400;

/// How often a held direction repeats after the delay.
pub const DEFAULT_REPEAT_INTERVAL_MS: u32 = 80;

/// The map key for the keyboard, which has no GUID.
pub const KEYBOARD_DEVICE: &str = "keyboard";

const fn default_deadzone() -> f32 {
    DEFAULT_DEADZONE
}

const fn default_repeat_delay() -> u32 {
    DEFAULT_REPEAT_DELAY_MS
}

const fn default_repeat_interval() -> u32 {
    DEFAULT_REPEAT_INTERVAL_MS
}

/// One device's bindings and feel.
///
/// An action maps to a *list* of bindings so both a d-pad and a stick can drive
/// `NavigateUp`; the mapper presses the action when any of them is satisfied and
/// releases it only when none is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputMap {
    /// The SDL GUID a gamepad is stored under, or [`KEYBOARD_DEVICE`].
    #[serde(default)]
    pub device: String,
    /// Bindings per action. A missing action is unbound on this device.
    #[serde(default)]
    pub bindings: BTreeMap<Action, Vec<Binding>>,
    /// Radial deadzone for the analog sticks.
    #[serde(default = "default_deadzone")]
    pub deadzone: f32,
    /// Hold time before a direction repeats, in milliseconds.
    #[serde(default = "default_repeat_delay")]
    pub repeat_delay_ms: u32,
    /// Repeat period once it starts, in milliseconds.
    #[serde(default = "default_repeat_interval")]
    pub repeat_interval_ms: u32,
}

impl InputMap {
    /// An empty map for a device, with the default feel.
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            bindings: BTreeMap::new(),
            deadzone: DEFAULT_DEADZONE,
            repeat_delay_ms: DEFAULT_REPEAT_DELAY_MS,
            repeat_interval_ms: DEFAULT_REPEAT_INTERVAL_MS,
        }
    }

    /// The built-in gamepad layout: d-pad *and* left stick move focus, face
    /// buttons are Confirm/Back/Context/Search, Start is Menu, Mode is Guide and
    /// the bumpers switch tabs.
    pub fn gamepad(guid: impl Into<String>) -> Self {
        let mut map = Self::new(guid);
        map.bind(Action::NavigateUp, Binding::button(Button::DPadUp));
        map.add_binding(
            Action::NavigateUp,
            Binding::stick(Stick::Left, Direction::Up),
        );
        map.bind(Action::NavigateDown, Binding::button(Button::DPadDown));
        map.add_binding(
            Action::NavigateDown,
            Binding::stick(Stick::Left, Direction::Down),
        );
        map.bind(Action::NavigateLeft, Binding::button(Button::DPadLeft));
        map.add_binding(
            Action::NavigateLeft,
            Binding::stick(Stick::Left, Direction::Left),
        );
        map.bind(Action::NavigateRight, Binding::button(Button::DPadRight));
        map.add_binding(
            Action::NavigateRight,
            Binding::stick(Stick::Left, Direction::Right),
        );
        map.bind(Action::Confirm, Binding::button(Button::South));
        map.bind(Action::Back, Binding::button(Button::East));
        map.bind(Action::Context, Binding::button(Button::West));
        map.bind(Action::Search, Binding::button(Button::North));
        map.bind(Action::Menu, Binding::button(Button::Start));
        map.bind(Action::Guide, Binding::button(Button::Mode));
        map.bind(Action::TabNext, Binding::button(Button::RightBumper));
        map.bind(Action::TabPrev, Binding::button(Button::LeftBumper));
        map
    }

    /// The built-in keyboard layout: arrows *and* WASD move focus, `Enter`/`Space`
    /// confirm, `Esc` goes back, `Tab`/`E` and `Q` switch tabs, `M` opens the
    /// shell menu (the pad's `Start`), `F11` switches shells (the pad's `Guide`)
    /// and `F` opens the on-screen keyboard (the pad's `Search`).
    pub fn keyboard() -> Self {
        let mut map = Self::new(KEYBOARD_DEVICE);
        map.bind(Action::NavigateUp, Binding::key(Key::ArrowUp));
        map.add_binding(Action::NavigateUp, Binding::key(Key::W));
        map.bind(Action::NavigateDown, Binding::key(Key::ArrowDown));
        map.add_binding(Action::NavigateDown, Binding::key(Key::S));
        map.bind(Action::NavigateLeft, Binding::key(Key::ArrowLeft));
        map.add_binding(Action::NavigateLeft, Binding::key(Key::A));
        map.bind(Action::NavigateRight, Binding::key(Key::ArrowRight));
        map.add_binding(Action::NavigateRight, Binding::key(Key::D));
        map.bind(Action::Confirm, Binding::key(Key::Enter));
        map.add_binding(Action::Confirm, Binding::key(Key::Space));
        map.bind(Action::Back, Binding::key(Key::Escape));
        // `F` is "find": the keyboard's text-entry button. While a text field
        // has focus the app's pump yields typing to the field, so an `f` still
        // types an `f` there.
        map.bind(Action::Search, Binding::key(Key::F));
        map.bind(Action::TabNext, Binding::key(Key::Tab));
        map.add_binding(Action::TabNext, Binding::key(Key::E));
        map.bind(Action::TabPrev, Binding::key(Key::Q));
        // `M` opens the shell menu, the keyboard's answer to the pad's `Start`
        // (the console overlay). A focused text field still gets the `m`.
        map.bind(Action::Menu, Binding::key(Key::M));
        // `F11` switches shells, the keyboard's answer to the pad's `Guide`.
        map.bind(Action::Guide, Binding::key(Key::F11));
        map
    }

    /// The bindings for an action, empty when it is unbound.
    pub fn binding(&self, action: Action) -> &[Binding] {
        self.bindings.get(&action).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The action a keyboard key drives on this map, if any.
    ///
    /// A key is only ever read from the keyboard map (device [`KEYBOARD_DEVICE`]);
    /// the GUI uses this to translate a window key event into the same action a
    /// controller would emit, so a remap of `Enter`/`Space` moves the focus
    /// graph without the GUI hard-coding the bindings. When a hand-edited file
    /// binds one key twice, [`Action::ALL`] order decides, deterministically.
    pub fn action_for_key(&self, key: Key) -> Option<Action> {
        Action::ALL.into_iter().find(|action| {
            self.binding(*action)
                .iter()
                .any(|binding| matches!(binding, Binding::Key { key: bound } if *bound == key))
        })
    }

    /// Whether an action has any binding on this device.
    pub fn is_bound(&self, action: Action) -> bool {
        !self.binding(action).is_empty()
    }

    /// Replace an action's bindings with a single one.
    pub fn bind(&mut self, action: Action, binding: Binding) {
        self.bindings.insert(action, vec![binding]);
    }

    /// Add another binding to an action.
    pub fn add_binding(&mut self, action: Action, binding: Binding) {
        self.bindings.entry(action).or_default().push(binding);
    }

    /// Unbind an action entirely.
    pub fn unbind(&mut self, action: Action) {
        self.bindings.remove(&action);
    }

    /// Replace every binding of an action; an empty list unbinds it.
    ///
    /// This is the remap UI's "reset" and "clear": one call sets the whole
    /// slot list without going through `bind`/`add_binding` one at a time.
    pub fn set_bindings(&mut self, action: Action, bindings: Vec<Binding>) {
        if bindings.is_empty() {
            self.bindings.remove(&action);
        } else {
            self.bindings.insert(action, bindings);
        }
    }

    /// Replace one slot of an action's bindings.
    ///
    /// Returns `false` when the action has no such slot, so a remap UI holding
    /// a stale index cannot silently corrupt the list.
    pub fn set_binding(&mut self, action: Action, index: usize, binding: Binding) -> bool {
        let Some(list) = self.bindings.get_mut(&action) else {
            return false;
        };
        let Some(slot) = list.get_mut(index) else {
            return false;
        };
        *slot = binding;
        true
    }

    /// Remove one slot of an action's bindings.
    ///
    /// An action left with no bindings is dropped, so [`InputMap::is_bound`]
    /// stays honest rather than reporting a bound-but-empty action.
    pub fn remove_binding(&mut self, action: Action, index: usize) -> Option<Binding> {
        let list = self.bindings.get_mut(&action)?;
        if index >= list.len() {
            return None;
        }
        let removed = list.remove(index);
        if list.is_empty() {
            self.bindings.remove(&action);
        }
        Some(removed)
    }

    /// Keep `binding` only on `action`, dropping it from every other action and
    /// every other slot of the same action.
    ///
    /// The map itself allows one control to drive two actions (a hand-edited
    /// file can say so), but a remap UI should not make that easy: a capture
    /// steals its control rather than duplicating it.
    pub fn dedup_binding(&mut self, action: Action, binding: &Binding) {
        for (other, list) in self.bindings.iter_mut() {
            if *other == action {
                let mut kept = false;
                list.retain(|candidate| {
                    if candidate == binding {
                        if kept {
                            return false;
                        }
                        kept = true;
                    }
                    true
                });
            } else {
                list.retain(|candidate| candidate != binding);
            }
        }
        self.bindings.retain(|_, list| !list.is_empty());
    }

    /// Actions and their bindings, in [`Action::ALL`] order.
    pub fn iter(&self) -> impl Iterator<Item = (Action, &[Binding])> {
        Action::ALL
            .into_iter()
            .filter_map(|action| self.bindings.get(&action).map(|b| (action, b.as_slice())))
    }

    /// Check the map's numbers, so a hand-edited file cannot make the mapper
    /// behave oddly.
    pub fn validate(&self) -> Result<(), InputMapsError> {
        if !(0.0..=1.0).contains(&self.deadzone) {
            return Err(InputMapsError::Deadzone {
                device: self.device.clone(),
                value: self.deadzone,
            });
        }
        if self.repeat_interval_ms == 0 {
            return Err(InputMapsError::RepeatInterval {
                device: self.device.clone(),
            });
        }
        Ok(())
    }
}

impl Default for InputMap {
    fn default() -> Self {
        Self::new("")
    }
}

/// Every persisted map, keyed by device GUID.
///
/// The file is `<XDG_CONFIG_HOME>/pitagames/input.toml`
/// ([`pitagames_core::paths::input_maps_file`]); it holds only overrides, so a
/// missing file is the normal state and means "use the defaults".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InputMaps {
    #[serde(default)]
    pub maps: BTreeMap<String, InputMap>,
}

impl InputMaps {
    /// An empty set: every device falls back to its default layout.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.maps.is_empty()
    }

    /// The stored map for a device, if it has one.
    pub fn get(&self, device: &str) -> Option<&InputMap> {
        self.maps.get(device)
    }

    /// Store (or replace) a device's map, keyed by its own `device` field.
    pub fn insert(&mut self, map: InputMap) {
        self.maps.insert(map.device.clone(), map);
    }

    pub fn remove(&mut self, device: &str) -> Option<InputMap> {
        self.maps.remove(device)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &InputMap)> {
        self.maps.iter().map(|(key, map)| (key.as_str(), map))
    }

    /// The map to use for a device: its override, else the built-in layout for
    /// its kind.
    pub fn for_device(&self, device: &str, is_gamepad: bool) -> InputMap {
        if let Some(map) = self.get(device) {
            return map.clone();
        }
        if is_gamepad {
            InputMap::gamepad(device)
        } else {
            let mut map = InputMap::keyboard();
            map.device = device.to_string();
            map
        }
    }

    /// The keyboard map: its override, else the built-in layout.
    pub fn keyboard(&self) -> InputMap {
        self.get(KEYBOARD_DEVICE)
            .cloned()
            .unwrap_or_else(InputMap::keyboard)
    }

    /// Validate every stored map.
    pub fn validate(&self) -> Result<(), InputMapsError> {
        for map in self.maps.values() {
            map.validate()?;
        }
        Ok(())
    }

    /// Serialize to TOML.
    pub fn to_toml(&self) -> Result<String, InputMapsError> {
        self.validate()?;
        Ok(toml::to_string_pretty(self)?)
    }

    /// Parse and validate TOML.
    pub fn from_toml(text: &str) -> Result<Self, InputMapsError> {
        let maps: InputMaps = toml::from_str(text)?;
        maps.validate()?;
        Ok(maps)
    }

    /// Read the maps from a file. A missing file is an empty set, not an error:
    /// nothing has been remapped yet.
    pub fn load(path: &Path) -> Result<Self, InputMapsError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(err) => Err(InputMapsError::Io {
                path: path.to_path_buf(),
                source: err,
            }),
        }
    }

    /// Write the maps, atomically so a crash cannot leave a half-written file.
    pub fn save(&self, path: &Path) -> Result<(), InputMapsError> {
        let text = self.to_toml()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| InputMapsError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|source| InputMapsError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| InputMapsError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Load from the default XDG path, if there is one.
    pub fn load_default() -> Result<Self, InputMapsError> {
        match pitagames_core::paths::input_maps_file() {
            Some(path) => Self::load(&path),
            None => Ok(Self::new()),
        }
    }

    /// Save to the default XDG path. A machine with no `HOME` is refused rather
    /// than written somewhere surprising.
    pub fn save_default(&self) -> Result<(), InputMapsError> {
        let path = pitagames_core::paths::input_maps_file().ok_or(InputMapsError::NoConfigDir)?;
        self.save(&path)
    }
}

/// Something is wrong with the stored input maps.
#[derive(Debug, thiserror::Error)]
pub enum InputMapsError {
    #[error("input maps file {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("input maps: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("input maps: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("{device}: deadzone {value} is outside 0.0..=1.0")]
    Deadzone { device: String, value: f32 },
    #[error("{device}: repeat interval must not be zero")]
    RepeatInterval { device: String },
    #[error("no config directory (HOME is unset)")]
    NoConfigDir,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gamepad_default_binds_both_the_dpad_and_the_stick() {
        let map = InputMap::gamepad("guid-1");
        assert_eq!(
            map.binding(Action::NavigateUp),
            &[
                Binding::button(Button::DPadUp),
                Binding::stick(Stick::Left, Direction::Up)
            ]
        );
        assert_eq!(
            map.binding(Action::Confirm),
            &[Binding::button(Button::South)]
        );
        assert_eq!(map.binding(Action::Back), &[Binding::button(Button::East)]);
        assert_eq!(map.binding(Action::Guide), &[Binding::button(Button::Mode)]);
        assert_eq!(map.deadzone, DEFAULT_DEADZONE);
        // Nothing is bound to a keyboard key on a pad.
        for action in Action::ALL {
            for binding in map.binding(action) {
                assert!(!matches!(binding, Binding::Key { .. }), "{binding:?}");
            }
        }
        map.validate().unwrap();
    }

    #[test]
    fn the_keyboard_default_binds_arrows_and_wasd_to_the_same_actions() {
        let map = InputMap::keyboard();
        assert_eq!(
            map.binding(Action::NavigateDown),
            &[Binding::key(Key::ArrowDown), Binding::key(Key::S)]
        );
        assert_eq!(
            map.binding(Action::Confirm),
            &[Binding::key(Key::Enter), Binding::key(Key::Space)]
        );
        assert_eq!(map.binding(Action::Back), &[Binding::key(Key::Escape)]);
        // `M` opens the shell menu and `F11` is the shell toggle: the keyboard's
        // answers to the pad's `Start` and `Guide`.
        assert_eq!(map.binding(Action::Menu), &[Binding::key(Key::M)]);
        assert_eq!(map.binding(Action::Guide), &[Binding::key(Key::F11)]);
        map.validate().unwrap();
    }

    #[test]
    fn a_key_resolves_to_the_action_the_keyboard_layout_binds() {
        let map = InputMap::keyboard();
        assert_eq!(map.action_for_key(Key::ArrowUp), Some(Action::NavigateUp));
        assert_eq!(map.action_for_key(Key::W), Some(Action::NavigateUp));
        assert_eq!(
            map.action_for_key(Key::ArrowRight),
            Some(Action::NavigateRight)
        );
        assert_eq!(map.action_for_key(Key::Enter), Some(Action::Confirm));
        assert_eq!(map.action_for_key(Key::Space), Some(Action::Confirm));
        assert_eq!(map.action_for_key(Key::Escape), Some(Action::Back));
        assert_eq!(map.action_for_key(Key::M), Some(Action::Menu));
        assert_eq!(map.action_for_key(Key::F11), Some(Action::Guide));
        assert_eq!(map.action_for_key(Key::F), Some(Action::Search));
        // A hand-edited duplicate is deterministic: the earliest action wins.
        let mut edited = map.clone();
        edited.add_binding(Action::TabPrev, Binding::key(Key::Enter));
        assert_eq!(edited.action_for_key(Key::Enter), Some(Action::Confirm));
    }

    #[test]
    fn a_bound_action_lists_in_the_action_order() {
        let map = InputMap::keyboard();
        let actions: Vec<Action> = map.iter().map(|(action, _)| action).collect();
        let mut sorted = actions.clone();
        sorted.sort();
        assert_eq!(actions, sorted);
    }

    #[test]
    fn a_device_without_an_override_falls_back_to_its_kind() {
        let maps = InputMaps::new();
        let pad = maps.for_device("unknown-guid", true);
        assert_eq!(
            pad.binding(Action::Confirm),
            &[Binding::button(Button::South)]
        );
        assert_eq!(pad.device, "unknown-guid");

        let other = maps.for_device("some-joystick", false);
        assert_eq!(
            other.binding(Action::Confirm),
            &[Binding::key(Key::Enter), Binding::key(Key::Space)]
        );
        assert_eq!(other.device, "some-joystick");
        assert_eq!(maps.keyboard().device, KEYBOARD_DEVICE);
    }

    #[test]
    fn an_override_replaces_the_default() {
        let mut maps = InputMaps::new();
        let mut map = InputMap::gamepad("pad-a");
        map.bind(Action::Confirm, Binding::button(Button::North));
        maps.insert(map);

        let resolved = maps.for_device("pad-a", true);
        assert_eq!(
            resolved.binding(Action::Confirm),
            &[Binding::button(Button::North)]
        );
        // A different device is untouched.
        assert_eq!(
            maps.for_device("pad-b", true).binding(Action::Confirm),
            &[Binding::button(Button::South)]
        );
    }

    #[test]
    fn maps_round_trip_through_toml() {
        let mut maps = InputMaps::new();
        let mut map = InputMap::gamepad("03000000deadbeef");
        map.bind(Action::Menu, Binding::button(Button::Select));
        map.deadzone = 0.4;
        maps.insert(map);
        maps.insert(InputMap::keyboard());

        let text = maps.to_toml().unwrap();
        let parsed = InputMaps::from_toml(&text).unwrap();
        assert_eq!(parsed, maps);
        // The action keys are the wire names, and the device is a table key.
        assert!(text.contains("navigate_up"), "{text}");
        assert!(text.contains("03000000deadbeef"), "{text}");
        assert!(text.contains("[maps.keyboard]"), "{text}");
    }

    #[test]
    fn a_hand_edited_file_is_validated() {
        let bad = "[maps.pad]\ndevice = \"pad\"\ndeadzone = 3.0\n";
        assert!(matches!(
            InputMaps::from_toml(bad),
            Err(InputMapsError::Deadzone { .. })
        ));

        let zero = "[maps.pad]\ndevice = \"pad\"\nrepeat_interval_ms = 0\n";
        assert!(matches!(
            InputMaps::from_toml(zero),
            Err(InputMapsError::RepeatInterval { .. })
        ));
    }

    #[test]
    fn a_missing_file_is_an_empty_set_and_a_save_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/input.toml");

        let empty = InputMaps::load(&path).unwrap();
        assert!(empty.is_empty());

        let mut maps = InputMaps::new();
        let mut map = InputMap::keyboard();
        map.bind(Action::Guide, Binding::key(Key::F));
        maps.insert(map);
        maps.save(&path).unwrap();

        let loaded = InputMaps::load(&path).unwrap();
        assert_eq!(loaded, maps);
        // No leftover temp file.
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn a_slot_can_be_replaced_removed_and_reset() {
        let mut map = InputMap::gamepad("pad");
        // Replacing a slot keeps the others.
        assert!(map.set_binding(Action::NavigateUp, 0, Binding::button(Button::North)));
        assert_eq!(
            map.binding(Action::NavigateUp),
            &[
                Binding::button(Button::North),
                Binding::stick(Stick::Left, Direction::Up)
            ]
        );
        // A stale index is refused, not clamped.
        assert!(!map.set_binding(Action::Confirm, 99, Binding::button(Button::North)));

        // Removing one of two bindings keeps the action bound.
        assert_eq!(
            map.remove_binding(Action::NavigateUp, 0),
            Some(Binding::button(Button::North))
        );
        assert_eq!(
            map.binding(Action::NavigateUp),
            &[Binding::stick(Stick::Left, Direction::Up)]
        );
        // Removing the last one drops the action entirely.
        map.remove_binding(Action::NavigateUp, 0);
        assert!(!map.is_bound(Action::NavigateUp));
        assert_eq!(map.remove_binding(Action::NavigateUp, 0), None);

        // Setting a whole list at once; an empty list unbinds.
        map.set_bindings(
            Action::Confirm,
            vec![Binding::key(Key::Enter), Binding::key(Key::Space)],
        );
        assert_eq!(map.binding(Action::Confirm).len(), 2);
        map.set_bindings(Action::Confirm, Vec::new());
        assert!(!map.is_bound(Action::Confirm));
    }

    #[test]
    fn capturing_a_control_steals_it_from_the_other_action() {
        let mut map = InputMap::gamepad("pad");
        // Rebinding Back to `South` (Confirm's button) makes Confirm lose it.
        map.set_bindings(Action::Back, vec![Binding::button(Button::South)]);
        map.dedup_binding(Action::Back, &Binding::button(Button::South));
        assert!(!map.is_bound(Action::Confirm));
        assert_eq!(map.binding(Action::Back), &[Binding::button(Button::South)]);
        // Duplicates inside one action collapse to the first slot.
        map.set_bindings(
            Action::Back,
            vec![
                Binding::button(Button::South),
                Binding::button(Button::South),
                Binding::button(Button::East),
            ],
        );
        map.dedup_binding(Action::Back, &Binding::button(Button::South));
        assert_eq!(
            map.binding(Action::Back),
            &[
                Binding::button(Button::South),
                Binding::button(Button::East)
            ]
        );
    }
}
