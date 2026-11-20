//! The gamepad pump: one `GilrsSource` → `Input` → the shared focus graph.
//!
//! [`crate::focus`] turns one action into the next focused cell; this module is
//! the other half of the console shell's input path — it opens the one `gilrs`
//! device for the app, folds its raw events through the
//! [`pitagames_input::Input`] state machine, and reports the resulting
//! [`ActionEvent`]s on a channel the grid drains. The keyboard pump in
//! `LibraryGrid` maps a key through the *same* action table (`InputMap::keyboard`)
//! and calls the *same* navigation code, so a pad and the keyboard cannot drift:
//! `Confirm` is one action whichever device produced it.
//!
//! Two shapes, deliberately split:
//!
//! - [`Pump`] is the pure loop body — a source and an injected clock in, actions
//!   out. A [`ReplaySource`](pitagames_input::ReplaySource) exercises every rule
//!   (the radial deadzone, the dominant-axis resolver, the 400 ms/80 ms repeat)
//!   with no device attached and no sleep.
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
//! **Scope.** The pump is hosted by the same component as the focus graph
//! (`LibraryGrid`), exactly like the keyboard pump: it starts when the grid
//! mounts and stops when it unmounts, so a pad cannot queue actions for a screen
//! that is not listening. Lifting it to `App` so every screen shares one pump is
//! the slice that adds the other screens' graphs.

use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_input::{ActionEvent, GilrsSource, Input, InputMaps, InputSource};

/// How often the worker polls the device backend.
///
/// 16 ms ≈ 60 Hz: fast enough that a repeat interval of 80 ms never drifts by a
/// whole frame, cheap enough that a poll on an idle machine is a no-op.
pub const POLL_INTERVAL: Duration = Duration::from_millis(16);

/// One report from the gamepad worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GamepadEvent {
    /// An action was pressed, released or repeated.
    Action(ActionEvent),
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
}

impl Pump {
    /// A pump over `maps`; devices arrive as their `Connected` events.
    pub fn new(maps: InputMaps) -> Self {
        Self {
            input: Input::new(maps),
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

    /// Drain every waiting raw event, then fire the repeats now due.
    ///
    /// One call is one tick: it folds all pressed/axis changes the backend has
    /// queued and then asks the machine to repeat any direction held long
    /// enough. Returns the actions the tick produced, releases included.
    pub fn step(&mut self, source: &mut dyn InputSource, now: Instant) -> Vec<ActionEvent> {
        let mut actions = Vec::new();
        while let Some(event) = source.poll() {
            actions.extend(self.input.handle(&event, now));
        }
        actions.extend(self.input.tick(now));
        actions
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
/// receiver is gone (the grid unmounted), which is what keeps a pad from driving
/// a screen that is no longer listening.
pub fn spawn(maps: InputMaps, sender: UnboundedSender<GamepadEvent>) {
    let reporter = sender.clone();
    let spawned = thread::Builder::new()
        .name("pitagames-gamepad".to_string())
        .spawn(move || run(maps, sender));

    if let Err(error) = spawned {
        let _ = reporter.unbounded_send(GamepadEvent::Unavailable(format!(
            "could not start the gamepad thread: {error}"
        )));
    }
}

/// The worker's whole body: open the backend, then poll and report forever.
fn run(maps: InputMaps, sender: UnboundedSender<GamepadEvent>) {
    let mut source = match GilrsSource::new() {
        Ok(source) => source,
        Err(error) => {
            let _ = sender.unbounded_send(GamepadEvent::Unavailable(error.to_string()));
            return;
        }
    };

    let mut pump = Pump::new(maps);
    pump.seed(&source);

    loop {
        let now = Instant::now();
        for action in pump.step(&mut source, now) {
            if sender.unbounded_send(GamepadEvent::Action(action)).is_err() {
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
}
