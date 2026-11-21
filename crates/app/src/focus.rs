//! Focus plumbing for the desktop shell.
//!
//! The graph itself is [`pitagames_input::FocusGraph`], which stays free of
//! Dioxus so the same rules can drive a future native renderer. This module is
//! the thin Dioxus seam:
//!
//! - [`use_focus`] keeps one screen's focused cell in a signal, so a move
//!   re-renders only the screen that owns it;
//! - [`key_from_tao`] translates a window key event into the shared closed key
//!   set, so keyboard navigation and a gamepad go through one action table;
//! - [`ring_style`] is the focus ring. It recolours the card's existing
//!   1px border and adds a `box-shadow`; both are painted outside the layout
//!   box, so a ring can never shift the grid under the pointer. It always names
//!   both properties (an unfocused card included): Dioxus's style setter
//!   re-adds any inline property a new `style` string omits, so an empty
//!   unfocused value would leave every ring behind.
//!
//! The graph is rebuilt from the screen's logical list on every move instead
//! of being stored: virtualization draws only a window of rows, but focus must
//! still be able to walk past the drawn edge and scroll it into view.
//!
//! The app's one input pump (keyboard and gamepad) does not call a screen's
//! navigation code directly. It posts an [`Action`] into the active screen's
//! [`ActionChannel`]; the screen owns its own `ScreenFocus` and drains its
//! channel in a future. That keeps the pump in `App` while each screen keeps
//! its own graph and its own scroll geometry.

use std::rc::Rc;

use dioxus::desktop::tao::keyboard::KeyCode;
use dioxus::prelude::*;
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_input::{Action, FocusGraph, FocusId, Key as InputKey};

/// The focus ring drawn around a focused tile.
///
/// `border-color` recolours the card's existing border and `box-shadow`
/// thickens it; neither changes layout, so focus never shifts the grid. The
/// unfocused value must name the same properties (see [`ring_style`]).
pub const FOCUS_RING: &str = "border-color:#7a9ad6;box-shadow:0 0 0 2px #7a9ad6;";

/// The unfocused counterpart. It is not empty on purpose: Dioxus's style
/// setter preserves any inline property the new string omits, so a ring left
/// behind by an empty string would never be cleared.
pub const RING_OFF: &str = "border-color:#2c2c38;box-shadow:none;";

/// Inline style fragment for a focusable card.
pub fn ring_style(focused: bool) -> &'static str {
    if focused { FOCUS_RING } else { RING_OFF }
}

/// One screen's focus, held in a signal.
#[derive(Clone, Copy, PartialEq)]
pub struct ScreenFocus {
    cell: Signal<Option<usize>>,
}

/// Create the focus signal for a screen.
///
/// It starts unfocused: the ring appears on the first navigation, not when a
/// pointer merely happens to be over the window.
pub fn use_focus() -> ScreenFocus {
    ScreenFocus {
        cell: use_signal(|| None),
    }
}

impl ScreenFocus {
    /// The focused cell index, if any.
    ///
    /// A grid screen's ids *are* its list indices, so this is also the focused
    /// card's position in the filtered list.
    pub fn index(self) -> Option<usize> {
        *self.cell.read()
    }

    /// The focused cell id, for a screen whose graph is not the plain grid.
    pub fn id(self) -> Option<FocusId> {
        self.index().map(FocusId::from)
    }

    /// Focus a cell directly (a card click continues from that card).
    pub fn set(mut self, index: Option<usize>) {
        self.cell.set(index);
    }

    /// Resolve one navigation action over `count` cells in `columns` columns.
    ///
    /// The first navigation on an unfocused screen enters at the first cell;
    /// a focused index that the list no longer contains falls back the same
    /// way. Returns the index to keep in view, or the unchanged focus for a
    /// non-navigation action or an empty screen.
    pub fn move_focus(mut self, action: Action, count: usize, columns: usize) -> Option<usize> {
        if !action.is_navigation() || count == 0 {
            return self.index();
        }
        let mut graph = FocusGraph::grid(count, columns);
        graph.focus_index(self.index());
        graph.move_focus(action);
        let next = graph.focused_index();
        if next != self.index() {
            self.cell.set(next);
        }
        next
    }

    /// Resolve one navigation action over a caller-built graph.
    ///
    /// [`ScreenFocus::move_focus`] covers the rectangular library grid; the
    /// console home has *ragged* rows (a full-bleed Continue hero above
    /// variable-length shelves), so it builds its own [`FocusGraph`] and hands
    /// it here. The current cell is re-applied by id before the move, so focus
    /// survives a shelf whose length changed under it. Returns the new id, or
    /// the unchanged focus for a non-navigation action or an empty graph.
    pub fn move_in(mut self, graph: &FocusGraph, action: Action) -> Option<FocusId> {
        if !action.is_navigation() {
            return self.id();
        }
        let mut graph = graph.clone();
        if let Some(id) = self.id() {
            graph.focus(id);
        }
        graph.move_focus(action);
        let next = graph.focused_node().map(|node| node.id);
        if next != self.id() {
            self.cell.set(next.map(|id| id.get() as usize));
        }
        next
    }
}

/// The app's one input pump hands actions to the active screen through this.
///
/// The pump lives in `App` and knows only which view is showing; it posts the
/// [`Action`] into that view's channel. The screen owns its own
/// [`ScreenFocus`], drains the channel in a future, and applies the action with
/// its own geometry. That keeps one keyboard/gamepad pump for the whole window
/// without moving every screen's scroll arithmetic into `App`.
///
/// The receiver sits behind an async mutex for the same reason
/// [`crate::gamepad::GamepadChannel`] does: a `use_hook` value must be `Clone`
/// and an `UnboundedReceiver` is not one. One consumer owns a screen's channel
/// at a time; a second concurrent `next()` waits.
#[derive(Clone)]
pub struct ActionChannel {
    sender: UnboundedSender<Action>,
    receiver: Rc<AsyncMutex<Option<UnboundedReceiver<Action>>>>,
}

impl PartialEq for ActionChannel {
    /// Two channels are the same when they share the one receiver. The sender
    /// is a clone of the same endpoint, so pointer identity is enough.
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.receiver, &other.receiver)
    }
}

impl ActionChannel {
    /// Create a channel; the app keeps one per screen with a focus graph.
    pub fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver: Rc::new(AsyncMutex::new(Some(receiver))),
        }
    }

    /// Post an action to the screen. Returns whether the screen is still
    /// listening (a closed channel means the screen is gone).
    pub fn send(&self, action: Action) -> bool {
        self.sender.unbounded_send(action).is_ok()
    }

    /// Wait for the next action; `None` once the channel closes.
    pub async fn next(&self) -> Option<Action> {
        let mut slot = self.receiver.lock().await;
        let receiver = slot.as_mut()?;
        receiver.next().await
    }
}

impl Default for ActionChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Translate a tao physical key into the shared key set.
///
/// Only the keys the keyboard layout binds are translated; anything else is
/// `None`, so the webview keeps typing, shortcuts and text entry that the
/// launcher does not own.
pub fn key_from_tao(code: KeyCode) -> Option<InputKey> {
    match code {
        KeyCode::ArrowUp => Some(InputKey::ArrowUp),
        KeyCode::ArrowDown => Some(InputKey::ArrowDown),
        KeyCode::ArrowLeft => Some(InputKey::ArrowLeft),
        KeyCode::ArrowRight => Some(InputKey::ArrowRight),
        KeyCode::Enter | KeyCode::NumpadEnter => Some(InputKey::Enter),
        KeyCode::Escape => Some(InputKey::Escape),
        KeyCode::Tab => Some(InputKey::Tab),
        KeyCode::Space => Some(InputKey::Space),
        KeyCode::KeyW => Some(InputKey::W),
        KeyCode::KeyA => Some(InputKey::A),
        KeyCode::KeyS => Some(InputKey::S),
        KeyCode::KeyD => Some(InputKey::D),
        KeyCode::KeyQ => Some(InputKey::Q),
        KeyCode::KeyE => Some(InputKey::E),
        KeyCode::KeyF => Some(InputKey::F),
        KeyCode::F11 => Some(InputKey::F11),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_from_tao_maps_the_bound_keys_and_refuses_the_rest() {
        assert_eq!(key_from_tao(KeyCode::ArrowUp), Some(InputKey::ArrowUp));
        assert_eq!(key_from_tao(KeyCode::NumpadEnter), Some(InputKey::Enter));
        assert_eq!(key_from_tao(KeyCode::KeyW), Some(InputKey::W));
        // The shell toggle is the one function key the layout binds.
        assert_eq!(key_from_tao(KeyCode::F11), Some(InputKey::F11));
        // A key the keyboard layout does not bind stays with the webview.
        assert_eq!(key_from_tao(KeyCode::KeyZ), None);
    }

    #[tokio::test]
    async fn an_action_channel_delivers_actions_in_order() {
        let channel = ActionChannel::new();
        assert!(channel.send(Action::Confirm));
        assert!(channel.send(Action::Back));
        assert_eq!(channel.next().await, Some(Action::Confirm));
        assert_eq!(channel.next().await, Some(Action::Back));
    }

    #[tokio::test]
    async fn a_clone_shares_the_one_receiver() {
        // The channel is Clone and its receiver is shared: a second clone
        // waits on the same endpoint, so the two drains never each see half
        // the stream. One action, one delivery.
        let channel = ActionChannel::new();
        let sibling = channel.clone();
        assert!(channel.send(Action::NavigateDown));
        assert_eq!(sibling.next().await, Some(Action::NavigateDown));
    }
}
