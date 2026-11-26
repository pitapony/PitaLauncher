//! The launcher's session model: the shell's state behind one handle (ADR-0027).
//!
//! One handle a screen reads instead of a props struct of signals the root
//! owns. The [`Session`] owns the [`Navigation`] (where the window is and what
//! it opened) and the shared concern handles every screen reaches through the
//! one [`crate::context::use_session`] seam: the play slot, the install queue,
//! the Downloads board, the input maps and their live reload handle, the
//! `notice` line and the history revision. A new screen wires the handles from
//! here rather than `App` growing another prop field.
//!
//! It `Deref`s to [`Navigation`] so a screen writes `session.read().view()` and
//! never has to name the inner model. The concern handles are [`Signal`]s, so a
//! write to one re-renders only the screens that read *it*, while the
//! navigation half stays plain, display-free data.

use std::ops::{Deref, DerefMut};

use dioxus::prelude::Signal;
use futures_channel::mpsc::UnboundedSender;
use pitagames_input::{InputMaps, PadAccess};

use crate::downloads::{DownloadEvent, Downloads};
use crate::gamepad::MapReload;
use crate::navigation::{Navigation, View};
use crate::play::{Play, PlayEvent};
use crate::store_installs::{InstallEvent, Installs};

/// The signals `App` builds from its own hooks and hands to the session.
///
/// `App` creates the state (the input maps are seeded from a hook and the
/// reload handle comes from the gamepad worker's hook), so the session takes
/// them here rather than reaching back into the root. `Copy` because every
/// field is a `Signal`, so handing them over does not consume the root's own
/// handles: `App` keeps using the same signals to drive its pumps.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct SessionHandles {
    /// The one running non-Steam game and its last verdict.
    pub play: Signal<Play>,
    /// Where the play worker's reports arrive.
    pub play_events: Signal<Option<UnboundedSender<PlayEvent>>>,
    /// The CLI stores' install queue.
    pub installs: Signal<Installs>,
    /// Where the install workers' reports arrive.
    pub install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    /// The Downloads board (Steam installs in flight).
    pub downloads: Signal<Downloads>,
    /// Where the Downloads trackers' reports arrive.
    pub download_events: Signal<Option<UnboundedSender<DownloadEvent>>>,
    /// The persisted input maps every pump and the hint row read.
    pub input_maps: Signal<InputMaps>,
    /// The live gamepad worker's reload handle, so a saved remap reaches it.
    pub map_reload: Signal<MapReload>,
    /// The read-only pad-permission probe's verdict, for the Controls screen.
    pub pad_access: Signal<PadAccess>,
    /// The transient one-line notice the header shows.
    pub notice: Signal<String>,
    /// Bumped when a launch session is written, so the history views reload.
    pub history_revision: Signal<u64>,
}

/// The shell's state: where the window is, what it has open, and the concern
/// handles every screen reaches through it.
#[derive(Clone, PartialEq)]
pub(crate) struct Session {
    navigation: Navigation,
    play: Signal<Play>,
    play_events: Signal<Option<UnboundedSender<PlayEvent>>>,
    installs: Signal<Installs>,
    install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    downloads: Signal<Downloads>,
    download_events: Signal<Option<UnboundedSender<DownloadEvent>>>,
    input_maps: Signal<InputMaps>,
    map_reload: Signal<MapReload>,
    pad_access: Signal<PadAccess>,
    notice: Signal<String>,
    history_revision: Signal<u64>,
}

impl Session {
    /// A fresh session on `view`, holding the app's concern handles.
    pub(crate) fn new(view: View, handles: SessionHandles) -> Self {
        Self {
            navigation: Navigation::new(view),
            play: handles.play,
            play_events: handles.play_events,
            installs: handles.installs,
            install_events: handles.install_events,
            downloads: handles.downloads,
            download_events: handles.download_events,
            input_maps: handles.input_maps,
            map_reload: handles.map_reload,
            pad_access: handles.pad_access,
            notice: handles.notice,
            history_revision: handles.history_revision,
        }
    }

    /// The one running non-Steam game and its last verdict.
    pub(crate) fn play(&self) -> Signal<Play> {
        self.play
    }

    /// Where the play worker's reports arrive.
    pub(crate) fn play_events(&self) -> Signal<Option<UnboundedSender<PlayEvent>>> {
        self.play_events
    }

    /// The CLI stores' install queue.
    pub(crate) fn installs(&self) -> Signal<Installs> {
        self.installs
    }

    /// Where the install workers' reports arrive.
    pub(crate) fn install_events(&self) -> Signal<Option<UnboundedSender<InstallEvent>>> {
        self.install_events
    }

    /// The Downloads board (Steam installs in flight).
    pub(crate) fn downloads(&self) -> Signal<Downloads> {
        self.downloads
    }

    /// Where the Downloads trackers' reports arrive.
    pub(crate) fn download_events(&self) -> Signal<Option<UnboundedSender<DownloadEvent>>> {
        self.download_events
    }

    /// The persisted input maps every pump and the hint row read.
    pub(crate) fn input_maps(&self) -> Signal<InputMaps> {
        self.input_maps
    }

    /// The live gamepad worker's reload handle.
    pub(crate) fn map_reload(&self) -> Signal<MapReload> {
        self.map_reload
    }

    /// The read-only pad-permission probe's verdict.
    pub(crate) fn pad_access(&self) -> Signal<PadAccess> {
        self.pad_access
    }

    /// The transient one-line notice the header shows.
    pub(crate) fn notice(&self) -> Signal<String> {
        self.notice
    }

    /// Bumped when a launch session is written, so the history views reload.
    pub(crate) fn history_revision(&self) -> Signal<u64> {
        self.history_revision
    }
}

impl Deref for Session {
    type Target = Navigation;

    fn deref(&self) -> &Navigation {
        &self.navigation
    }
}

impl DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Navigation {
        &mut self.navigation
    }
}

#[cfg(test)]
mod tests {
    // The `rsx!` macro emits `dioxus_core`/`dioxus_signals`/`dioxus_elements`
    // paths, so the prelude glob has to be in scope for the harness render.
    use dioxus::prelude::*;
    use pitagames_input::{InputMaps, PadAccess};

    use crate::downloads::Downloads;
    use crate::gamepad::MapReload;
    use crate::navigation::View;
    use crate::play::Play;
    use crate::session::{Session, SessionHandles};
    use crate::store_installs::Installs;

    /// A do-nothing component, so a `VirtualDom` has a root scope to hand out.
    fn harness() -> dioxus::prelude::Element {
        dioxus::prelude::rsx! {
            div {}
        }
    }

    /// Build a `Session` with fresh signals on a headless runtime.
    ///
    /// The concern handles are `Signal`s, so a session can only exist inside a
    /// Dioxus runtime; a `VirtualDom` supplies one without a window or a
    /// renderer, which is how a seam struct that holds signals is tested.
    fn with_session(body: impl FnOnce(&mut Session, &SessionHandles)) {
        let mut vdom = dioxus::prelude::VirtualDom::new(harness);
        let _ = vdom.rebuild_to_vec();
        vdom.in_scope(dioxus::prelude::ScopeId::ROOT, || {
            let handles = SessionHandles {
                play: Signal::new(Play::default()),
                play_events: Signal::new(None),
                installs: Signal::new(Installs::default()),
                install_events: Signal::new(None),
                downloads: Signal::new(Downloads::default()),
                download_events: Signal::new(None),
                input_maps: Signal::new(InputMaps::new()),
                map_reload: Signal::new(MapReload::detached()),
                pad_access: Signal::new(PadAccess::NoDevices),
                notice: Signal::new(String::new()),
                history_revision: Signal::new(0u64),
            };
            let mut session = Session::new(View::Home, handles);
            body(&mut session, &handles);
        });
    }

    #[test]
    fn a_session_delegates_navigation_through_the_one_handle() {
        with_session(|session, _| {
            assert_eq!(session.view(), View::Home);
            session.set_view(View::Logs);
            assert_eq!(session.view(), View::Logs);
            session.go(View::Stores);
            assert_eq!(session.view(), View::Stores);
            assert_eq!(session.back(), View::Library);
        });
    }

    #[test]
    fn every_concern_handle_is_the_apps_one_instance() {
        with_session(|session, handles| {
            // Reaching a handle through the session yields the very signal the
            // root owns, so a screen and a pump never diverge.
            assert_eq!(session.play(), handles.play);
            assert_eq!(session.play_events(), handles.play_events);
            assert_eq!(session.installs(), handles.installs);
            assert_eq!(session.install_events(), handles.install_events);
            assert_eq!(session.downloads(), handles.downloads);
            assert_eq!(session.download_events(), handles.download_events);
            assert_eq!(session.input_maps(), handles.input_maps);
            assert_eq!(session.map_reload(), handles.map_reload);
            assert_eq!(session.pad_access(), handles.pad_access);
            assert_eq!(session.notice(), handles.notice);
            assert_eq!(session.history_revision(), handles.history_revision);
        });
    }

    #[test]
    fn a_concern_handle_written_through_the_session_is_visible_through_the_root() {
        with_session(|session, handles| {
            session.notice().set("hello".to_string());
            assert_eq!(handles.notice.read().as_str(), "hello");
            session.history_revision().set(7);
            assert_eq!(*handles.history_revision.read(), 7);
        });
    }
}
