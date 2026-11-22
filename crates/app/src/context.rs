//! The Dioxus seam for the session model (ADR-0027).
//!
//! The whole point of the deepening: a screen asks for **one handle** with
//! [`use_session`] instead of receiving a 2–16-field props struct of signals the
//! root owns. `App` calls [`provide_session`] once, in render, and every screen
//! below it reads the same [`Signal<Session>`].

use dioxus::prelude::*;

use crate::navigation::View;
use crate::session::Session;

/// Build the session and put it in context for every descendant screen.
///
/// Called once from `App`; returns the same handle the screens will read, so
/// the root keeps direct access without a second copy of the state.
pub(crate) fn provide_session(view: View) -> Signal<Session> {
    let session = use_signal(move || Session::new(view));
    provide_context(session);
    session
}

/// The session handle, from the provider in `App`.
pub(crate) fn use_session() -> Signal<Session> {
    use_context::<Signal<Session>>()
}
