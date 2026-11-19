//! Controller and keyboard input for PitaGames.
//!
//! This crate is the whole input path for both shells and, like every crate
//! below `app`, it contains no Dioxus types. It deliberately does **not** use
//! the webview's `navigator.getGamepads()`: WebKitGTK exposes it through
//! `libmanette`, but it only reports while the webview has focus, carries no
//! remap or hotplug metadata, and cannot be exercised headlessly. `gilrs`
//! speaks evdev directly and is the single source for both shells.
//!
//! The layers, outermost first:
//!
//! - [`source`] is the hardware boundary: an [`InputSource`] yields [`RawEvent`]s
//!   (a button, an axis, a key, a connect/disconnect, a battery change) with no
//!   knowledge of actions. [`GilrsSource`] wraps [`gilrs`]; [`ReplaySource`]
//!   feeds a recorded script so tests never touch a device.
//! - [`Binding`] maps one physical control (a button, a key, an axis direction,
//!   a stick direction) onto an [`Action`]. [`InputMap`] holds one device's
//!   bindings, its radial deadzone and its repeat timing.
//! - [`Input`] is the state machine: it folds raw events through the map,
//!   applies the deadzone, turns held directions into timed repeats, and emits
//!   [`ActionEvent`]s. Time is passed in ([`std::time::Instant`]) so the repeat
//!   rules are tested without sleeping.
//!
//! [`Action`] is runtime state, not a persisted domain type: the keyboard and
//! every controller share it, so the focus graph in `crates/input` (a later
//! slice) and the same graph in `app` cannot disagree about what "Confirm"
//! means. Only the [`InputMap`]s are persisted, in
//! `<XDG_CONFIG_HOME>/pitagames/input.toml`.

mod action;
mod binding;
mod gamepad;
mod map;
mod mapper;
mod replay;
mod source;

pub use action::{Action, ActionEvent, ActionKind, UnknownAction};
pub use binding::{Axis, Binding, Button, Direction, Key, Stick};
pub use gamepad::{GilrsSource, SourceError};
pub use map::{
    DEFAULT_DEADZONE, DEFAULT_REPEAT_DELAY_MS, DEFAULT_REPEAT_INTERVAL_MS, InputMap, InputMaps,
    InputMapsError, KEYBOARD_DEVICE,
};
pub use mapper::{Input, LastInput};
pub use replay::ReplaySource;
pub use source::{Battery, Device, DeviceId, InputSource, RawEvent};
