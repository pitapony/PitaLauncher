//! The state machine that turns raw events into actions.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::action::{Action, ActionEvent, ActionKind};
use crate::binding::{Axis, Binding, Button, Direction, Key, Stick};
use crate::map::{DEFAULT_REPEAT_DELAY_MS, DEFAULT_REPEAT_INTERVAL_MS, InputMap, InputMaps};
use crate::source::{Battery, Device, DeviceId, RawEvent};

/// Where a binding was read from: the keyboard, or one gamepad session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Origin {
    Keyboard,
    Pad(DeviceId),
}

/// One physical control currently satisfying (or not) a binding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Source {
    origin: Origin,
    binding: Binding,
}

/// An action that is currently held.
#[derive(Debug, Clone)]
struct Hold {
    /// Every source holding it, so a second binding (or a second device) does
    /// not release it while the first is still down.
    sources: BTreeSet<Source>,
    /// When the next repeat is due, if the action repeats.
    next_repeat: Option<Instant>,
    /// The repeat period copied from the map at press time, so `tick` does not
    /// have to look the map up again.
    interval: Duration,
}

/// The live state of one connected gamepad.
#[derive(Debug)]
struct PadState {
    device: Device,
    map: InputMap,
    pressed: BTreeSet<Button>,
    axes: BTreeMap<Axis, f32>,
}

/// The most recent device to produce input, for glyph hints and cursor hiding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastInput {
    /// No input has arrived yet.
    None,
    Keyboard,
    Gamepad(DeviceId),
}

/// The input state machine.
///
/// Feed it [`RawEvent`]s with an explicit `now`, call [`Input::tick`] on your
/// own timer, and act on the [`ActionEvent`]s it returns. Time is injected so
/// the repeat rules are tested without sleeping, and so the two shells can share
/// one clock.
#[derive(Debug)]
pub struct Input {
    maps: InputMaps,
    keyboard: InputMap,
    pads: BTreeMap<DeviceId, PadState>,
    keys: BTreeSet<Key>,
    active: BTreeMap<Action, Hold>,
    last: LastInput,
    battery: BTreeMap<DeviceId, Battery>,
}

impl Input {
    /// An input state with no devices, using `maps` for the defaults.
    pub fn new(maps: InputMaps) -> Self {
        let keyboard = maps.keyboard();
        Self {
            maps,
            keyboard,
            pads: BTreeMap::new(),
            keys: BTreeSet::new(),
            active: BTreeMap::new(),
            last: LastInput::None,
            battery: BTreeMap::new(),
        }
    }

    /// An input state seeded with already-connected devices.
    pub fn with_devices(maps: InputMaps, devices: impl IntoIterator<Item = Device>) -> Self {
        let mut input = Self::new(maps);
        for device in devices {
            input.connect(device);
        }
        input
    }

    /// The stored maps, for the remap UI.
    pub fn maps(&self) -> &InputMaps {
        &self.maps
    }

    /// Replace the maps. Any held action is dropped (a remap mid-hold should not
    /// leave a phantom hold) and devices are re-resolved by GUID.
    pub fn set_maps(&mut self, maps: InputMaps) {
        self.maps = maps;
        self.keyboard = self.maps.keyboard();
        let pads: Vec<(DeviceId, Device)> = self
            .pads
            .iter()
            .map(|(id, pad)| (*id, pad.device.clone()))
            .collect();
        for (id, device) in pads {
            let map = self.maps.for_device(&device.guid, device.is_gamepad);
            if let Some(pad) = self.pads.get_mut(&id) {
                pad.map = map;
            }
        }
        self.active.clear();
    }

    /// Register a connected device. Safe to call twice: a reconnected pad takes
    /// its map from the GUID, not from the session id.
    pub fn connect(&mut self, device: Device) {
        let map = self.maps.for_device(&device.guid, device.is_gamepad);
        self.pads.insert(
            device.id,
            PadState {
                device,
                map,
                pressed: BTreeSet::new(),
                axes: BTreeMap::new(),
            },
        );
    }

    /// Forget a device and release anything it was holding.
    pub fn disconnect(&mut self, id: DeviceId) -> Vec<ActionEvent> {
        self.pads.remove(&id);
        self.battery.remove(&id);
        if self.last == LastInput::Gamepad(id) {
            self.last = LastInput::None;
        }
        self.drop_origin(Origin::Pad(id))
    }

    /// Fold one raw event in and return the actions it changed.
    pub fn handle(&mut self, event: &RawEvent, now: Instant) -> Vec<ActionEvent> {
        match event {
            RawEvent::Connected(device) => {
                self.connect(device.clone());
                Vec::new()
            }
            RawEvent::Disconnected(id) => self.disconnect(*id),
            RawEvent::Button {
                device,
                button,
                pressed,
            } => {
                let Some(pad) = self.pads.get_mut(device) else {
                    return Vec::new();
                };
                if *pressed {
                    pad.pressed.insert(*button);
                } else {
                    pad.pressed.remove(button);
                }
                self.last = LastInput::Gamepad(*device);
                self.reevaluate(Origin::Pad(*device), now)
            }
            RawEvent::Axis {
                device,
                axis,
                value,
            } => {
                let Some(pad) = self.pads.get_mut(device) else {
                    return Vec::new();
                };
                pad.axes.insert(*axis, *value);
                self.last = LastInput::Gamepad(*device);
                self.reevaluate(Origin::Pad(*device), now)
            }
            RawEvent::Key { key, pressed } => {
                if *pressed {
                    self.keys.insert(*key);
                } else {
                    self.keys.remove(key);
                }
                self.last = LastInput::Keyboard;
                self.reevaluate(Origin::Keyboard, now)
            }
            RawEvent::Battery { device, state } => {
                self.battery.insert(*device, *state);
                Vec::new()
            }
        }
    }

    /// Emit a repeat for every held direction whose timer has come due.
    ///
    /// Call it on the shell's tick (say every 16 ms); it is cheap and does
    /// nothing when nothing repeats.
    pub fn tick(&mut self, now: Instant) -> Vec<ActionEvent> {
        let mut due = Vec::new();
        for (action, hold) in &self.active {
            if let Some(next) = hold.next_repeat
                && next <= now
            {
                due.push((*action, hold.interval));
            }
        }
        let mut events = Vec::with_capacity(due.len());
        for (action, interval) in due {
            events.push(ActionEvent::repeated(action));
            if let Some(hold) = self.active.get_mut(&action) {
                hold.next_repeat = Some(now + interval);
            }
        }
        events
    }

    /// The most recent input device.
    pub fn last_input(&self) -> LastInput {
        self.last
    }

    /// The connected devices, in id order.
    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.pads.values().map(|pad| &pad.device)
    }

    /// The last battery state reported for a device.
    pub fn battery(&self, id: DeviceId) -> Option<Battery> {
        self.battery.get(&id).copied()
    }

    /// Whether an action is currently held.
    pub fn is_active(&self, action: Action) -> bool {
        self.active.contains_key(&action)
    }

    /// Every held action, in [`Action`] order.
    pub fn active_actions(&self) -> impl Iterator<Item = Action> + '_ {
        self.active.keys().copied()
    }

    fn map_of(&self, origin: Origin) -> Option<&InputMap> {
        match origin {
            Origin::Keyboard => Some(&self.keyboard),
            Origin::Pad(id) => self.pads.get(&id).map(|pad| &pad.map),
        }
    }

    fn timings(&self, origin: Origin) -> (Duration, Duration) {
        let (delay, interval) = match self.map_of(origin) {
            Some(map) => (map.repeat_delay_ms, map.repeat_interval_ms),
            None => (DEFAULT_REPEAT_DELAY_MS, DEFAULT_REPEAT_INTERVAL_MS),
        };
        (
            Duration::from_millis(u64::from(delay)),
            Duration::from_millis(u64::from(interval)),
        )
    }

    /// Which bindings of one origin are satisfied right now.
    fn produced(&self, origin: Origin) -> BTreeMap<Action, BTreeSet<Source>> {
        let mut out: BTreeMap<Action, BTreeSet<Source>> = BTreeMap::new();
        let Some(map) = self.map_of(origin) else {
            return out;
        };
        for (action, bindings) in map.iter() {
            for binding in bindings {
                if self.binding_active(origin, binding, map.deadzone) {
                    out.entry(action).or_default().insert(Source {
                        origin,
                        binding: binding.clone(),
                    });
                }
            }
        }
        out
    }

    /// Whether one binding is currently satisfied by one origin.
    ///
    /// A key binding is only ever read from the keyboard and a controller
    /// binding only from a gamepad, so a key listed in a pad's map cannot be
    /// pressed by the keyboard acting as that pad.
    fn binding_active(&self, origin: Origin, binding: &Binding, deadzone: f32) -> bool {
        match (origin, binding) {
            (Origin::Keyboard, Binding::Key { key }) => self.keys.contains(key),
            (Origin::Pad(id), Binding::Button { button }) => self
                .pads
                .get(&id)
                .is_some_and(|pad| pad.pressed.contains(button)),
            (Origin::Pad(id), Binding::Axis { axis, positive }) => {
                let value = self
                    .pads
                    .get(&id)
                    .and_then(|pad| pad.axes.get(axis))
                    .copied()
                    .unwrap_or(0.0);
                if *positive {
                    value >= deadzone
                } else {
                    value <= -deadzone
                }
            }
            (Origin::Pad(id), Binding::Stick { stick, direction }) => {
                self.pads
                    .get(&id)
                    .and_then(|pad| stick_direction(&pad.axes, *stick, deadzone))
                    == Some(*direction)
            }
            _ => false,
        }
    }

    /// Recompute one origin's contribution and diff it against the active set.
    ///
    /// A single event can both start one action and end another (a stick push
    /// changes the dominant axis), so releases are reported before presses: a
    /// caller that walks a focus graph sees the old direction let go before the
    /// new one is taken.
    fn reevaluate(&mut self, origin: Origin, now: Instant) -> Vec<ActionEvent> {
        let produced = self.produced(origin);
        let (delay, interval) = self.timings(origin);

        let mut touched: BTreeSet<Action> = produced.keys().copied().collect();
        for (action, hold) in &self.active {
            if hold.sources.iter().any(|source| source.origin == origin) {
                touched.insert(*action);
            }
        }

        let mut released = Vec::new();
        let mut pressed = Vec::new();
        for action in touched {
            let previous = self
                .active
                .remove(&action)
                .map(|hold| (hold.sources, hold.next_repeat));
            let (mut sources, previous_next) = previous.unwrap_or_default();
            let was_active = !sources.is_empty();
            sources.retain(|source| source.origin != origin);
            if let Some(new) = produced.get(&action) {
                sources.extend(new.iter().cloned());
            }

            if sources.is_empty() {
                if was_active {
                    released.push(ActionEvent::new(action, ActionKind::Released));
                }
                continue;
            }

            let next_repeat = if was_active {
                previous_next
            } else if action.repeats() {
                Some(now + delay)
            } else {
                None
            };
            self.active.insert(
                action,
                Hold {
                    sources,
                    next_repeat,
                    interval,
                },
            );
            if !was_active {
                pressed.push(ActionEvent::new(action, ActionKind::Pressed));
            }
        }
        released.extend(pressed);
        released
    }

    /// Release everything one origin was holding.
    fn drop_origin(&mut self, origin: Origin) -> Vec<ActionEvent> {
        let mut emptied = Vec::new();
        for (action, hold) in self.active.iter_mut() {
            hold.sources.retain(|source| source.origin != origin);
            if hold.sources.is_empty() {
                emptied.push(*action);
            }
        }
        let mut events = Vec::with_capacity(emptied.len());
        for action in emptied {
            self.active.remove(&action);
            events.push(ActionEvent::new(action, ActionKind::Released));
        }
        events
    }
}

/// Resolve an analog stick's push to one cardinal direction.
///
/// The radial deadzone is applied to the stick as a whole, and then the dominant
/// axis wins, so a diagonal push moves one cell rather than two. Positive `Y` is
/// down, matching SDL and `gilrs`.
fn stick_direction(axes: &BTreeMap<Axis, f32>, stick: Stick, deadzone: f32) -> Option<Direction> {
    let (x_axis, y_axis) = match stick {
        Stick::Left => (Axis::LeftStickX, Axis::LeftStickY),
        Stick::Right => (Axis::RightStickX, Axis::RightStickY),
    };
    let x = axes.get(&x_axis).copied().unwrap_or(0.0);
    let y = axes.get(&y_axis).copied().unwrap_or(0.0);
    if (x * x + y * y).sqrt() < deadzone {
        return None;
    }
    Some(if x.abs() >= y.abs() {
        if x > 0.0 {
            Direction::Right
        } else {
            Direction::Left
        }
    } else if y > 0.0 {
        Direction::Down
    } else {
        Direction::Up
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::InputMaps;
    use crate::source::DeviceId;

    fn pad(id: u64) -> Device {
        Device::new(DeviceId(id), format!("Pad {id}"), format!("guid-{id}"))
    }

    fn button(id: u64, button: Button, pressed: bool) -> RawEvent {
        RawEvent::button(DeviceId(id), button, pressed)
    }

    fn axis(id: u64, axis: Axis, value: f32) -> RawEvent {
        RawEvent::axis(DeviceId(id), axis, value)
    }

    fn connect(input: &mut Input, id: u64) {
        input.handle(&RawEvent::Connected(pad(id)), Instant::now());
    }

    #[test]
    fn a_button_press_and_release_is_one_action() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);

        let events = input.handle(&button(1, Button::South, true), t0);
        assert_eq!(
            events,
            [ActionEvent::new(Action::Confirm, ActionKind::Pressed)]
        );
        assert!(input.is_active(Action::Confirm));

        let events = input.handle(&button(1, Button::South, false), t0);
        assert_eq!(
            events,
            [ActionEvent::new(Action::Confirm, ActionKind::Released)]
        );
        assert!(!input.is_active(Action::Confirm));
    }

    #[test]
    fn an_unbound_controller_button_does_nothing() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);
        assert!(
            input
                .handle(&button(1, Button::LeftThumb, true), t0)
                .is_empty()
        );
        assert!(input.active_actions().next().is_none());
    }

    #[test]
    fn the_dpad_and_the_stick_both_move_focus() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);

        assert_eq!(
            input.handle(&button(1, Button::DPadUp, true), t0),
            [ActionEvent::new(Action::NavigateUp, ActionKind::Pressed)]
        );
        assert_eq!(
            input.handle(&button(1, Button::DPadUp, false), t0),
            [ActionEvent::new(Action::NavigateUp, ActionKind::Released)]
        );

        // The left stick, pushed up (negative Y).
        assert_eq!(
            input.handle(&axis(1, Axis::LeftStickY, -0.9), t0),
            [ActionEvent::new(Action::NavigateUp, ActionKind::Pressed)]
        );
        assert_eq!(
            input.handle(&axis(1, Axis::LeftStickY, 0.0), t0),
            [ActionEvent::new(Action::NavigateUp, ActionKind::Released)]
        );
    }

    #[test]
    fn the_radial_deadzone_ignores_small_movements() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);

        // Just inside the default 0.35 deadzone.
        assert!(input.handle(&axis(1, Axis::LeftStickX, 0.2), t0).is_empty());
        assert!(input.handle(&axis(1, Axis::LeftStickY, 0.2), t0).is_empty());
        // A diagonal can clear the deadzone even when neither axis does.
        let events = input.handle(&axis(1, Axis::LeftStickX, 0.3), t0);
        assert_eq!(
            events,
            [ActionEvent::new(Action::NavigateRight, ActionKind::Pressed)]
        );
    }

    #[test]
    fn a_diagonal_push_moves_in_one_direction() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);

        // Equal push: the horizontal axis wins the tie.
        let events = input.handle(&axis(1, Axis::LeftStickX, 0.5), t0);
        let events = [events, input.handle(&axis(1, Axis::LeftStickY, 0.5), t0)].concat();
        assert_eq!(
            events,
            [ActionEvent::new(Action::NavigateRight, ActionKind::Pressed)]
        );
        // Dominating the vertical axis switches the direction, still one press.
        let events = input.handle(&axis(1, Axis::LeftStickY, -0.9), t0);
        assert_eq!(
            events,
            [
                ActionEvent::new(Action::NavigateRight, ActionKind::Released),
                ActionEvent::new(Action::NavigateUp, ActionKind::Pressed),
            ]
        );
    }

    #[test]
    fn holding_a_direction_repeats_after_the_delay_then_on_the_interval() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);
        input.handle(&button(1, Button::DPadDown, true), t0);

        // Nothing before the 400 ms delay.
        assert!(input.tick(t0 + Duration::from_millis(399)).is_empty());
        assert_eq!(
            input.tick(t0 + Duration::from_millis(400)),
            [ActionEvent::repeated(Action::NavigateDown)]
        );
        // Then one per 80 ms interval.
        assert!(input.tick(t0 + Duration::from_millis(450)).is_empty());
        assert_eq!(
            input.tick(t0 + Duration::from_millis(480)),
            [ActionEvent::repeated(Action::NavigateDown)]
        );
        // Releasing stops it.
        input.handle(
            &button(1, Button::DPadDown, false),
            t0 + Duration::from_millis(500),
        );
        assert!(input.tick(t0 + Duration::from_millis(2000)).is_empty());
    }

    #[test]
    fn a_non_repeating_action_is_never_repeated() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);
        input.handle(&button(1, Button::South, true), t0);
        assert!(input.tick(t0 + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn the_keyboard_drives_the_same_actions() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());

        assert_eq!(
            input.handle(&RawEvent::key(Key::ArrowDown, true), t0),
            [ActionEvent::new(Action::NavigateDown, ActionKind::Pressed)]
        );
        // A second binding for the same action keeps it held...
        assert!(input.handle(&RawEvent::key(Key::S, true), t0).is_empty());
        assert!(
            input
                .handle(&RawEvent::key(Key::ArrowDown, false), t0)
                .is_empty()
        );
        assert!(input.is_active(Action::NavigateDown));
        // ...until the last one is released.
        assert_eq!(
            input.handle(&RawEvent::key(Key::S, false), t0),
            [ActionEvent::new(Action::NavigateDown, ActionKind::Released)]
        );
    }

    #[test]
    fn an_axis_binding_reads_a_trigger() {
        let t0 = Instant::now();
        let mut maps = InputMaps::new();
        let mut keyboard = InputMap::keyboard();
        keyboard.unbind(Action::Confirm);
        maps.insert(keyboard);
        let mut jump = InputMap::gamepad("guid-jump");
        jump.bind(Action::Confirm, Binding::axis(Axis::LeftZ, true));
        maps.insert(jump);

        let mut input = Input::new(maps);
        let mut device = pad(1);
        device.guid = "guid-jump".into();
        input.handle(&RawEvent::Connected(device), t0);

        assert!(input.handle(&axis(1, Axis::LeftZ, 0.2), t0).is_empty());
        assert_eq!(
            input.handle(&axis(1, Axis::LeftZ, 0.6), t0),
            [ActionEvent::new(Action::Confirm, ActionKind::Pressed)]
        );
        assert_eq!(
            input.handle(&axis(1, Axis::LeftZ, 0.0), t0),
            [ActionEvent::new(Action::Confirm, ActionKind::Released)]
        );
    }

    #[test]
    fn only_the_keyboard_answers_key_bindings() {
        let t0 = Instant::now();
        let mut maps = InputMaps::new();
        // A pad whose map wrongly lists a keyboard key must not read the
        // keyboard as that pad.
        let mut odd = InputMap::gamepad("guid-odd");
        odd.bind(Action::Confirm, Binding::key(Key::Enter));
        maps.insert(odd);
        // And an empty keyboard map, so the key press itself is silent.
        maps.insert(InputMap::new(crate::map::KEYBOARD_DEVICE));

        let mut input = Input::new(maps);
        let mut device = pad(1);
        device.guid = "guid-odd".into();
        input.handle(&RawEvent::Connected(device), t0);

        assert!(input.handle(&button(1, Button::South, true), t0).is_empty());
        assert!(
            input
                .handle(&RawEvent::key(Key::Enter, true), t0)
                .is_empty()
        );
        assert!(input.active_actions().next().is_none());
    }

    #[test]
    fn a_keyboard_key_does_not_disturb_a_pad_hold() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);
        input.handle(&button(1, Button::South, true), t0);
        // The keyboard's Enter is Confirm too: the action stays held once.
        assert!(
            input
                .handle(&RawEvent::key(Key::Enter, true), t0)
                .is_empty()
        );
        assert!(
            input
                .handle(&RawEvent::key(Key::Enter, false), t0)
                .is_empty()
        );
        assert!(input.is_active(Action::Confirm));
    }

    #[test]
    fn disconnecting_a_pad_releases_what_it_held() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);
        input.handle(&button(1, Button::South, true), t0);

        let events = input.disconnect(DeviceId(1));
        assert_eq!(
            events,
            [ActionEvent::new(Action::Confirm, ActionKind::Released)]
        );
        assert!(!input.is_active(Action::Confirm));
        assert!(input.devices().next().is_none());
    }

    #[test]
    fn a_reconnect_keeps_the_remap_by_guid() {
        let t0 = Instant::now();
        let mut maps = InputMaps::new();
        let mut map = InputMap::gamepad("guid-9");
        // Move Confirm off South, and free North from Search so it is unambiguous.
        map.unbind(Action::Search);
        map.bind(Action::Confirm, Binding::button(Button::North));
        maps.insert(map);

        let mut input = Input::new(maps);
        let mut device = pad(1);
        device.guid = "guid-9".into();
        input.handle(&RawEvent::Connected(device), t0);

        // The remapped button works...
        assert_eq!(
            input.handle(&button(1, Button::North, true), t0),
            [ActionEvent::new(Action::Confirm, ActionKind::Pressed)]
        );
        // ...and the default one no longer does.
        input.handle(&button(1, Button::North, false), t0);
        assert!(input.handle(&button(1, Button::South, true), t0).is_empty());

        // A replug with the same GUID and a new session id keeps the remap.
        input.disconnect(DeviceId(1));
        let mut replugged = pad(7);
        replugged.guid = "guid-9".into();
        input.handle(&RawEvent::Connected(replugged), t0);
        assert_eq!(
            input.handle(&button(7, Button::North, true), t0),
            [ActionEvent::new(Action::Confirm, ActionKind::Pressed)]
        );
    }

    #[test]
    fn events_for_an_unknown_pad_are_ignored() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        assert!(
            input
                .handle(&button(42, Button::South, true), t0)
                .is_empty()
        );
        assert!(
            input
                .handle(&axis(42, Axis::LeftStickX, 1.0), t0)
                .is_empty()
        );
        assert!(input.active_actions().next().is_none());
    }

    #[test]
    fn a_battery_change_is_remembered() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);
        let state = Battery::Discharging { percent: 55 };
        assert!(
            input
                .handle(
                    &RawEvent::Battery {
                        device: DeviceId(1),
                        state,
                    },
                    t0
                )
                .is_empty()
        );
        assert_eq!(input.battery(DeviceId(1)), Some(state));
        input.disconnect(DeviceId(1));
        assert_eq!(input.battery(DeviceId(1)), None);
    }

    #[test]
    fn the_last_input_follows_the_device() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 3);
        assert_eq!(input.last_input(), LastInput::None);

        input.handle(&button(3, Button::South, true), t0);
        assert_eq!(input.last_input(), LastInput::Gamepad(DeviceId(3)));

        input.handle(&RawEvent::key(Key::Enter, true), t0);
        assert_eq!(input.last_input(), LastInput::Keyboard);

        input.disconnect(DeviceId(3));
        assert_eq!(input.last_input(), LastInput::Keyboard);
    }

    #[test]
    fn replacing_the_maps_drops_hold_state() {
        let t0 = Instant::now();
        let mut input = Input::new(InputMaps::new());
        connect(&mut input, 1);
        input.handle(&button(1, Button::South, true), t0);
        assert!(input.is_active(Action::Confirm));

        input.set_maps(InputMaps::new());
        assert!(!input.is_active(Action::Confirm));
        // The device is still there and still maps to the defaults.
        assert_eq!(input.devices().count(), 1);
        assert_eq!(
            input.handle(&button(1, Button::South, true), t0),
            [ActionEvent::new(Action::Confirm, ActionKind::Pressed)]
        );
    }
}
