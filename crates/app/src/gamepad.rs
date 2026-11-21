//! The gamepad pump: one `GilrsSource` → `Input` → the shared focus graph.
//!
//! [`crate::focus`] turns one action into the next focused cell; this module is
//! the other half of the console shell's input path — it opens the one `gilrs`
//! device for the app, folds its raw events through the
//! [`pitagames_input::Input`] state machine, and reports the resulting
//! [`ActionEvent`]s on a channel `App` drains and posts into the active screen's
//! action channel. The keyboard pump in `App` maps a key through the *same*
//! action table (`InputMap::keyboard`) and calls the *same* navigation code, so a
//! pad and the keyboard cannot drift: `Confirm` is one action whichever device
//! produced it.
//!
//! Two shapes, deliberately split:
//!
//! - [`Pump`] is the pure loop body — a source and an injected clock in, actions
//!   out (plus the raw controls a remap capture may want). A
//!   [`ReplaySource`](pitagames_input::ReplaySource) exercises every rule (the
//!   radial deadzone, the dominant-axis resolver, the 400 ms/80 ms repeat) with
//!   no device attached and no sleep.
//! - [`spawn`] runs that loop on a thread over a real
//!   [`GilrsSource`](pitagames_input::GilrsSource), polling every
//!   [`POLL_INTERVAL`]. A machine with no pad, or one whose backend cannot be
//!   opened, is a reported [`GamepadEvent::Unavailable`] — never a crash — so the
//!   launcher is still usable with the keyboard; the actionable "add me to the
//!   `input` group" copy is a later slice.
//!
//! The channel is the same shape as [`crate::play::PlayChannel`]: one drain
//! future owns the receiver behind an async mutex, because a `use_hook` value
//! must be `Clone` and an `UnboundedReceiver` is not one.
//!
//! **Scope.** The pump is hosted by `App`, so one `gilrs` source serves the whole
//! window and every screen's graph is fed through the screen's own
//! `ActionChannel`. The worker also names each device by GUID (for the remap UI
//! and the hint row) and accepts a [`MapReload`] so a saved remap reaches the
//! live machine without a restart.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_input::{
    ActionEvent, Battery, Binding, Device, DeviceId, GilrsSource, Input, InputMaps, InputSource,
    RawEvent,
};

/// How often the worker polls the device backend.
///
/// 16 ms ≈ 60 Hz: fast enough that a repeat interval of 80 ms never drifts by a
/// whole frame, cheap enough that a poll on an idle machine is a no-op.
pub const POLL_INTERVAL: Duration = Duration::from_millis(16);

/// One report from the gamepad worker.
#[derive(Debug, Clone, PartialEq)]
pub enum GamepadEvent {
    /// An action was pressed, released or repeated.
    Action(ActionEvent),
    /// A raw button press, before it is mapped to an action.
    ///
    /// The remap UI captures a control the map does not bind yet, so it needs
    /// the press itself and not only the action the map happens to produce. It
    /// is sent *after* the action for the same press, so a screen that swallows
    /// the action still sees the control.
    Control(Binding),
    /// A pad the worker knows about, named by its stable SDL GUID.
    ///
    /// The remap UI keys a controller's map by the GUID, so the app has to learn
    /// it; the same GUID also picks the right hint row for the last pad used.
    Device(Device),
    /// A pad's power state changed, for the console overlay's controller line.
    ///
    /// `gilrs` has no battery event, so the worker polls and reports each change
    /// once; the overlay pairs it with the pad's name.
    Battery { device: DeviceId, state: Battery },
    /// The backend could not be opened; the launcher runs with the keyboard
    /// only. The message is display-ready but currently only logged.
    Unavailable(String),
}

/// The pure pump body: raw events and an injected clock in, actions out.
///
/// It owns the [`Input`] state machine and nothing else, so a test drives it
/// with a scripted source and never touches a device or sleeps.
#[derive(Debug)]
pub struct Pump {
    input: Input,
    /// The button presses the last [`Pump::step`] drained, for the remap UI.
    controls: Vec<Binding>,
}

impl Pump {
    /// A pump over `maps`; devices arrive as their `Connected` events.
    pub fn new(maps: InputMaps) -> Self {
        Self {
            input: Input::new(maps),
            controls: Vec::new(),
        }
    }

    /// Register the devices a source already knows about.
    ///
    /// `gilrs` may not emit a `Connected` event for a pad that was already
    /// plugged in when the backend opened, so the worker seeds itself from
    /// [`InputSource::devices`] once before its first poll.
    pub fn seed(&mut self, source: &dyn InputSource) {
        for device in source.devices() {
            self.input.connect(device);
        }
    }

    /// Replace the maps a running pump uses.
    ///
    /// This is how the remap UI takes effect without a restart: the worker
    /// pushes the freshly saved maps in, and the machine re-resolves every
    /// device by GUID.
    pub fn set_maps(&mut self, maps: InputMaps) {
        self.input.set_maps(maps);
    }

    /// Drain every waiting raw event, then fire the repeats now due.
    ///
    /// One call is one tick: it folds all pressed/axis changes the backend has
    /// queued and then asks the machine to repeat any direction held long
    /// enough. Returns the actions the tick produced, releases included; the raw
    /// button presses it saw are available afterwards through [`Pump::controls`].
    pub fn step(&mut self, source: &mut dyn InputSource, now: Instant) -> Vec<ActionEvent> {
        self.controls.clear();
        let mut actions = Vec::new();
        while let Some(event) = source.poll() {
            if let RawEvent::Button {
                button,
                pressed: true,
                ..
            } = &event
            {
                self.controls.push(Binding::button(*button));
            }
            actions.extend(self.input.handle(&event, now));
        }
        actions.extend(self.input.tick(now));
        actions
    }

    /// The button presses the last [`Pump::step`] saw.
    pub fn controls(&self) -> &[Binding] {
        &self.controls
    }

    /// The last power state the machine recorded for a device, if any.
    ///
    /// The worker reports each change to the app; the overlay pairs it with the
    /// pad's name.
    pub fn battery(&self, id: DeviceId) -> Option<Battery> {
        self.input.battery(id)
    }
}

/// A handle to push a new map set into a running gamepad worker.
///
/// The receiver lives on the worker thread, so a remap saved in the window
/// reaches the live pump without restarting it. A dropped receiver (the worker
/// exited) makes [`MapReload::send`] return `false`, which the caller ignores.
#[derive(Debug, Clone)]
pub struct MapReload {
    sender: Rc<std::sync::mpsc::Sender<InputMaps>>,
}

impl MapReload {
    /// Hand the worker a new map set. Returns whether it was still listening.
    pub fn send(&self, maps: InputMaps) -> bool {
        self.sender.send(maps).is_ok()
    }
}

impl PartialEq for MapReload {
    /// Every handle that shares one worker's sender is the same handle; Dioxus
    /// props need an equality even though the value is only ever passed down.
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.sender, &other.sender)
    }
}

/// The window's end of the gamepad event channel.
#[derive(Clone)]
pub struct GamepadChannel {
    sender: UnboundedSender<GamepadEvent>,
    receiver: Rc<AsyncMutex<Option<UnboundedReceiver<GamepadEvent>>>>,
}

impl GamepadChannel {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver: Rc::new(AsyncMutex::new(Some(receiver))),
        }
    }

    /// A sender for [`spawn`].
    pub fn sender(&self) -> UnboundedSender<GamepadEvent> {
        self.sender.clone()
    }

    /// Wait for the next report; `None` once the channel closes.
    pub async fn next(&self) -> Option<GamepadEvent> {
        let mut slot = self.receiver.lock().await;
        let receiver = slot.as_mut()?;
        receiver.next().await
    }
}

impl Default for GamepadChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Start the gamepad worker thread.
///
/// The thread owns the `gilrs` context for its whole life and exits when the
/// receiver is gone (the window closed), which is what keeps a pad from driving
/// a screen that is no longer listening. The returned [`MapReload`] lets the
/// window push a remapped device in without restarting the thread.
pub fn spawn(maps: InputMaps, sender: UnboundedSender<GamepadEvent>) -> MapReload {
    let (reload_tx, reload_rx) = std::sync::mpsc::channel();
    let reporter = sender.clone();
    let spawned = thread::Builder::new()
        .name("pitagames-gamepad".to_string())
        .spawn(move || run(maps, sender, reload_rx));

    if let Err(error) = spawned {
        let _ = reporter.unbounded_send(GamepadEvent::Unavailable(format!(
            "could not start the gamepad thread: {error}"
        )));
    }

    MapReload {
        sender: Rc::new(reload_tx),
    }
}

/// The worker's whole body: open the backend, then poll and report forever.
fn run(
    maps: InputMaps,
    sender: UnboundedSender<GamepadEvent>,
    reload: std::sync::mpsc::Receiver<InputMaps>,
) {
    let mut source = match GilrsSource::new() {
        Ok(source) => source,
        Err(error) => {
            let _ = sender.unbounded_send(GamepadEvent::Unavailable(error.to_string()));
            return;
        }
    };

    let mut pump = Pump::new(maps);
    pump.seed(&source);

    // The pads whose GUID the window has been told about, so a newly plugged
    // pad is named once. The session id is not stable, so the GUID is the key.
    let mut known: BTreeMap<DeviceId, String> = BTreeMap::new();
    // The power state last reported per device, so a poll that changed nothing
    // does not spam the channel.
    let mut reported_battery: BTreeMap<DeviceId, Battery> = BTreeMap::new();

    loop {
        // A remap the window saved applies to the live machine.
        while let Ok(maps) = reload.try_recv() {
            pump.set_maps(maps);
        }
        for device in source.devices() {
            if known.get(&device.id) != Some(&device.guid) {
                known.insert(device.id, device.guid.clone());
                if sender.unbounded_send(GamepadEvent::Device(device)).is_err() {
                    return;
                }
            }
        }

        let now = Instant::now();
        let actions = pump.step(&mut source, now);
        // Actions first, then the raw controls for the same press: a remap
        // capture swallows the action and binds the control, in that order.
        for action in actions {
            if sender.unbounded_send(GamepadEvent::Action(action)).is_err() {
                return;
            }
        }
        for control in pump.controls() {
            if sender
                .unbounded_send(GamepadEvent::Control(control.clone()))
                .is_err()
            {
                return;
            }
        }
        // A battery change is reported once; `gilrs` has no event for it, so the
        // worker polls the state the machine recorded.
        for device in source.devices() {
            let Some(state) = pump.battery(device.id) else {
                continue;
            };
            if reported_battery.get(&device.id) == Some(&state) {
                continue;
            }
            reported_battery.insert(device.id, state);
            if sender
                .unbounded_send(GamepadEvent::Battery {
                    device: device.id,
                    state,
                })
                .is_err()
            {
                return;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_input::{
        Action, ActionKind, Axis, Button, Device, DeviceId, InputMaps, RawEvent, ReplaySource,
    };

    fn pad(id: u64) -> Device {
        Device::new(DeviceId(id), format!("Pad {id}"), format!("guid-{id}"))
    }

    #[test]
    fn a_connected_pad_seeds_so_a_button_becomes_an_action() {
        let t0 = Instant::now();
        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [RawEvent::button(DeviceId(1), Button::DPadDown, true)],
        );
        let mut pump = Pump::new(InputMaps::new());
        pump.seed(&source);

        // The seed registers the pad, so the button maps without a Connected
        // event. `gilrs` may not emit one for a pad already present at startup.
        assert_eq!(
            pump.step(&mut source, t0),
            [ActionEvent::new(Action::NavigateDown, ActionKind::Pressed)]
        );
    }

    #[test]
    fn a_connected_event_alone_also_registers_a_pad() {
        let t0 = Instant::now();
        let mut source = ReplaySource::new([
            RawEvent::Connected(pad(2)),
            RawEvent::button(DeviceId(2), Button::South, true),
        ]);
        let mut pump = Pump::new(InputMaps::new());

        // Both events drain into one step; the press is reported once.
        assert_eq!(
            pump.step(&mut source, t0),
            [ActionEvent::new(Action::Confirm, ActionKind::Pressed)]
        );
    }

    #[test]
    fn a_held_direction_repeats_after_the_delay_then_on_the_interval() {
        let t0 = Instant::now();
        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [RawEvent::button(DeviceId(1), Button::DPadRight, true)],
        );
        let mut pump = Pump::new(InputMaps::new());
        pump.seed(&source);

        assert_eq!(
            pump.step(&mut source, t0),
            [ActionEvent::new(Action::NavigateRight, ActionKind::Pressed)]
        );
        // Nothing before the 400 ms delay, then one per 80 ms interval.
        assert!(
            pump.step(&mut source, t0 + Duration::from_millis(399))
                .is_empty()
        );
        assert_eq!(
            pump.step(&mut source, t0 + Duration::from_millis(400)),
            [ActionEvent::repeated(Action::NavigateRight)]
        );
        assert!(
            pump.step(&mut source, t0 + Duration::from_millis(450))
                .is_empty()
        );
        assert_eq!(
            pump.step(&mut source, t0 + Duration::from_millis(480)),
            [ActionEvent::repeated(Action::NavigateRight)]
        );
    }

    #[test]
    fn a_diagonal_stick_push_moves_in_one_direction() {
        let t0 = Instant::now();
        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [
                RawEvent::axis(DeviceId(1), Axis::LeftStickX, 0.9),
                RawEvent::axis(DeviceId(1), Axis::LeftStickY, 0.4),
            ],
        );
        let mut pump = Pump::new(InputMaps::new());
        pump.seed(&source);

        // The horizontal axis dominates, so a diagonal is one action, not two.
        assert_eq!(
            pump.step(&mut source, t0),
            [ActionEvent::new(Action::NavigateRight, ActionKind::Pressed)]
        );
    }

    #[test]
    fn the_deadzone_ignores_a_small_push() {
        let t0 = Instant::now();
        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [RawEvent::axis(DeviceId(1), Axis::LeftStickX, 0.2)],
        );
        let mut pump = Pump::new(InputMaps::new());
        pump.seed(&source);
        assert!(pump.step(&mut source, t0).is_empty());
    }

    #[test]
    fn a_battery_change_is_readable_from_the_pump() {
        let t0 = Instant::now();
        let state = Battery::Discharging { percent: 42 };
        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [RawEvent::Battery {
                device: DeviceId(1),
                state,
            }],
        );
        let mut pump = Pump::new(InputMaps::new());
        pump.seed(&source);
        assert_eq!(pump.battery(DeviceId(1)), None);

        pump.step(&mut source, t0);
        // The worker reports this to the overlay, which pairs it with the name.
        assert_eq!(pump.battery(DeviceId(1)), Some(state));
    }

    #[test]
    fn a_press_and_release_in_one_tick_is_reported_in_order() {
        let t0 = Instant::now();
        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [
                RawEvent::button(DeviceId(1), Button::South, true),
                RawEvent::button(DeviceId(1), Button::South, false),
            ],
        );
        let mut pump = Pump::new(InputMaps::new());
        pump.seed(&source);

        assert_eq!(
            pump.step(&mut source, t0),
            [
                ActionEvent::new(Action::Confirm, ActionKind::Pressed),
                ActionEvent::new(Action::Confirm, ActionKind::Released),
            ]
        );
    }

    #[test]
    fn an_unbound_button_produces_nothing() {
        let t0 = Instant::now();
        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [RawEvent::button(DeviceId(1), Button::LeftThumb, true)],
        );
        let mut pump = Pump::new(InputMaps::new());
        pump.seed(&source);
        assert!(pump.step(&mut source, t0).is_empty());
    }

    #[test]
    fn a_button_press_is_reported_as_a_control_beside_its_action() {
        let t0 = Instant::now();
        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [RawEvent::button(DeviceId(1), Button::South, true)],
        );
        let mut pump = Pump::new(InputMaps::new());
        pump.seed(&source);

        assert_eq!(
            pump.step(&mut source, t0),
            [ActionEvent::new(Action::Confirm, ActionKind::Pressed)]
        );
        // The remap UI gets the raw control, not only the mapped action.
        assert_eq!(pump.controls(), &[Binding::button(Button::South)]);

        // A release is not a capture candidate, and a fresh tick clears the
        // controls so a stale press is never bound twice.
        source = ReplaySource::with_devices(
            vec![pad(1)],
            [RawEvent::button(DeviceId(1), Button::North, false)],
        );
        pump.step(&mut source, t0);
        assert!(pump.controls().is_empty());
    }

    #[test]
    fn a_remap_pushed_into_the_pump_takes_effect() {
        let t0 = Instant::now();
        let mut pump = Pump::new(InputMaps::new());
        pump.seed(&ReplaySource::with_devices(vec![pad(1)], []));

        // With the built-in layout `North` is Search, and `Search` does not
        // repeat, so it is one press.
        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [RawEvent::button(DeviceId(1), Button::North, true)],
        );
        assert_eq!(
            pump.step(&mut source, t0),
            [ActionEvent::new(Action::Search, ActionKind::Pressed)]
        );

        // Remap `North` to `Confirm` for this pad's GUID and push it in. The
        // session's own edit steals the control from `Search`, which bound it
        // by default, so the map has one action per control.
        let mut maps = InputMaps::new();
        let mut map = pitagames_input::InputMap::gamepad("guid-1");
        map.bind(Action::Confirm, Binding::button(Button::North));
        map.dedup_binding(Action::Confirm, &Binding::button(Button::North));
        maps.insert(map);
        pump.set_maps(maps);

        let mut source = ReplaySource::with_devices(
            vec![pad(1)],
            [RawEvent::button(DeviceId(1), Button::North, true)],
        );
        assert_eq!(
            pump.step(&mut source, t0),
            [ActionEvent::new(Action::Confirm, ActionKind::Pressed)]
        );
    }

    #[test]
    fn a_map_reload_handle_is_the_same_when_it_shares_a_worker() {
        // Dioxus props need `PartialEq`; two clones of one handle are equal and
        // a handle from another worker is not.
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = MapReload {
            sender: Rc::new(tx),
        };
        assert_eq!(handle, handle.clone());
        drop(rx);
        // With the worker gone a push is reported as undelivered, not a panic.
        assert!(!handle.send(InputMaps::new()));
    }
}
