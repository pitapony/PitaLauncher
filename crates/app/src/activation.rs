//! Bringing the running launcher to the front.
//!
//! [`pitagames_core::activation`] owns the socket protocol; this module owns the
//! two ends the GUI needs:
//!
//! * [`start`] binds the socket before the window exists and forwards each
//!   request from its own thread, because `accept` blocks.
//! * [`Requests`] is the render-side receiver. A `use_future` in [`crate::App`]
//!   awaits it and raises the window on the event-loop thread — window calls
//!   from the socket thread would be off-limits.
//! * [`focus_running`] is what a second launch calls: it asks the instance that
//!   holds the lock to come forward and then exits.
//!
//! The socket is a convenience, never a dependency: if it cannot be bound the
//! launcher starts normally and a second launch just reports that it is already
//! running.

use std::io;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};
use std::thread;

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_core::activation::{Activation, ActivationError, ActivationServer};

/// The receiver side, parked until the first render claims it.
///
/// `start` runs from `main`, before any Dioxus scope exists; the component then
/// takes it out through [`Requests::take`] exactly once.
static REQUESTS: OnceLock<Mutex<Option<UnboundedReceiver<Activation>>>> = OnceLock::new();

/// Bind the activation socket and forward requests into the window.
///
/// Every failure is a warning printed to stderr: a launcher without focus
/// forwarding still works, it just cannot be raised by a second launch.
pub fn start() {
    let Some(path) = pitagames_core::paths::activation_socket() else {
        eprintln!("[pitagames] no data directory; a second launch cannot raise this window");
        return;
    };

    let server = match ActivationServer::bind(&path) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("[pitagames] focus forwarding is off: {error}");
            return;
        }
    };

    let (sender, receiver) = unbounded::<Activation>();
    // `OnceLock::set` only fails if something already bound a socket; that
    // cannot happen because `main` calls this once, but ignore it like any
    // other warning rather than panic.
    let _ = REQUESTS.set(Mutex::new(Some(receiver)));

    let spawned = thread::Builder::new()
        .name("pitagames-activation".to_string())
        .spawn(move || serve(server, sender));

    if let Err(error) = spawned {
        eprintln!("[pitagames] focus forwarding is off: {error}");
    }
}

/// Ask the instance that holds the lock to come to the front.
///
/// Called by the second launch just before it exits. A missing listener (an
/// older build, a crashed instance) is reported, not escalated: there is
/// nothing useful left to do after the lock said "already running".
pub fn focus_running() {
    let Some(path) = pitagames_core::paths::activation_socket() else {
        eprintln!("PitaGames is already running, and there is no socket to raise it.");
        return;
    };

    match pitagames_core::activation::request_focus(&path) {
        Ok(()) => println!("PitaGames is already running; asked it to come to the front."),
        Err(error) => {
            eprintln!("PitaGames is already running, but it could not be raised: {error}");
        }
    }
}

/// Accept requests forever, forwarding each one to the window.
fn serve(server: ActivationServer, sender: UnboundedSender<Activation>) {
    loop {
        match server.accept() {
            Ok(request) => {
                // A closed receiver means the window is gone; stop listening
                // rather than spin on a channel nobody drains.
                if sender.unbounded_send(request).is_err() {
                    break;
                }
            }
            // A signal during `accept` is not a reason to give up the socket.
            Err(ActivationError::Io { source, .. })
                if source.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                eprintln!("[pitagames] activation socket closed: {error}");
                break;
            }
        }
    }
}

/// The render-side end of the activation channel.
///
/// Cloning shares one receiver; only one `use_future` ever polls it. `use_hook`
/// needs a `Clone` value, and an `UnboundedReceiver` is not one — hence the
/// shared slot. The slot is an async mutex so holding it across `next().await`
/// is legal: a socket thread and the render thread never contend for it anyway.
#[derive(Clone)]
pub struct Requests(Rc<AsyncMutex<Option<UnboundedReceiver<Activation>>>>);

impl Requests {
    /// Take the receiver that [`start`] parked, or `None` when no socket exists.
    ///
    /// Returns `Some` only on the first call: the slot is emptied so a second
    /// component cannot poll the same channel.
    pub fn take() -> Option<Self> {
        take_from(&REQUESTS)
    }

    /// Wait for the next request from a second launch.
    ///
    /// `None` means the channel is closed — the listener thread is gone — and
    /// the caller should stop polling rather than spin.
    pub async fn next(&self) -> Option<Activation> {
        let mut slot = self.0.lock().await;
        let receiver = slot.as_mut()?;
        receiver.next().await
    }
}

/// The body of [`Requests::take`], split out so a test can use its own slot.
fn take_from(slot: &OnceLock<Mutex<Option<UnboundedReceiver<Activation>>>>) -> Option<Requests> {
    let slot = slot.get()?;
    let mut guard = slot.lock().ok()?;
    let receiver = guard.take()?;
    Some(Requests(Rc::new(AsyncMutex::new(Some(receiver)))))
}

/// Bring the window to the front.
///
/// Best effort by design: a Wayland compositor may refuse focus (focus stealing
/// prevention) and the window is still raised. The calls are cheap and
/// idempotent, so they run on every request rather than tracking window state.
pub fn raise_window() {
    let window = dioxus::desktop::window();
    if window.is_minimized() {
        window.set_minimized(false);
    }
    window.set_visible(true);
    window.set_focus();
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt as _;

    // The channel plumbing is exercised through `Requests` without a window:
    // the receiver is a plain futures channel, and the socket itself is tested
    // in `pitagames-core`.

    #[test]
    fn a_request_reaches_the_receiver_once() {
        let (sender, receiver) = unbounded::<Activation>();
        let requests = Requests(Rc::new(AsyncMutex::new(Some(receiver))));

        sender.unbounded_send(Activation::Focus).unwrap();
        drop(sender);

        // The queue already holds the request, so one poll resolves it.
        assert!(matches!(
            requests.next().now_or_never(),
            Some(Some(Activation::Focus))
        ));
        // A closed, drained channel ends the `use_future` loop instead of
        // spinning on `None`.
        assert!(matches!(requests.next().now_or_never(), Some(None)));
    }

    #[test]
    fn a_taken_receiver_cannot_be_taken_again() {
        let slot = OnceLock::new();
        assert!(take_from(&slot).is_none(), "nothing parked yet");

        let (_sender, receiver) = unbounded::<Activation>();
        slot.set(Mutex::new(Some(receiver))).unwrap();

        assert!(take_from(&slot).is_some());
        assert!(take_from(&slot).is_none());
    }
}
