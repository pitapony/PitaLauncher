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
use futures_channel::mpsc::UnboundedSender;
use pitagames_core::{Game, GameId};
use pitagames_stores::queue::{InstallJob, JobState};
use time::OffsetDateTime;

use crate::focus;
use crate::handoff;
use crate::handoff::{Handoff, WindowAction};
use crate::library::Library;
use crate::play::{Play, PlayEvent};
use crate::session::Session;
use crate::store_installs;
use crate::store_installs::{InstallEvent, InstallOutcome, Installs};
use crate::{View, activation, launch_start, persist, shell};

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
