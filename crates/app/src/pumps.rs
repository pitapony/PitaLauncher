//! The drain futures `App` runs for the lifetime of the window (ADR-0027).
//!
//! Each pump is one loop over a channel: the install queue's reports, the play
//! slot's reports, the Downloads screen's actions and the activation socket.
//! They live here rather than inline in the root so `App` *registers* its pumps
//! instead of implementing them, and so a drain sits beside the helpers only it
//! uses (`begin_handoff`/`end_handoff`, `handle_downloads_action`,
//! `record_store_install`).

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use dioxus::prelude::*;
use futures_channel::mpsc::{UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use pitagames_core::{Game, GameId};
use pitagames_input::{Battery, Device, DeviceId, RemapSession};
use pitagames_metadata::CachePolicy;
use pitagames_stores::queue::{InstallJob, JobState};
use time::OffsetDateTime;

use crate::downloads::{DownloadEvent, Downloads};
use crate::focus;
use crate::gamepad;
use crate::handoff;
use crate::handoff::{Handoff, WindowAction};
use crate::library::Library;
use crate::library::OWNED_METADATA_PER_RUN;
use crate::play::{Play, PlayEvent};
use crate::session::Session;
use crate::store_installs;
use crate::store_installs::{InstallEvent, InstallOutcome, Installs};
use crate::{View, activation, art, launch_start, persist, runtime, shell, steam_account};

/// Come to the front when a second launch asks this instance to.
pub(crate) fn activation_requests(requests: Option<activation::Requests>) {
    use_future(move || {
        let requests = requests.clone();
        async move {
            let Some(requests) = requests else {
                return;
            };
            while let Some(request) = requests.next().await {
                if request == pitagames_core::activation::Activation::Focus {
                    activation::raise_window();
                    eprintln!("[pitagames] raised the window for a second launch");
                }
            }
        }
    });
}

/// Apply the Downloads screen's actions to its inline focus graph.
pub(crate) fn downloads_actions(
    channel: focus::ActionChannel,
    downloads_focus: focus::ScreenFocus,
    installs: Signal<Installs>,
    install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    session: Signal<Session>,
    notice: Signal<String>,
) {
    use_future(move || {
        let channel = channel.clone();
        async move {
            while let Some(action) = channel.next().await {
                if session.read().view() != View::Downloads {
                    continue;
                }
                handle_downloads_action(
                    action,
                    downloads_focus,
                    installs,
                    install_events,
                    session,
                    notice,
                );
            }
        }
    });
}

/// Fold the install queue's worker reports into the board.
pub(crate) fn install_events(
    channel: store_installs::InstallChannel,
    mut installs: Signal<Installs>,
    install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    library_state: Signal<Option<Library>>,
    db_state: Signal<Option<pitagames_db::Db>>,
    mut notice: Signal<String>,
) {
    use_future(move || {
        let channel = channel.clone();
        async move {
            while let Some(event) = channel.next().await {
                let finished = match &event {
                    InstallEvent::Finished { id, outcome } => Some((id.clone(), outcome.clone())),
                    InstallEvent::Progress { .. } => None,
                };

                installs.write().apply(&event);

                if let Some((id, outcome)) = finished {
                    let job = installs.read().job_by_id(&id).cloned();
                    match (outcome, job) {
                        (InstallOutcome::Done, Some(job)) => {
                            record_store_install(job, library_state, db_state, notice)
                        }
                        (InstallOutcome::Failed(message), _) => notice.set(message),
                        // A cancelled run leaves the row `cancelled`; nothing to
                        // record and nothing to complain about.
                        (InstallOutcome::Done | InstallOutcome::Cancelled, _) => {}
                    }
                    launch_start::pump_installs(installs, install_events, notice);
                }
            }
        }
    });
}

/// Fold the play slot's reports in: the header strip, the hand-off, history and
/// playtime.
#[allow(clippy::too_many_arguments)]
pub(crate) fn play_events(
    channel: crate::play::PlayChannel,
    play_shell: shell::ShellState,
    play_handoff: Signal<Handoff>,
    play_deadline: Rc<Cell<Option<Instant>>>,
    mut play: Signal<Play>,
    mut overlay: Signal<Option<crate::overlay::Overlay>>,
    mut library_state: Signal<Option<Library>>,
    db_state: Signal<Option<pitagames_db::Db>>,
    mut history_revision: Signal<u64>,
) {
    use_future(move || {
        let channel = channel.clone();
        let mut shell = play_shell.clone();
        let play_handoff = play_handoff;
        let play_deadline = Rc::clone(&play_deadline);
        async move {
            while let Some(event) = channel.next().await {
                // The session has to be read before `apply` frees the slot: it
                // carries the start time and the launch shape the history row
                // needs, and `Finished` deliberately repeats neither.
                let session = play.read().running().cloned();
                play.write().apply(&event);

                match event {
                    PlayEvent::Started { id, .. } => {
                        // Only the slot holder may move the window; a stale
                        // `Started` (another launch already began) is ignored.
                        if session.as_ref().is_some_and(|running| running.id == id) {
                            // The overlay is the launcher's; a game owns the
                            // screen now, so close it before the window steps
                            // aside.
                            overlay.set(None);
                            begin_handoff(play_handoff, &mut shell, &id, &play_deadline);
                        }
                    }
                    PlayEvent::Finished { id, title, outcome } => {
                        // A report for a game that no longer holds the slot is
                        // stale; another launch has already begun, so the slot
                        // is not ours.
                        let Some(session) = session.filter(|running| running.id == id) else {
                            continue;
                        };

                        // Bring the window back *before* the SQLite writes
                        // below, so a slow database never delays the return trip.
                        let summary = play
                            .read()
                            .last()
                            .map(|last| last.summary.clone())
                            .unwrap_or_default();
                        end_handoff(play_handoff, &title, &summary, &play_deadline);

                        // History covers failures too: a launch that never
                        // started has no outcome, and "no Proton build" is the
                        // row worth keeping.
                        if let Some(db) = db_state.read().clone() {
                            let record = session.record(&outcome, OffsetDateTime::now_utc());
                            history_revision.set(history_revision() + 1);
                            spawn(async move {
                                if let Err(error) =
                                    persist::record_launch_session(&db, &record).await
                                {
                                    eprintln!("[pitagames] {error}");
                                }
                            });
                        }

                        let Ok(outcome) = outcome else {
                            continue;
                        };

                        // Playtime accumulates: a non-Steam game's number is the
                        // sum of its sessions, since no store counts it for us.
                        let now = OffsetDateTime::now_utc();
                        let total = {
                            let mut guard = library_state.write();
                            match guard.as_mut() {
                                Some(library) => {
                                    let previous = library
                                        .games
                                        .iter()
                                        .find(|entry| entry.game.id == id)
                                        .map(|entry| entry.playtime())
                                        .unwrap_or_default();
                                    let total = previous + outcome.duration;
                                    let mut row = Game::new(id.clone(), title);
                                    row.playtime = total;
                                    row.last_played = Some(now);
                                    library.apply_stored(&[row]);
                                    total
                                }
                                None => outcome.duration,
                            }
                        };

                        if let Some(db) = db_state.read().clone() {
                            let id = id.clone();
                            spawn(async move {
                                if let Err(error) = persist::record_playtime(&db, &id, total).await
                                {
                                    eprintln!("[pitagames] {error}");
                                }
                            });
                        }
                    }
                }
            }
        }
    });
}

/// Apply the console hand-off when a supervised game starts.
///
/// In the console shell the window hides while the game runs; in the desktop
/// shell it stays, so a pointer user can keep browsing. The window call is
/// best-effort (a compositor may ignore it) and a failed hide costs nothing.
fn begin_handoff(
    mut handoff: Signal<Handoff>,
    shell: &mut shell::ShellState,
    id: &GameId,
    deadline: &Rc<Cell<Option<Instant>>>,
) {
    let action = handoff.write().started(id, shell.shell().is_console());
    if action == WindowAction::Hide {
        shell::hide_window();
        deadline.set(None);
        eprintln!("[pitagames] stepping aside while {} runs", id.store_id);
    }
}

/// Apply the console hand-off when a supervised game ends.
///
/// The return trip arms the focus deadline; if no focus event arrives in time,
/// the timer in `App` promotes the fallback badge. `title`/`summary` are the
/// verdict the badge carries.
fn end_handoff(
    mut handoff: Signal<Handoff>,
    title: &str,
    summary: &str,
    deadline: &Rc<Cell<Option<Instant>>>,
) {
    let action = handoff.write().ended(title, summary);
    if action == WindowAction::Show {
        shell::show_and_focus();
        deadline.set(Some(Instant::now() + handoff::FOCUS_GRACE));
        eprintln!("[pitagames] {title} ended; asking for focus");
    }
}

/// Apply one action to the Downloads screen's inline focus graph.
///
/// The cells are the actionable store-install rows: the "Clear finished" row
/// first when there is one, then each active job (Cancel), then each finished
/// job (Retry when it failed, Remove otherwise). Steam's own download rows are
/// informational and stay pointer-only for now.
fn handle_downloads_action(
    action: pitagames_input::Action,
    focus: focus::ScreenFocus,
    mut installs: Signal<Installs>,
    install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    mut session: Signal<crate::session::Session>,
    notice: Signal<String>,
) {
    if action == pitagames_input::Action::Back {
        session.write().set_view(View::Library);
        return;
    }

    let (count, clear_cell, active_len, finished_len) = {
        let snapshot = installs.read();
        let active_len = snapshot.active().len();
        let finished_len = snapshot.finished().len();
        let clear_cell = usize::from(finished_len > 0);
        (
            clear_cell + active_len + finished_len,
            clear_cell,
            active_len,
            finished_len,
        )
    };

    if action.is_navigation() {
        focus.move_focus(action, count, 1);
        return;
    }

    if action != pitagames_input::Action::Confirm {
        return;
    }

    let Some(index) = focus.index() else {
        return;
    };
    if clear_cell == 1 && index == 0 {
        installs.write().clear_finished();
        return;
    }

    let row = index.saturating_sub(clear_cell);
    if row < active_len {
        let row_index = {
            let snapshot = installs.read();
            snapshot.active().get(row).map(|(index, _)| *index)
        };
        if let Some(row_index) = row_index {
            installs.write().cancel(row_index);
        }
        return;
    }

    let finished_index = row - active_len;
    if finished_index >= finished_len {
        return;
    }
    let row_index = {
        let snapshot = installs.read();
        snapshot
            .finished()
            .get(finished_index)
            .map(|(index, _)| *index)
    };
    let Some(row_index) = row_index else {
        return;
    };
    let retry = {
        let snapshot = installs.read();
        snapshot
            .finished()
            .get(finished_index)
            .map(|(_, job)| matches!(job.state, JobState::Failed { .. } | JobState::Cancelled))
            .unwrap_or(false)
    };
    if retry {
        if installs.write().retry(row_index) {
            launch_start::pump_installs(installs, install_events, notice);
        }
    } else {
        installs.write().remove(row_index);
    }
}

/// A store install finished: flip the row and remember it.
///
/// The store CLI created the directory, so the row is installed from now on;
/// the write is what keeps it installed after a restart without another sync.
fn record_store_install(
    job: InstallJob,
    mut library_state: Signal<Option<Library>>,
    db_state: Signal<Option<pitagames_db::Db>>,
    mut notice: Signal<String>,
) {
    if let Some(library) = library_state.write().as_mut() {
        library.apply_install(&job.id, &job.target);
    }

    let Some(db) = (*db_state.read()).clone() else {
        return;
    };
    let mut game = Game::new(job.id, job.title);
    game.installed = true;
    game.install_dir = Some(job.target);
    spawn(async move {
        if let Err(error) = persist::record_install(&db, &game).await {
            notice.set(error);
        }
    });
}

/// The gamepad pump's drain: actions, remap captures, devices and battery.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gamepad_actions(
    channel: gamepad::GamepadChannel,
    actions: (
        focus::ActionChannel,
        focus::ActionChannel,
        focus::ActionChannel,
        focus::ActionChannel,
        focus::ActionChannel,
        focus::ActionChannel,
    ),
    pad_shell: shell::ShellState,
    pad_active: Signal<bool>,
    pad_handoff: Signal<Handoff>,
    overlay: Signal<Option<crate::overlay::Overlay>>,
    session: Signal<Session>,
    mut remap: Signal<Option<RemapSession>>,
    mut pads: Signal<Vec<Device>>,
    mut batteries: Signal<std::collections::BTreeMap<DeviceId, Battery>>,
    library_state: Signal<Option<Library>>,
    notice: Signal<String>,
    play: Signal<Play>,
) {
    use_future(move || {
        let channel = channel.clone();
        let actions = actions.clone();
        let mut shell = pad_shell.clone();
        let mut pad_active = pad_active;
        let pad_handoff = pad_handoff;
        async move {
            while let Some(event) = channel.next().await {
                match event {
                    gamepad::GamepadEvent::Action(action) if action.is_press() => {
                        // While the hand-off has the window hidden, the game owns
                        // the pad: a stray press must not move a ring the player
                        // cannot see or toggle a shell behind the game.
                        if pad_handoff.read().hidden() {
                            continue;
                        }
                        // Any pad press makes the pad the last device: the hint
                        // row switches to its glyphs and the cursor may hide.
                        pad_active.set(true);
                        // While a remap capture is pending, the pad's actions are
                        // swallowed so the pressed control can be bound instead
                        // of navigating; `Back` cancels the capture. The capture
                        // flag is read into a local so its guard drops before the
                        // write below.
                        let capturing = session.read().view() == View::Controls && {
                            let guard = remap.read();
                            guard.as_ref().is_some_and(|session| session.is_capturing())
                        };
                        if capturing {
                            if action.action == pitagames_input::Action::Back {
                                let mut guard = remap.write();
                                if let Some(session) = guard.as_mut() {
                                    session.cancel_capture();
                                }
                            }
                            continue;
                        }
                        // The overlay is modal: while it is open it owns every
                        // action, so a direction walks its rows and `Confirm`
                        // chooses them, never the screen underneath. `Back`,
                        // `Menu` and `Guide` close it.
                        if overlay.read().is_some() {
                            crate::apply_overlay(
                                action.action,
                                overlay,
                                &mut shell,
                                session,
                                notice,
                                play,
                            );
                            continue;
                        }
                        // `Menu` (the pad's `Start`, the keyboard's `M`) opens the
                        // console overlay in either shell.
                        if action.action == pitagames_input::Action::Menu {
                            crate::open_overlay(overlay, &shell, library_state, session, play);
                            continue;
                        }
                        // `Guide` is the shell's own action, not the screen's:
                        // it toggles the two shells wherever the player is (or
                        // dismisses the hand-off's "game ended" badge).
                        if action.action == pitagames_input::Action::Guide {
                            crate::guide_action(&mut shell, session, pad_handoff);
                            continue;
                        }
                        crate::dispatch_action(
                            action.action,
                            session,
                            &actions.0,
                            &actions.1,
                            &actions.2,
                            &actions.3,
                            &actions.4,
                            &actions.5,
                        );
                    }
                    gamepad::GamepadEvent::Control(binding) => {
                        // A raw button press is only useful to a pending capture.
                        if session.read().view() == View::Controls {
                            let mut guard = remap.write();
                            if let Some(session) = guard.as_mut()
                                && session.is_capturing()
                            {
                                session.capture_control(binding);
                            }
                        }
                    }
                    gamepad::GamepadEvent::Device(device) => {
                        let mut known = pads.write();
                        if !known.iter().any(|seen| seen.guid == device.guid) {
                            known.push(device);
                        }
                    }
                    gamepad::GamepadEvent::Battery { device, state } => {
                        batteries.write().insert(device, state);
                    }
                    gamepad::GamepadEvent::Action(_) => {}
                    gamepad::GamepadEvent::Unavailable(message) => {
                        eprintln!("[pitagames] {message}");
                    }
                }
            }
        }
    });
}

/// The startup bootstrap: scan, persist, publish the channels, then keep
/// draining download events for the window's lifetime.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bootstrap(
    channel: store_installs::InstallChannel,
    play_channel: crate::play::PlayChannel,
    mut proton_state: Signal<Option<pitagames_platform::proton::DiscoveryReport>>,
    mut db_state: Signal<Option<pitagames_db::Db>>,
    mut account_state: Signal<Option<crate::steam_account::Account>>,
    mut metadata_queue: Signal<Option<crate::MetadataQueue>>,
    mut art_state: Signal<Option<std::sync::Arc<crate::art::ArtContext>>>,
    mut events: Signal<Option<UnboundedSender<DownloadEvent>>>,
    mut install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    mut play_events: Signal<Option<UnboundedSender<PlayEvent>>>,
    mut library_state: Signal<Option<Library>>,
    art_revision: Signal<u64>,
    mut board: Signal<Downloads>,
) {
    use_future(move || {
        let channel = channel.clone();
        let play_channel = play_channel.clone();
        async move {
            let mut library = Library::load();
            let mut metadata_db = None;
            let mut owned_stale = false;
            // Read-only and fast; the detail page needs it as soon as a game opens.
            proton_state.set(Some(runtime::discover_proton()));

            match persist::open_default().await {
                Ok(db) => {
                    let games = library.snapshot_games();
                    match persist::sync(&db, &games).await {
                        Ok(stored) => library.apply_stored(&stored),
                        Err(error) => library.warnings.push(error),
                    }
                    match persist::load_metadata(&db).await {
                        Ok(rows) => library.apply_metadata(&rows),
                        Err(error) => library.metadata_warnings.push(error),
                    }
                    // The owned cache is optional; a read failure is a warning.
                    match persist::load_owned(&db).await {
                        Ok((owned, fetched_at)) => {
                            if let Some(fetched_at) = fetched_at {
                                let total = owned.len();
                                library.apply_owned(&owned, total, fetched_at);
                                owned_stale = steam_account::is_stale(
                                    Some(fetched_at),
                                    OffsetDateTime::now_utc(),
                                );
                            } else {
                                owned_stale = true;
                            }
                        }
                        Err(error) => library.metadata_warnings.push(error),
                    }
                    // The CLI stores' cached rows (a previous sync or a finished
                    // install here). They are never scanned, so a store whose CLI
                    // is missing simply keeps what the cache holds.
                    match persist::load_store_rows(&db).await {
                        Ok(rows) => library.apply_store_rows(&rows),
                        Err(error) => library.metadata_warnings.push(error),
                    }
                    metadata_db = Some(db.clone());
                    db_state.set(Some(db));
                }
                Err(error) => library
                    .warnings
                    .push(format!("Playtime will not be saved: {error}")),
            }

            // The key and SteamID need the Steam root for `userdata`, so read them
            // after the scan; the library renders either way.
            let account = crate::steam_account::Account::load(library.steam_root.as_deref());
            let account_ready = account.is_ready();
            if account.is_configured() {
                library.owned.pending = owned_stale;
            }
            account_state.set(Some(account));

            // Only ask the store for ids the cache cannot answer; this is what the
            // 200-requests-per-five-minutes budget in `pitagames-metadata` gates.
            // Installed games are refreshed in full; owned ones are capped per run.
            let metadata_targets = library.metadata_candidates(
                CachePolicy::DEFAULT,
                OffsetDateTime::now_utc(),
                OWNED_METADATA_PER_RUN,
            );
            library.metadata_pending = !metadata_targets.is_empty();

            // Art is decorative: if the cache, server or HTTP client cannot start,
            // the library still renders with placeholders.
            let art_context = match art::ArtContext::start().await {
                Ok(context) => {
                    art_state.set(Some(context.clone()));
                    Some(context)
                }
                Err(error) => {
                    library.metadata_warnings.push(error);
                    None
                }
            };

            // Publish the channels before the library: once the grid is on screen,
            // every button must find a sender.
            let (sender, mut receiver) = unbounded::<DownloadEvent>();
            events.set(Some(sender.clone()));
            install_events.set(Some(channel.sender()));
            play_events.set(Some(play_channel.sender()));

            // Images for metadata that was already cached need no store request.
            let art_requests = library.art_requests();
            let libraries = library.libraries.clone();
            let in_flight: Vec<(String, String)> = library
                .in_flight()
                .iter()
                .map(|entry| (entry.app_id().to_string(), entry.title().to_string()))
                .collect();
            library_state.set(Some(library));

            if let Some(context) = art_context.clone()
                && !art_requests.is_empty()
            {
                spawn(crate::warm_art(context, art_requests, art_revision));
            }

            // Metadata is fetched after the first paint: a slow or offline store
            // must never hold up the library. One worker owns the client and the
            // budget; the owned refresh and later scans enqueue into it.
            let metadata_db_worker = metadata_db.clone();
            if let Some(db) = metadata_db_worker {
                let (metadata_sender, metadata_receiver) = unbounded::<Vec<u32>>();
                let queue = crate::MetadataQueue {
                    sender: Some(metadata_sender),
                    queued: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                };
                metadata_queue.set(Some(queue.clone()));
                spawn(crate::metadata_worker(
                    db,
                    library_state,
                    metadata_receiver,
                    queue.counter(),
                    art_context.clone(),
                    art_revision,
                ));
                queue.enqueue(metadata_targets);
            }

            // Refresh the account's owned list after the first paint: one request
            // for the whole library, only when a key is configured and the cache
            // has aged out.
            if account_ready
                && owned_stale
                && let Some(account) = account_state.read().clone()
                && let (Some(key), Some(steam_id)) = (account.key().cloned(), account.steam_id)
            {
                let queue = (*metadata_queue.read()).clone();
                spawn(crate::refresh_owned(
                    key,
                    steam_id,
                    library_state,
                    metadata_db.clone(),
                    queue,
                ));
            }

            // Steam may already be downloading: follow those without a click.
            for (app_id, title) in in_flight {
                launch_start::start_download(board, &sender, &libraries, &app_id, &title);
            }

            while let Some(event) = receiver.next().await {
                board.write().apply(&event);
            }
        }
    });
}
