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
//! - [`FocusGraph`] is the screen's logical layout: cells at `(row, col)`, a
//!   spatial resolver that prefers the same row or column, per-axis wrap and a
//!   deterministic tie-break. The shells render the ring and scroll the focused
//!   cell into view; nothing here touches a DOM.
//! - [`RemapSession`] is the one writer of the maps: it edits one device's
//!   [`InputMap`] slot by slot, captures the next control a pump reports, and
//!   folds the result back into [`InputMaps`] for the app to save.
//! - [`OnScreenKeyboard`] is the console shell's text entry: one ragged key
//!   grid walked by the same [`FocusGraph`], with shift and symbol layers, a
//!   character caret and the text it edits. Its screen is a bottom sheet; this
//!   module owns every rule that sheet applies.
//! - [`assess`] is the read-only permission probe: over an [`InputLayout`] (the
//!   real [`DevInput`] or a fake) it reports whether a controller's `/dev/input`
//!   node is usable or denied, and the one-line fix when it is denied. It never
//!   grabs a device, so a game keeps receiving input.
//!
//! [`Action`] is runtime state, not a persisted domain type: the keyboard and
//! every controller share it, so the focus graph and both shells cannot disagree
//! about what "Confirm" means. Only the [`InputMap`]s are persisted, in
//! `<XDG_CONFIG_HOME>/pitagames/input.toml`, and only [`RemapSession`] writes
//! them.

mod action;
mod binding;
mod focus;
mod gamepad;
mod hint;
mod layout;
mod map;
mod mapper;
mod osk;
mod permission;
mod remap;
mod replay;
mod source;

pub use action::{Action, ActionEvent, ActionKind, UnknownAction};
pub use binding::{Axis, Binding, Button, Direction, Key, Stick};
pub use focus::{Band, FocusGraph, FocusId, FocusNode};
pub use gamepad::{GilrsSource, SourceError};
pub use hint::{
    GAME_PAGE_HINTS, GRAPHICS_HINTS, HINT_ACTIONS, Hint, SETTINGS_HINTS, ScreenHint, action_label,
    binding_label, hints, hints_for,
};
pub use layout::{COLUMN_GAP, H_PADDING, TILE_MIN_WIDTH, TILE_MIN_WIDTH_CONSOLE, fit_columns};
pub use map::{
    DEFAULT_DEADZONE, DEFAULT_REPEAT_DELAY_MS, DEFAULT_REPEAT_INTERVAL_MS, InputMap, InputMaps,
    InputMapsError, KEYBOARD_DEVICE,
};
pub use mapper::{Input, LastInput};
pub use osk::{MAX_TEXT, OnScreenKeyboard, OskKey, OskLayer, OskPress};
pub use permission::{DevInput, GroupState, INPUT_GROUP, InputLayout, PadAccess, assess};
pub use remap::{CaptureSlot, RemapSession, RemapTarget};
pub use replay::ReplaySource;
pub use source::{Battery, Device, DeviceId, InputSource, RawEvent};
