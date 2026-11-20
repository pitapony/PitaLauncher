//! The remap session: the one writer of the input maps.
//!
//! `input.toml` holds only *overrides*, so the screen that edits bindings is
//! also the only code that writes them. This module is the framework-free half:
//! it starts from a device's stored map (or its built-in layout), applies one
//! edit at a time, and folds the result back into an [`InputMaps`] — dropping
//! the override entirely when the device ends up matching its default, so a file
//! exists only while a device is actually customised.
//!
//! Editing is **slot-based rather than "one control per action"**: a gamepad's
//! `NavigateUp` is deliberately bound to *both* the d-pad and the left stick, so
//! a rebind replaces one slot and a capture can add another. A capture steals its
//! control from the action that previously owned it ([`InputMap::dedup_binding`]).
//!
//! A capture is the "press a control to bind it" step: the session holds the
//! pending slot, the app's keyboard and gamepad pumps hand it the next control,
//! and only a control of the right kind (a pad button for a pad, a key for the
//! keyboard) is accepted — a pad's map never holds a `Key`, and the keyboard's
//! never holds a `Button`, which is the invariant the mapper relies on.

use crate::action::Action;
use crate::binding::Binding;
use crate::hint::{action_label, binding_label};
use crate::map::{InputMap, InputMaps, KEYBOARD_DEVICE};

/// A device a remap session can edit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RemapTarget {
    /// The keyboard, stored under [`KEYBOARD_DEVICE`].
    #[default]
    Keyboard,
    /// One controller, stored under its SDL GUID so a remap survives a replug.
    Pad {
        /// The SDL GUID the map is keyed by.
        guid: String,
        /// The pad's display name, for the screen.
        name: String,
    },
}

impl RemapTarget {
    /// The keyboard target.
    pub fn keyboard() -> Self {
        Self::Keyboard
    }

    /// A controller target from its GUID and display name.
    pub fn pad(guid: impl Into<String>, name: impl Into<String>) -> Self {
        Self::Pad {
            guid: guid.into(),
            name: name.into(),
        }
    }
    /// The key the map is stored under.
    pub fn device(&self) -> &str {
        match self {
            Self::Keyboard => KEYBOARD_DEVICE,
            Self::Pad { guid, .. } => guid,
        }
    }

    /// Whether this is the keyboard.
    pub fn is_keyboard(&self) -> bool {
        matches!(self, Self::Keyboard)
    }

    /// The device's display name.
    pub fn label(&self) -> String {
        match self {
            Self::Keyboard => "Keyboard".to_string(),
            Self::Pad { guid, name } => {
                if name.trim().is_empty() {
                    guid.clone()
                } else {
                    name.clone()
                }
            }
        }
    }
}

/// What the next captured control should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSlot {
    /// Replace a specific binding of an action.
    Replace {
        /// The action being edited.
        action: Action,
        /// The slot in that action's binding list.
        index: usize,
    },
    /// Append a new binding to an action.
    Add {
        /// The action being edited.
        action: Action,
    },
}

impl CaptureSlot {
    /// The action the capture edits.
    pub const fn action(self) -> Action {
        match self {
            Self::Replace { action, .. } | Self::Add { action } => action,
        }
    }

    /// Whether the capture appends rather than replaces.
    pub const fn is_add(self) -> bool {
        matches!(self, Self::Add { .. })
    }
}

/// One editing session over a device's map.
///
/// The session is `Clone + PartialEq` because the app keeps it in a signal; it
/// owns no closure and no handle. Editing is local until [`RemapSession::apply`]
/// folds the map into an [`InputMaps`] for the caller to save.
#[derive(Debug, Clone, PartialEq)]
pub struct RemapSession {
    target: RemapTarget,
    map: InputMap,
    /// The device's built-in layout, for `reset` and `is_customized`.
    default: InputMap,
    capture: Option<CaptureSlot>,
    message: Option<String>,
    error: bool,
}

impl RemapSession {
    /// Start editing `target`, from its stored override or its built-in layout.
    pub fn new(target: RemapTarget, maps: &InputMaps) -> Self {
        let device = target.device();
        let default = if target.is_keyboard() {
            InputMap::keyboard()
        } else {
            InputMap::gamepad(device)
        };
        let mut map = maps.get(device).cloned().unwrap_or_else(|| default.clone());
        // The file keys a map by the map's own `device` field; a hand-edited
        // file could disagree, so the session normalises it to the target.
        map.device = device.to_string();
        Self {
            target,
            map,
            default,
            capture: None,
            message: None,
            error: false,
        }
    }

    /// The device being edited.
    pub fn target(&self) -> &RemapTarget {
        &self.target
    }

    /// The working map, as it will be saved.
    pub fn map(&self) -> &InputMap {
        &self.map
    }

    /// An action's bindings, empty when it is unbound.
    pub fn bindings(&self, action: Action) -> &[Binding] {
        self.map.binding(action)
    }

    /// The capture awaiting a control, if any.
    pub fn capture(&self) -> Option<CaptureSlot> {
        self.capture
    }

    /// Whether a control is being captured.
    pub fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    /// The last status line, e.g. what a capture bound.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Whether the last status line is a refusal.
    pub fn has_error(&self) -> bool {
        self.error
    }

    /// Whether the working map differs from the device's built-in layout.
    pub fn is_customized(&self) -> bool {
        self.map != self.default
    }

    /// Start capturing a control for a slot.
    pub fn begin_capture(&mut self, slot: CaptureSlot) {
        let action = slot.action();
        self.capture = Some(slot);
        self.error = false;
        self.message = Some(if slot.is_add() {
            format!(
                "Press a control to add a binding to {}.",
                action_label(action)
            )
        } else {
            format!("Press a control to rebind {}.", action_label(action))
        });
    }

    /// Give up on a capture without changing anything.
    pub fn cancel_capture(&mut self) {
        self.capture = None;
        self.message = None;
        self.error = false;
    }

    /// Apply the next captured control.
    ///
    /// Returns whether it was accepted. A control of the wrong kind for the
    /// device leaves the capture pending and reports why, so the person can
    /// press the right one.
    pub fn capture_control(&mut self, binding: Binding) -> bool {
        let Some(slot) = self.capture.take() else {
            return false;
        };
        let is_key = matches!(binding, Binding::Key { .. });
        if is_key != self.target.is_keyboard() {
            self.capture = Some(slot);
            self.error = true;
            self.message = Some(if self.target.is_keyboard() {
                "This is the keyboard: press a key.".to_string()
            } else {
                "This is a controller: press a button.".to_string()
            });
            return false;
        }

        let action = slot.action();
        match slot {
            CaptureSlot::Replace { action, index } => {
                if !self.map.set_binding(action, index, binding.clone()) {
                    self.error = true;
                    self.message = Some(format!(
                        "{} no longer has that binding; press a control again.",
                        action_label(action)
                    ));
                    return false;
                }
            }
            CaptureSlot::Add { action } => self.map.add_binding(action, binding.clone()),
        }
        self.map.dedup_binding(action, &binding);
        self.error = false;
        self.message = Some(match binding_label(&binding) {
            Some(label) => format!("{} → {label}", action_label(action)),
            None => format!("{} bound.", action_label(action)),
        });
        true
    }

    /// Remove one binding slot.
    pub fn remove_binding(&mut self, action: Action, index: usize) -> bool {
        let removed = self.map.remove_binding(action, index);
        if let Some(binding) = &removed {
            self.error = false;
            self.message = Some(match binding_label(binding) {
                Some(label) => format!("Removed {label} from {}.", action_label(action)),
                None => format!("Removed a binding from {}.", action_label(action)),
            });
        }
        removed.is_some()
    }

    /// Restore one action's built-in bindings.
    pub fn reset_action(&mut self, action: Action) {
        let defaults = self.default.binding(action).to_vec();
        self.map.set_bindings(action, defaults);
        self.capture = None;
        self.error = false;
        self.message = Some(format!("Reset {}.", action_label(action)));
    }

    /// Restore the whole device to its built-in layout.
    pub fn reset_all(&mut self) {
        self.map = self.default.clone();
        self.capture = None;
        self.error = false;
        self.message = Some("Reset every binding to the defaults.".to_string());
    }

    /// Set the stick deadzone, clamped to `0.0..=1.0`.
    pub fn set_deadzone(&mut self, value: f32) {
        self.map.deadzone = value.clamp(0.0, 1.0);
    }

    /// Set the hold time before a direction repeats.
    pub fn set_repeat_delay_ms(&mut self, value: u32) {
        self.map.repeat_delay_ms = value;
    }

    /// Set the repeat period, never zero.
    pub fn set_repeat_interval_ms(&mut self, value: u32) {
        self.map.repeat_interval_ms = value.max(1);
    }

    /// Fold the edit into a map set.
    ///
    /// A device whose map matches its built-in layout loses its override, so
    /// resetting everything empties the file rather than storing a copy of the
    /// defaults.
    pub fn apply(&self, maps: &mut InputMaps) {
        if self.is_customized() {
            maps.insert(self.map.clone());
        } else {
            maps.remove(self.target.device());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::{Button, Key};
    use crate::map::InputMaps;

    fn keyboard_session(maps: &InputMaps) -> RemapSession {
        RemapSession::new(RemapTarget::keyboard(), maps)
    }

    fn pad_session(guid: &str, maps: &InputMaps) -> RemapSession {
        RemapSession::new(RemapTarget::pad(guid, "Test Pad"), maps)
    }

    #[test]
    fn a_new_session_loads_the_stored_override_or_the_default() {
        let maps = InputMaps::new();
        let session = pad_session("guid-1", &maps);
        assert_eq!(session.target().device(), "guid-1");
        assert_eq!(session.target().label(), "Test Pad");
        assert!(!session.is_customized());
        assert_eq!(
            session.bindings(Action::Confirm),
            &[Binding::button(Button::South)]
        );

        let mut stored = InputMaps::new();
        let mut map = InputMap::gamepad("guid-1");
        map.bind(Action::Confirm, Binding::button(Button::North));
        stored.insert(map);
        let session = pad_session("guid-1", &stored);
        assert!(session.is_customized());
        assert_eq!(
            session.bindings(Action::Confirm),
            &[Binding::button(Button::North)]
        );
    }

    #[test]
    fn a_capture_only_applies_while_it_is_pending() {
        let maps = InputMaps::new();
        let mut session = pad_session("guid-1", &maps);
        assert!(!session.capture_control(Binding::button(Button::North)));
        assert!(session.message().is_none());

        session.begin_capture(CaptureSlot::Replace {
            action: Action::Confirm,
            index: 0,
        });
        assert!(session.is_capturing());
        assert!(session.capture_control(Binding::button(Button::North)));
        assert!(!session.is_capturing());
        assert_eq!(
            session.bindings(Action::Confirm),
            &[Binding::button(Button::North)]
        );
        assert_eq!(session.message(), Some("Confirm → Y"));
    }

    #[test]
    fn a_capture_refuses_the_wrong_kind_of_control() {
        let maps = InputMaps::new();
        let mut pad = pad_session("guid-1", &maps);
        pad.begin_capture(CaptureSlot::Add {
            action: Action::Confirm,
        });
        assert!(!pad.capture_control(Binding::key(Key::Enter)));
        // Still pending, with an explanation and the map untouched.
        assert!(pad.is_capturing());
        assert!(pad.has_error());
        assert_eq!(pad.bindings(Action::Confirm).len(), 1);

        let mut keyboard = keyboard_session(&maps);
        keyboard.begin_capture(CaptureSlot::Add {
            action: Action::Confirm,
        });
        assert!(!keyboard.capture_control(Binding::button(Button::South)));
        assert!(keyboard.is_capturing());
        assert!(keyboard.has_error());
    }

    #[test]
    fn a_capture_steals_the_control_from_the_action_that_owned_it() {
        let maps = InputMaps::new();
        let mut session = keyboard_session(&maps);
        // Enter was Confirm; bind it to Back and Confirm loses it.
        session.begin_capture(CaptureSlot::Replace {
            action: Action::Back,
            index: 0,
        });
        assert!(session.capture_control(Binding::key(Key::Enter)));
        assert_eq!(session.bindings(Action::Back), &[Binding::key(Key::Enter)]);
        assert!(
            !session
                .bindings(Action::Confirm)
                .contains(&Binding::key(Key::Enter))
        );
    }

    #[test]
    fn adding_a_binding_keeps_the_existing_ones() {
        let maps = InputMaps::new();
        let mut session = pad_session("guid-1", &maps);
        // NavigateUp already has the d-pad and the stick.
        let before = session.bindings(Action::NavigateUp).len();
        session.begin_capture(CaptureSlot::Add {
            action: Action::NavigateUp,
        });
        assert!(session.capture_control(Binding::button(Button::North)));
        assert_eq!(session.bindings(Action::NavigateUp).len(), before + 1);
        assert_eq!(
            session.bindings(Action::NavigateUp).last(),
            Some(&Binding::button(Button::North))
        );
    }

    #[test]
    fn removing_the_last_binding_unbinds_the_action() {
        let maps = InputMaps::new();
        let mut session = pad_session("guid-1", &maps);
        assert!(session.remove_binding(Action::Confirm, 0));
        assert!(!session.map().is_bound(Action::Confirm));
        // A stale index is a no-op, not a panic.
        assert!(!session.remove_binding(Action::Confirm, 0));
    }

    #[test]
    fn resetting_an_action_and_the_whole_map_restores_the_defaults() {
        let maps = InputMaps::new();
        let mut session = pad_session("guid-1", &maps);
        session.begin_capture(CaptureSlot::Replace {
            action: Action::Confirm,
            index: 0,
        });
        assert!(session.capture_control(Binding::button(Button::North)));
        assert!(session.is_customized());

        session.reset_action(Action::Confirm);
        assert_eq!(
            session.bindings(Action::Confirm),
            &[Binding::button(Button::South)]
        );
        // Resetting one action does not hand the control it stole back to the
        // action that used to own it (Search lost `North`), so the device is
        // still customised until a full reset.
        assert!(session.is_customized());

        session.begin_capture(CaptureSlot::Replace {
            action: Action::Back,
            index: 0,
        });
        assert!(session.capture_control(Binding::button(Button::South)));
        assert!(session.is_customized());
        session.reset_all();
        assert!(!session.is_customized());
        assert!(!session.is_capturing());
    }

    #[test]
    fn apply_stores_an_override_and_a_reset_removes_it() {
        let maps = InputMaps::new();
        let mut session = pad_session("guid-1", &maps);
        session.begin_capture(CaptureSlot::Replace {
            action: Action::Confirm,
            index: 0,
        });
        assert!(session.capture_control(Binding::button(Button::North)));

        let mut edited = maps.clone();
        session.apply(&mut edited);
        let stored = edited.get("guid-1").expect("override written");
        assert_eq!(
            stored.binding(Action::Confirm),
            &[Binding::button(Button::North)]
        );

        session.reset_all();
        session.apply(&mut edited);
        assert!(edited.get("guid-1").is_none(), "reset drops the override");
    }

    #[test]
    fn tuning_is_clamped_and_saved_in_the_same_override() {
        let maps = InputMaps::new();
        let mut session = pad_session("guid-1", &maps);
        session.set_deadzone(3.5);
        session.set_repeat_interval_ms(0);
        session.set_repeat_delay_ms(250);
        assert_eq!(session.map().deadzone, 1.0);
        assert_eq!(session.map().repeat_interval_ms, 1);
        assert_eq!(session.map().repeat_delay_ms, 250);
        assert!(session.is_customized());

        let mut edited = maps.clone();
        session.apply(&mut edited);
        assert_eq!(edited.get("guid-1").map(|m| m.deadzone), Some(1.0));
    }

    #[test]
    fn cancelling_a_capture_changes_nothing() {
        let maps = InputMaps::new();
        let mut session = pad_session("guid-1", &maps);
        session.begin_capture(CaptureSlot::Replace {
            action: Action::Confirm,
            index: 0,
        });
        session.cancel_capture();
        assert!(!session.is_capturing());
        assert!(session.message().is_none());
        assert!(!session.is_customized());
    }
}
