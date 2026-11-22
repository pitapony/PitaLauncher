//! The shared "start something" paths: play, install, and the install pump.
//!
//! These sit above the framework-free models (`play`, `store_installs`,
//! `downloads`) and are Dioxus-bound because they take `Signal`s. They live in
//! their own module rather than in the root so a screen depends on a module
//! instead of upward on `main.rs` (ADR-0027): the grid, the home hero and the
//! detail page all call in here, and none of them has to reach into `App`.

use std::path::PathBuf;
use std::time::Duration;

use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;
use pitagames_core::{Game, GameId, Store};
use pitagames_stores::queue::StartOutcome;

use crate::downloads::{self, DownloadEvent, Downloads};
use crate::play::{self, Play, PlayEvent};
use crate::store_installs::{self, InstallEvent, Installs};
use crate::{View, format, library, persist};

/// Start a game from a card, the home hero or the detail page.
///
/// One function so the grid's Play button, the home hero's Play and a
/// controller's `Confirm` cannot drift: a Steam row asks Steam to launch itself
/// (and records the playtime Steam will not), every other store starts the
/// supervised hand-off. `notice` doubles as the error line the caller shows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_play(
    game: &Game,
    playtime: Duration,
    mut play: Signal<Play>,
    play_events: Signal<Option<UnboundedSender<PlayEvent>>>,
    mut notice: Signal<String>,
    db: Option<pitagames_db::Db>,
) {
    notice.set(String::new());

    if game.id.store != Store::Steam {
        let Some(sender) = play_events.read().clone() else {
            notice.set("The launcher is still starting; try again in a moment.".to_string());
            return;
        };
        if let Err(message) = play.write().begin(game) {
            notice.set(message);
            return;
        }
        play::spawn(game.clone(), sender);
        return;
    }

    if let Err(error) = library::spawn_steam(&game.id.store_id) {
        notice.set(format!(
            "Could not ask Steam to run appid {}: {error}",
            game.id.store_id
        ));
        return;
    }
    let Some(db) = db else {
        return;
    };
    let id = game.id.clone();
    spawn(async move {
        if let Err(message) = persist::record_launch(&db, &id, playtime).await {
            notice.set(message);
        }
    });
}

/// Install (or update) a game from a card or the home hero.
///
/// Steam installs through its own client and the board follows the manifest;
/// the CLI stores run their own client on the serial queue. The two paths also
/// live in the grid's card button, so both go through this one function.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_install(
    id: &GameId,
    title: &str,
    mut installs: Signal<Installs>,
    install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    events: Signal<Option<UnboundedSender<DownloadEvent>>>,
    board: Signal<Downloads>,
    libraries: &[PathBuf],
    mut notice: Signal<String>,
    mut session: Signal<crate::session::Session>,
) {
    notice.set(String::new());

    if id.store != Store::Steam {
        match pitagames_stores::installer::plan(id, title) {
            Ok(plan) => {
                if let Err(error) = installs.write().enqueue(plan.job()) {
                    notice.set(error.to_string());
                    return;
                }
                pump_installs(installs, install_events, notice);
                session.write().set_view(View::Downloads);
            }
            Err(error) => notice.set(error.to_string()),
        }
        return;
    }

    if let Err(error) = library::spawn_install(&id.store_id) {
        notice.set(format!(
            "Could not ask Steam to install appid {}: {error}",
            id.store_id
        ));
        return;
    }
    match events.read().clone() {
        Some(sender) => {
            start_download(board, &sender, libraries, &id.store_id, title);
            session.write().set_view(View::Downloads);
        }
        None => notice.set("Downloads are not ready yet; try again in a moment.".to_string()),
    }
}

/// Track an install: register the row, then start the polling thread.
///
/// Returns `false` when the appid is already being tracked, so a double click
/// does not spawn a second thread.
pub(crate) fn start_download(
    mut board: Signal<Downloads>,
    events: &UnboundedSender<DownloadEvent>,
    libraries: &[PathBuf],
    app_id: &str,
    title: &str,
) -> bool {
    if !board.write().start(app_id, title) {
        return false;
    }

    let sender = events.clone();
    downloads::track(app_id, libraries, move |event| {
        sender.unbounded_send(event).is_ok()
    });

    true
}

/// Start the next queued CLI-store install, if any, on a worker thread.
///
/// The queue is serial: `start_next` refuses while a job is running, so this
/// only ever hands one job to a worker. A job the disk pre-check refused is
/// already `Failed` (and its row says why), so the loop keeps looking for one
/// it can start.
pub(crate) fn pump_installs(
    mut installs: Signal<Installs>,
    install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    mut notice: Signal<String>,
) {
    let Some(sender) = (*install_events.read()).clone() else {
        return;
    };

    loop {
        // The write guard must not outlive the call: `start_next` takes it, and
        // the `Started` arm reads the job back through a second borrow.
        let outcome = installs.write().start_next();
        match outcome {
            StartOutcome::Started { index } => {
                let job = installs.read().job(index).cloned();
                // The token is registered before the thread starts, so a Cancel
                // click during the CLI's startup still reaches it.
                let control = installs.write().control(index);
                if let (Some(job), Some(control)) = (job, control) {
                    store_installs::spawn(job, sender.clone(), control);
                }
                return;
            }
            StartOutcome::Skipped {
                available,
                required,
                ..
            } => notice.set(format!(
                "Not enough free space for one queued install: {} needed, {} available.",
                format::human_bytes(required),
                format::human_bytes(available)
            )),
            StartOutcome::Busy | StartOutcome::Empty => return,
        }
    }
}
