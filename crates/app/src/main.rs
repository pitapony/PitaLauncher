//! The PitaGames launcher.
//!
//! This crate is the only place allowed to touch Dioxus/RSX: everything below
//! the render layer (`core`, `stores`, `db`, …) stays framework-free so the
//! renderer underneath the grid can be replaced later.
//!
//! M1 scope: scan the Steam library at startup, show install state and playtime
//! from SQLite, hand a game to Steam on Play, and follow installs/updates in the
//! Downloads view. Controller navigation, the console shell and graphics
//! profiles arrive in later milestones.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dioxus::prelude::*;
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::stream::{self, StreamExt};
use pitagames_core::{Game, GameId, Store};
use pitagames_db::StoredMetadata;
use pitagames_metadata::CachePolicy;
use pitagames_stores::queue::{InstallJob, JobState, StartOutcome};
use time::OffsetDateTime;

use downloads::{DownloadEvent, Downloads};
use library::{Library, OWNED_METADATA_PER_RUN};
use play::{Play, PlayEvent};
use steam_account::Account;
use store_installs::{InstallEvent, Installs};

mod account;
mod activation;
mod anticheat;
mod art;
mod auth;
mod copy;
mod desktop_entry;
mod detail;
mod diagnostics;
mod downloads;
mod format;
mod grid;
mod icon;
mod library;
mod log_view;
mod persist;
mod play;
mod runtime;
mod steam_account;
mod store_installs;
mod store_sync;
mod stores_view;
mod tray;
mod wrappers;

fn main() {
    // `--version` and `-V` are for the package: a bug report or a support
    // question can name the build without opening a webview.
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "-V" || arg == "--version")
    {
        println!("pitagames {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // One launcher at a time. Two instances would scan the same libraries and
    // race over Steam's config files; the lock is held for the whole run and
    // released by the kernel when this process exits.
    let _lock = take_instance_lock();

    // The socket a second launch connects to. Started after the lock so only
    // the instance that owns the launcher listens on it.
    activation::start();
    // A checkout has no packaged menu entry; write one that points here.
    desktop_entry::ensure();

    // A desktop window, not fullscreen: the 10-foot console shell is M2.5.
    let window = dioxus::desktop::WindowBuilder::new()
        .with_title("PitaGames")
        .with_inner_size(dioxus::desktop::LogicalSize::new(1280.0, 800.0));

    // Dioxus installs a default Window/Edit/Help menu bar on a decorated
    // window. The launcher has no use for it (navigation lives in the left
    // rail), so pass an empty menu.
    let mut config = dioxus::desktop::Config::new()
        .with_window(window)
        .with_menu(None);
    // The window icon is painted in code (`icon::rgba`); without one the
    // window falls back to the toolkit's default placeholder.
    if let Some(icon) = icon::window_icon(icon::DEFAULT_SIZE) {
        config = config.with_icon(icon);
    }

    dioxus::LaunchBuilder::desktop()
        .with_cfg(config)
        .launch(App);
}

/// Take the single-instance lock, or exit if another launcher holds it.
///
/// A missing data directory (`HOME` unset) is a warning, not a hard stop: the
/// launcher still runs, it just cannot lock. A genuine I/O failure is fatal —
/// continuing unlocked would allow the very race the lock exists to prevent.
fn take_instance_lock() -> Option<pitagames_core::InstanceLock> {
    let Some(path) = pitagames_core::paths::lock_file() else {
        eprintln!(
            "[pitagames] no data directory (HOME is unset); running without the single-instance lock"
        );
        return None;
    };

    match pitagames_core::InstanceLock::acquire(&path) {
        Ok(lock) => Some(lock),
        Err(pitagames_core::LockError::AlreadyRunning { path, pid }) => {
            match pid {
                Some(pid) => eprintln!("PitaGames is already running (pid {pid})."),
                None => eprintln!("PitaGames is already running."),
            }
            eprintln!("Another instance holds {}.", path.display());
            // The window the user meant to reach is already open; ask it to
            // come forward instead of leaving the click looking ignored.
            activation::focus_running();
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("[pitagames] could not take the single-instance lock: {error}");
            std::process::exit(1);
        }
    }
}

/// Which top-level list the window shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Library,
    Downloads,
    Stores,
    Detail,
    Account,
    Logs,
}

/// The handle every metadata enqueuer holds.
///
/// The ids go into one channel owned by a single worker (one HTTP client, one
/// rate budget). `queued` counts the appids still to fetch, so the worker can
/// tell "the batch I just finished was the last one" from "another enqueuer is
/// waiting" without a racy peek at the channel.
#[derive(Clone, Debug, Default)]
pub struct MetadataQueue {
    sender: Option<UnboundedSender<Vec<u32>>>,
    queued: Arc<AtomicUsize>,
}

impl PartialEq for MetadataQueue {
    /// Whether there is a worker to send to; the counter is runtime state.
    fn eq(&self, other: &Self) -> bool {
        self.sender.is_some() == other.sender.is_some()
    }
}

impl MetadataQueue {
    /// Queue a batch of appids. Returns whether anything was sent.
    fn enqueue(&self, ids: Vec<u32>) -> bool {
        let Some(sender) = self.sender.as_ref() else {
            return false;
        };
        if ids.is_empty() {
            return false;
        }
        let count = ids.len();
        if sender.unbounded_send(ids).is_err() {
            return false;
        }
        self.queued.fetch_add(count, Ordering::SeqCst);
        true
    }

    /// The counter shared with the worker.
    fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.queued)
    }
}

// Component; PascalCase is the dioxus convention.
#[allow(non_snake_case)]
fn App() -> Element {
    let mut library_state = use_signal(|| None::<Library>);
    let mut db_state = use_signal(|| None::<pitagames_db::Db>);
    let mut board = use_signal(Downloads::default);
    let mut events = use_signal(|| None::<UnboundedSender<DownloadEvent>>);
    let mut view = use_signal(|| View::Library);
    // The detail page follows the library by id, so cached metadata arriving
    // after it opens shows up without a second copy of the game.
    let mut selected_game = use_signal(|| None::<GameId>);
    let mut query = use_signal(String::new);
    let mut notice = use_signal(String::new);
    // Art is optional: until the cache/server is up, cards show placeholders.
    let mut art_state = use_signal(|| None::<std::sync::Arc<art::ArtContext>>);
    // Bumped whenever a new image lands on disk; re-renders the <img> sources.
    let art_revision = use_signal(|| 0u64);
    // Proton builds on this machine; read once, shown on the detail page.
    let mut proton_state = use_signal(|| None::<pitagames_platform::proton::DiscoveryReport>);
    // The Steam account: key presence, SteamID and what the owned list knows.
    let mut account_state = use_signal(|| None::<Account>);
    // The single metadata fetch channel; its worker holds the only client, so
    // the 200/5-minute budget is global rather than per-task.
    let mut metadata_queue = use_signal(|| None::<MetadataQueue>);
    // The CLI stores' install queue and the worker reports feeding it.
    let mut installs = use_signal(Installs::default);
    let mut install_events = use_signal(|| None::<UnboundedSender<InstallEvent>>);
    // The one game the launcher is running (non-Steam) and its worker reports.
    // Steam is started by its own client, so it never lands here.
    let mut play = use_signal(Play::default);
    let mut play_events = use_signal(|| None::<UnboundedSender<PlayEvent>>);

    // Activation: the socket thread forwards "focus" requests here, and this
    // future raises the window on the event-loop thread (tao window calls are
    // only safe there). With no socket the receiver is `None` and the future
    // returns immediately.
    let requests = use_hook(activation::Requests::take);
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

    // The CLI-store install queue's channel: one future owns the receiver, and
    // the grid holds a sender once the window is up.
    let install_channel = use_hook(store_installs::InstallChannel::new);

    // The play slot's channel, same shape: the grid and the detail page hold a
    // sender, one drain future below owns the receiver.
    let play_channel = use_hook(play::PlayChannel::new);

    // Drive the install queue: fold each worker report in, flip the row a
    // finished install turned into a real directory, and hand the next queued
    // job to a thread. The queue is serial by design, so this is the only place
    // that starts work.
    let install_drain = install_channel.clone();
    use_future(move || {
        let channel = install_drain.clone();
        async move {
            while let Some(event) = channel.next().await {
                let finished = match &event {
                    InstallEvent::Finished { index, result } => Some((*index, result.clone())),
                    InstallEvent::Progress { .. } => None,
                };

                installs.write().apply(&event);

                if let Some((index, result)) = finished {
                    let job = installs.read().job(index).cloned();
                    match (result, job) {
                        (Ok(()), Some(job)) => {
                            record_store_install(job, library_state, db_state, notice)
                        }
                        (Err(message), _) => notice.set(message),
                        (Ok(()), None) => {}
                    }
                    pump_installs(installs, install_events, notice);
                }
            }
        }
    });

    // A finished game writes its session into the row (playtime, last played)
    // and moves the header strip from "playing" to its verdict. The write is
    // SQLite, so it happens off the event loop.
    let play_drain = play_channel.clone();
    use_future(move || {
        let channel = play_drain.clone();
        async move {
            while let Some(event) = channel.next().await {
                play.write().apply(&event);
                let PlayEvent::Finished {
                    id,
                    title,
                    outcome: Ok(outcome),
                } = event
                else {
                    continue;
                };

                // Playtime accumulates: a non-Steam game's number is the sum of
                // its sessions, since no store counts it for us.
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
                        if let Err(error) = persist::record_playtime(&db, &id, total).await {
                            eprintln!("[pitagames] {error}");
                        }
                    });
                }
            }
        }
    });

    // The tray icon is optional: a missing status-notifier host costs the icon,
    // never the window. The menu handler is registered unconditionally because
    // hooks must not be conditional; without an icon it simply never fires.
    // A left click is raised by Dioxus itself (enabled by default).
    let tray = use_hook(|| tray::build_optional().map(std::rc::Rc::new));
    let tray_menu = tray
        .as_ref()
        .map(|tray| (tray.show_id().clone(), tray.quit_id().clone()));
    dioxus::desktop::use_tray_menu_event_handler(move |event| {
        let Some((show, quit)) = tray_menu.as_ref() else {
            return;
        };
        if event.id == *show {
            activation::raise_window();
        } else if event.id == *quit {
            // The default close behaviour ends the event loop once the last
            // window is gone, which is the quit path.
            dioxus::desktop::window().close();
        }
    });

    // One task owns the background work: scan, persist, then drain download
    // events for the lifetime of the window. Scans are file IO only, so running
    // them here (once) is cheaper than a worker pool.
    let install_sender_channel = install_channel.clone();
    let play_sender_channel = play_channel.clone();
    use_future(move || {
        let channel = install_sender_channel.clone();
        let play_channel = play_sender_channel.clone();
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
            let account = Account::load(library.steam_root.as_deref());
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
                spawn(warm_art(context, art_requests, art_revision));
            }

            // Metadata is fetched after the first paint: a slow or offline store
            // must never hold up the library. One worker owns the client and the
            // budget; the owned refresh and later scans enqueue into it.
            let metadata_db_worker = metadata_db.clone();
            if let Some(db) = metadata_db_worker {
                let (metadata_sender, metadata_receiver) = unbounded::<Vec<u32>>();
                let queue = MetadataQueue {
                    sender: Some(metadata_sender),
                    queued: Arc::new(AtomicUsize::new(0)),
                };
                metadata_queue.set(Some(queue.clone()));
                spawn(metadata_worker(
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
                spawn(refresh_owned(
                    key,
                    steam_id,
                    library_state,
                    metadata_db.clone(),
                    queue,
                ));
            }

            // Steam may already be downloading: follow those without a click.
            for (app_id, title) in in_flight {
                start_download(board, &sender, &libraries, &app_id, &title);
            }

            while let Some(event) = receiver.next().await {
                board.write().apply(&event);
            }
        }
    });

    let snapshot: Option<Library> = (*library_state.read()).clone();
    // The detail page's runtime panel needs the Steam root and the Proton list.
    let steam_root = snapshot
        .as_ref()
        .and_then(|library| library.steam_root.clone());
    let proton_report = proton_state.read().clone();
    // Held as a plain Arc so card closures can clone it cheaply.
    let art_context = art_state.read().clone();
    // Reading the revision makes the whole view re-render when an image lands.
    let _ = art_revision();
    // Resolve the detail target from the current library on every render, so a
    // background metadata update reaches an open detail page.
    let selected_id = selected_game.read().clone();
    let detail_entry = selected_id.as_ref().and_then(|id| {
        snapshot
            .as_ref()?
            .games
            .iter()
            .find(|entry| &entry.game.id == id)
            .cloned()
    });
    let hero_url = detail_entry.as_ref().and_then(|entry| {
        let metadata = entry.store_metadata();
        art_context.as_ref()?.url_for(
            &[
                metadata.and_then(|metadata| metadata.header_image.as_deref()),
                metadata.and_then(|metadata| metadata.background.as_deref()),
            ],
            pitagames_art::ArtSize::Hero,
        )
    });
    let downloads_snapshot: Downloads = (*board.read()).clone();
    let active_downloads: Vec<downloads::Download> = downloads_snapshot.active().cloned().collect();
    let finished_downloads: Vec<downloads::Download> =
        downloads_snapshot.finished().cloned().collect();
    // The CLI stores install in-process, so their queue has its own rows on
    // this board: same screen, different backend.
    let install_snapshot: Installs = (*installs.read()).clone();
    let store_active: Vec<(usize, InstallJob)> = install_snapshot
        .active()
        .into_iter()
        .map(|(index, job)| (index, job.clone()))
        .collect();
    let store_finished: Vec<(usize, InstallJob)> = install_snapshot
        .finished()
        .into_iter()
        .map(|(index, job)| (index, job.clone()))
        .collect();
    let active_count = downloads_snapshot.active_count() + install_snapshot.active_count();
    let nothing_tracked = downloads_snapshot.rows().is_empty()
        && store_active.is_empty()
        && store_finished.is_empty();
    let has_finished = !finished_downloads.is_empty();
    let current_view = view();
    let db_handle = (*db_state.read()).clone();
    let owned_summary = snapshot
        .as_ref()
        .map(|library| library.owned.clone())
        .unwrap_or_default();
    // The one running game and the last verdict, for the strip under the nav.
    let play_snapshot: Play = (*play.read()).clone();
    let running_play = play_snapshot.running().cloned();
    let last_play = play_snapshot.last().cloned();

    let subtitle = match snapshot.as_ref() {
        None => "Scanning…".to_string(),
        Some(library) => {
            let steam_games = library
                .games
                .iter()
                .filter(|entry| entry.is_steam())
                .count();
            let mut subtitle = match &library.steam_root {
                Some(root) => format!("{steam_games} Steam games · {}", root.display()),
                None => "No Steam library on this machine".to_string(),
            };
            // The CLI stores' rows come from the cache, not a scan; saying so
            // keeps the count honest when only some of them are synced.
            let other_stores = library.games.len() - steam_games;
            if other_stores > 0 {
                subtitle.push_str(&format!(" · {other_stores} from other stores"));
            }
            if let Some(total) = library.owned.total {
                subtitle.push_str(&format!(" · {total} owned"));
            }
            if library.metadata_pending {
                subtitle.push_str(" · fetching store metadata…");
            }
            subtitle
        }
    };

    // The grid is a child component so scroll-position changes re-render only
    // it, never the whole window (and never clone the library per scroll tick).
    let library_body = rsx! {
        LibraryGrid {
            library: library_state,
            query,
            art_state,
            art_revision,
            board,
            view,
            selected_game,
            notice,
            events,
            db: db_state,
            installs,
            install_events,
            play,
            play_events,
        }
    };

    let downloads_body = rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:10px;padding:0 32px 40px;",

            if nothing_tracked {
                div {
                    style: "padding:40px 0;color:#78788a;",
                    "Nothing downloading. Installs and updates started here or in Steam show up as they happen."
                }
            }

            if !store_active.is_empty() || !store_finished.is_empty() {
                div {
                    style: "display:flex;align-items:center;gap:12px;margin-top:8px;",
                    span {
                        style: "font-size:11px;text-transform:uppercase;letter-spacing:0.06em;color:#78788a;",
                        "Store installs"
                    }
                    if !store_finished.is_empty() {
                        button {
                            onclick: move |_| {
                                installs.write().clear_finished();
                            },
                            style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#c8c8d4;font-size:12px;cursor:pointer;",
                            "Clear finished"
                        }
                    }
                }

                for (index, job) in store_active {
                    {
                        let title = job.title.clone();
                        let store = job.id.store.as_str();
                        let store_id = job.id.store_id.clone();
                        let state = job.state.label();
                        let percent = job.progress.percent;
                        let width = percent.unwrap_or(0.0);
                        let percent_label = percent
                            .map(|percent| format!("{percent:.0}%"))
                            .unwrap_or_default();
                        let stage = job.progress.stage.clone().unwrap_or_default();
                        let target = job.target.display().to_string();
                        let queued = job.state == JobState::Queued;

                        rsx! {
                            div {
                                key: "store-active-{index}",
                                style: "display:flex;flex-direction:column;gap:8px;padding:14px;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;",

                                div {
                                    style: "display:flex;align-items:baseline;gap:12px;",
                                    span {
                                        style: "font-size:14px;font-weight:600;color:#ffffff;",
                                        "{title}"
                                    }
                                    span {
                                        style: "font-size:11px;text-transform:uppercase;letter-spacing:0.06em;color:#7a9ad6;",
                                        "{state}"
                                    }
                                    span {
                                        style: "margin-left:auto;font-size:11px;color:#78788a;",
                                        "{percent_label}"
                                    }
                                }

                                div {
                                    style: "height:6px;border-radius:3px;background:#2c2c38;overflow:hidden;",
                                    div {
                                        style: "height:100%;width:{width}%;background:#5a7ad6;",
                                    }
                                }

                                div {
                                    style: "display:flex;align-items:baseline;gap:10px;font-size:11px;color:#78788a;",
                                    span { "{store} · {store_id}" }
                                    if !stage.is_empty() {
                                        span {
                                            style: "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                            "{stage}"
                                        }
                                    }
                                    if queued {
                                        button {
                                            onclick: move |_| {
                                                installs.write().cancel(index);
                                            },
                                            style: "margin-left:auto;padding:5px 10px;border-radius:7px;border:1px solid #4a2c2c;background:#2a1e1e;color:#e0a0a0;font-size:11px;cursor:pointer;",
                                            "Cancel"
                                        }
                                    }
                                }

                                span {
                                    style: "font-size:10px;color:#5a5a68;overflow-wrap:anywhere;",
                                    "{target}"
                                }
                            }
                        }
                    }
                }

                for (index, job) in store_finished {
                    {
                        let title = job.title.clone();
                        let meta = format!(
                            "{} · {} · {}",
                            job.id.store,
                            job.id.store_id,
                            job.state.label()
                        );
                        let message = match &job.state {
                            JobState::Failed { message } => message.clone(),
                            _ => String::new(),
                        };
                        let retry = matches!(
                            job.state,
                            JobState::Failed { .. } | JobState::Cancelled
                        );

                        rsx! {
                            div {
                                key: "store-done-{index}",
                                style: "display:flex;flex-direction:column;gap:4px;padding:12px 14px;border-radius:12px;background:#1a1a22;border:1px solid #26262f;",
                                div {
                                    style: "display:flex;align-items:center;gap:12px;",
                                    span {
                                        style: "font-size:13px;color:#c8c8d4;",
                                        "{title}"
                                    }
                                    span {
                                        style: "font-size:11px;color:#78788a;",
                                        "{meta}"
                                    }
                                    if retry {
                                        button {
                                            onclick: move |_| {
                                                if installs.write().retry(index) {
                                                    pump_installs(installs, install_events, notice);
                                                }
                                            },
                                            style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:12px;cursor:pointer;",
                                            "Retry"
                                        }
                                    }
                                    button {
                                        onclick: move |_| {
                                            installs.write().remove(index);
                                        },
                                        style: "padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#c8c8d4;font-size:12px;cursor:pointer;margin-left:auto;",
                                        "Remove"
                                    }
                                }
                                if !message.is_empty() {
                                    span {
                                        style: "font-size:11px;color:#e8a0a0;overflow-wrap:anywhere;",
                                        "{message}"
                                    }
                                }
                            }
                        }
                    }
                }
            }

            for row in active_downloads {
                {
                    let app_id = row.app_id.clone();
                    let title = row.title.clone();
                    let state = row.state.label();
                    let width = row.percent().unwrap_or(0);
                    let percent_label = row
                        .percent()
                        .map(|percent| format!("{percent}%"))
                        .unwrap_or_default();
                    let bytes = match (row.downloaded, row.total) {
                        (Some(downloaded), Some(total)) => format!(
                            "{} / {}",
                            format::human_bytes(downloaded),
                            format::human_bytes(total)
                        ),
                        (Some(downloaded), None) => format::human_bytes(downloaded),
                        _ => String::new(),
                    };

                    rsx! {
                        div {
                            key: "active-{app_id}",
                            style: "display:flex;flex-direction:column;gap:8px;padding:14px;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;",

                            div {
                                style: "display:flex;align-items:baseline;gap:12px;",
                                span {
                                    style: "font-size:14px;font-weight:600;color:#ffffff;",
                                    "{title}"
                                }
                                span {
                                    style: "font-size:11px;text-transform:uppercase;letter-spacing:0.06em;color:#7a9ad6;",
                                    "{state}"
                                }
                                span {
                                    style: "margin-left:auto;font-size:11px;color:#78788a;",
                                    "{percent_label}"
                                }
                            }

                            div {
                                style: "height:6px;border-radius:3px;background:#2c2c38;overflow:hidden;",
                                div {
                                    style: "height:100%;width:{width}%;background:#5a7ad6;",
                                }
                            }

                            div {
                                style: "display:flex;justify-content:space-between;font-size:11px;color:#78788a;",
                                span { "steam · appid {app_id}" }
                                span { "{bytes}" }
                            }
                        }
                    }
                }
            }

            if has_finished {
                div {
                    style: "display:flex;align-items:center;gap:12px;margin-top:8px;",
                    span {
                        style: "font-size:11px;text-transform:uppercase;letter-spacing:0.06em;color:#78788a;",
                        "Finished"
                    }
                    button {
                        onclick: move |_| {
                            board.write().dismiss_finished();
                        },
                        style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#c8c8d4;font-size:12px;cursor:pointer;",
                        "Clear"
                    }
                }

                for row in finished_downloads {
                    {
                        let app_id = row.app_id.clone();
                        let title = row.title.clone();
                        let state = row.state.label();
                        let dismiss_id = app_id.clone();

                        rsx! {
                            div {
                                key: "done-{app_id}",
                                style: "display:flex;align-items:center;gap:12px;padding:12px 14px;border-radius:12px;background:#1a1a22;border:1px solid #26262f;",
                                span {
                                    style: "font-size:13px;color:#c8c8d4;",
                                    "{title}"
                                }
                                span {
                                    style: "font-size:11px;color:#78788a;",
                                    "steam · appid {app_id} · {state}"
                                }
                                button {
                                    onclick: move |_| {
                                        board.write().dismiss(&dismiss_id);
                                    },
                                    style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#c8c8d4;font-size:12px;cursor:pointer;",
                                    "Dismiss"
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    rsx! {
        div {
            style: "display:flex;flex-direction:column;height:100vh;overflow:hidden;background:#14141c;color:#e8e8ee;font-family:sans-serif;",

            header {
                style: "display:flex;align-items:baseline;gap:24px;padding:20px 32px 12px;",
                h1 {
                    style: "font-size:22px;font-weight:600;margin:0;color:#ffffff;",
                    "PitaGames"
                }
                span {
                    style: "font-size:12px;color:#78788a;",
                    "{subtitle}"
                }
                input {
                    placeholder: "Search your library",
                    value: "{query}",
                    oninput: move |event| query.set(event.value()),
                    style: "margin-left:auto;padding:8px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#e8e8ee;width:260px;outline:none;",
                }
            }

            div {
                style: "display:flex;gap:8px;padding:0 32px 16px;",
                button {
                    onclick: move |_| {
                        selected_game.set(None);
                        view.set(View::Library);
                    },
                    style: nav_style(current_view == View::Library),
                    "Library"
                }
                button {
                    onclick: move |_| {
                        selected_game.set(None);
                        view.set(View::Downloads);
                    },
                    style: nav_style(current_view == View::Downloads),
                    "Downloads"
                    if active_count > 0 {
                        " ({active_count})"
                    }
                }
                button {
                    onclick: move |_| {
                        selected_game.set(None);
                        view.set(View::Stores);
                    },
                    style: nav_style(current_view == View::Stores),
                    "Stores"
                }
                button {
                    onclick: move |_| {
                        selected_game.set(None);
                        view.set(View::Account);
                    },
                    style: nav_style(current_view == View::Account),
                    "Account"
                }
                button {
                    onclick: move |_| {
                        selected_game.set(None);
                        view.set(View::Logs);
                    },
                    style: nav_style(current_view == View::Logs),
                    "Logs"
                }
            }

            if let Some(running) = running_play.clone() {
                div {
                    style: "display:flex;align-items:center;gap:12px;margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#1e2a24;color:#b8dcc4;font-size:12px;",
                    span {
                        style: "font-weight:600;",
                        "Playing {running.title}"
                    }
                    if let Some(log) = running.log.clone() {
                        span {
                            style: "color:#6f8f7c;font-family:monospace;font-size:11px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                            "{log.display()}"
                        }
                    }
                    button {
                        onclick: move |_| {
                            if !play.write().stop() {
                                notice.set("The game has not started yet; give it a moment.".to_string());
                            }
                        },
                        style: "margin-left:auto;padding:6px 14px;border-radius:8px;border:1px solid #3a5a48;background:#243a2e;color:#d8efe0;font-size:12px;cursor:pointer;",
                        "Stop"
                    }
                }
            } else if let Some(last) = last_play.clone() {
                div {
                    style: "display:flex;align-items:center;gap:12px;margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#1e1e28;color:#a8a8b8;font-size:12px;",
                    span {
                        style: "font-weight:600;color:#e8e8ee;",
                        "{last.title}"
                    }
                    span { "{last.summary}" }
                    button {
                        onclick: move |_| {
                            play.write().clear_last();
                        },
                        style: "margin-left:auto;padding:4px 10px;border-radius:8px;border:1px solid #2c2c38;background:#14141c;color:#78788a;font-size:11px;cursor:pointer;",
                        "Dismiss"
                    }
                }
            }

            if !notice.read().is_empty() {
                div {
                    style: "margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#3a1e1e;color:#e8a0a0;font-size:12px;",
                    "{notice}"
                }
            }

            if current_view == View::Detail {
                div {
                    style: "flex:1;min-height:0;overflow-y:auto;",
                    if let Some(entry) = detail_entry {
                        detail::Detail {
                            key: "{entry.store().as_str()}-{entry.app_id()}",
                            entry,
                            db: detail::DbHandle(db_handle.clone()),
                            view,
                            hero: hero_url,
                            steam_root: steam_root.clone(),
                            proton: proton_report.clone(),
                            play,
                            play_events,
                        }
                    }
                }
            } else if current_view == View::Downloads {
                div {
                    style: "flex:1;min-height:0;overflow-y:auto;",
                    {downloads_body}
                }
            } else if current_view == View::Stores {
                div {
                    style: "flex:1;min-height:0;overflow-y:auto;",
                    stores_view::StoresView {
                        library: library_state,
                        db: db_state,
                    }
                }
            } else if current_view == View::Account {
                div {
                    style: "flex:1;min-height:0;overflow-y:auto;",
                    account::AccountView {
                        account: account_state,
                        owned: owned_summary,
                        steam_root: steam_root.clone(),
                        db: detail::DbHandle(db_handle.clone()),
                        metadata: metadata_queue,
                        library: library_state,
                    }
                }
            } else if current_view == View::Logs {
                div {
                    style: "flex:1;min-height:0;overflow-y:auto;",
                    log_view::LogsView { dir: pitagames_core::paths::logs_dir() }
                }
            } else {
                {library_body}
            }
        }
    }
}

/// Props for [`LibraryGrid`].
///
/// Every field is a `Signal` so the grid can read state without cloning it and
/// write the pieces it owns (selection, view, notices) without callbacks.
#[derive(Props, Clone, PartialEq)]
struct LibraryGridProps {
    pub library: Signal<Option<Library>>,
    pub query: Signal<String>,
    pub art_state: Signal<Option<std::sync::Arc<art::ArtContext>>>,
    pub art_revision: Signal<u64>,
    pub board: Signal<Downloads>,
    pub view: Signal<View>,
    pub selected_game: Signal<Option<GameId>>,
    pub notice: Signal<String>,
    pub events: Signal<Option<UnboundedSender<DownloadEvent>>>,
    pub db: Signal<Option<pitagames_db::Db>>,
    /// The CLI stores' install queue: a card whose game is queued reads its
    /// state here, and its Install button enqueues into it.
    pub installs: Signal<Installs>,
    pub install_events: Signal<Option<UnboundedSender<InstallEvent>>>,
    /// The one running non-Steam game: its card shows `starting`/`playing`
    /// instead of a Play button.
    pub play: Signal<Play>,
    pub play_events: Signal<Option<UnboundedSender<PlayEvent>>>,
}

/// The windowed library grid.
///
/// Scroll offset and viewport size live here rather than in `App`, so a scroll
/// tick re-renders this component alone instead of cloning the whole library
/// (and rebuilding the header) on every event. Only the rows inside the
/// viewport plus [`grid::OVERSCAN_ROWS`] are drawn; a full-height sizer keeps
/// the scrollbar and the scroll offset honest.
#[allow(non_snake_case)]
fn LibraryGrid(props: LibraryGridProps) -> Element {
    let library = props.library;
    let query = props.query;
    let art_state = props.art_state;
    let art_revision = props.art_revision;
    let board = props.board;
    // Signal setters need `&mut`; signals are Copy, so rebind locally and the
    // `move` closures below each get their own mutable copy.
    let mut view = props.view;
    let mut selected_game = props.selected_game;
    let mut notice = props.notice;
    let events = props.events;
    let db = props.db;
    let mut installs = props.installs;
    let install_events = props.install_events;
    let mut play = props.play;
    let play_events = props.play_events;

    let mut scroll_top = use_signal(|| 0.0f64);
    let mut viewport_width = use_signal(|| 0.0f64);
    let mut viewport_height = use_signal(|| 0.0f64);

    // Reading the revision makes a capsule that just finished downloading
    // repaint without re-rendering the whole window.
    let _ = art_revision();

    let library_guard = library.read();
    let Some(state) = library_guard.as_ref() else {
        return rsx! {
            div {
                style: "padding:32px;color:#78788a;",
                "Scanning Steam libraries…"
            }
        };
    };

    let needle = query.read().to_string();
    let matches: Vec<&library::GameEntry> = state.filtered(&needle);
    let geometry = grid::GridGeometry::for_width(viewport_width());
    let window = grid::window(
        matches.len(),
        geometry,
        scroll_top(),
        viewport_height(),
        grid::OVERSCAN_ROWS,
    );
    let visible = window.slice(&matches);

    let art_context = art_state.read().clone();
    let downloads_snapshot: Downloads = (*board.read()).clone();
    let installs_snapshot: Installs = (*installs.read()).clone();
    // Whether the store queue has a worker; the Install button is disabled until
    // the startup future publishes its sender.
    let install_ready = (*install_events.read()).is_some();
    let libraries = state.libraries.clone();
    let events_sender: Option<UnboundedSender<DownloadEvent>> = (*events.read()).clone();
    let db_handle = (*db.read()).clone();

    let warning_count = state.warnings.len();
    let metadata_notes = state.metadata_warnings.clone();
    let metadata_warning_count = metadata_notes.len();
    let has_games = !state.games.is_empty();

    // Bound as locals so the RSX format strings only interpolate identifiers.
    let columns = geometry.columns;
    let row_gap = grid::ROW_GAP;
    let column_gap = grid::COLUMN_GAP;
    let h_padding = grid::H_PADDING;
    let card_height = geometry.art_height() + grid::TILE_CHROME;
    let total_height = window.total_height;
    let offset_y = window.offset_y;

    rsx! {
        if warning_count > 0 {
            div {
                style: "margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#2a2420;color:#e0b090;font-size:12px;",
                "{warning_count} scan warning(s) — run `pitagames-cli scan --json` for detail."
            }
        }

        if metadata_warning_count > 0 {
            div {
                style: "margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#1e2430;color:#9ab0d8;font-size:12px;display:flex;flex-direction:column;gap:4px;",
                for note in metadata_notes.iter().take(2) {
                    span { "{note}" }
                }
                if metadata_warning_count > 2 {
                    span { "…and {metadata_warning_count - 2} more metadata warning(s)." }
                }
            }
        }

        if !has_games {
            div {
                style: "flex:1;padding:48px 32px;color:#78788a;",
                "No games yet. PitaGames reads Steam's own libraries; open Stores and choose \"Sync libraries\" to fetch your Epic, Amazon and GOG games."
            }
        } else if window.is_empty() {
            div {
                style: "flex:1;padding:48px 32px;color:#78788a;",
                "Nothing matches \"{needle}\"."
            }
        } else {
            // The scroll viewport fills the window; the page itself never
            // scrolls, so `onscroll` reports this container's offset.
            div {
                style: "flex:1;min-height:0;overflow-y:auto;overflow-x:hidden;",
                onscroll: move |event| {
                    scroll_top.set(event.scroll_top());
                    if event.client_height() > 0 {
                        viewport_height.set(event.client_height() as f64);
                    }
                    if event.client_width() > 0 {
                        viewport_width.set(event.client_width() as f64);
                    }
                },
                onresize: move |event| {
                    // The content box matches `clientWidth`/`clientHeight` on
                    // the first measurement (no padding, scrollbar excluded).
                    if let Ok(size) = event.get_content_box_size() {
                        if size.width > 0.0 {
                            viewport_width.set(size.width);
                        }
                        if size.height > 0.0 {
                            viewport_height.set(size.height);
                        }
                    }
                },

                div {
                    // Full-height sizer. Its height covers every row, so the
                    // scrollbar is accurate while only the window is drawn.
                    style: "position:relative;height:{total_height}px;",

                    div {
                        style: "position:absolute;left:0;right:0;top:{offset_y}px;display:grid;grid-template-columns:repeat({columns},minmax(0,1fr));gap:{row_gap}px {column_gap}px;padding:0 {h_padding}px;",

                        for entry in visible {
                            {
                                // Every closure below needs owned data: rsx
                                // children outlive this iteration.
                                let app_id = entry.app_id().to_string();
                                let title = entry.title().to_string();
                                let game_id = entry.game.id.clone();
                                let art_url = art_context.as_ref().and_then(|context| {
                                    let metadata = entry.store_metadata();
                                    context.url_for(
                                        &[
                                            metadata
                                                .and_then(|metadata| metadata.capsule_image.as_deref()),
                                            metadata
                                                .and_then(|metadata| metadata.header_image.as_deref()),
                                        ],
                                        pitagames_art::ArtSize::Grid,
                                    )
                                });
                                // Placeholder letter while the art downloads: an
                                // empty box would look like a missing image.
                                let initial = title
                                    .chars()
                                    .next()
                                    .map(|first| first.to_uppercase().to_string())
                                    .unwrap_or_default();
                                let playtime = entry.playtime();
                                let meta = entry.meta_line();
                                let status = entry.status_label().unwrap_or_default();
                                let can_play = entry.can_play();
                                // Steam launches through its own client; every
                                // other store goes through the umu hand-off,
                                // which needs the listing's executable.
                                let show_play = entry.game.installed && entry.play_is_wired();
                                let playing = play.read().label(&game_id);
                                let can_install = entry.can_install();
                                let install_label = if entry.phase == pitagames_stores::InstallPhase::UpdateAvailable {
                                    "Update"
                                } else {
                                    "Install"
                                };
                                // A queued/running CLI-store install owns the
                                // card's action slot, so a second click cannot
                                // queue a duplicate.
                                let queued = installs_snapshot.active_label(&game_id);
                                let steam_install = entry.is_steam();
                                let tracked = downloads_snapshot
                                    .get(&app_id)
                                    .is_some_and(|row| !row.state.is_finished());

                                let play_id = app_id.clone();
                                let play_db = db_handle.clone();
                                let play_game_id = game_id.clone();
                                let steam_play = entry.is_steam();
                                let play_game = entry.game.clone();
                                let install_id = app_id.clone();
                                let install_game = game_id.clone();
                                let install_title = title.clone();
                                let card_key = format!("{}-{app_id}", entry.store().as_str());
                                let install_sender = events_sender.clone();
                                let install_enabled = if steam_install {
                                    install_sender.is_some()
                                } else {
                                    install_ready
                                };
                                let install_libraries = libraries.clone();
                                let open_id = game_id.clone();

                                rsx! {
                                    div {
                                        key: "{card_key}",
                                        onclick: move |_| {
                                            selected_game.set(Some(open_id.clone()));
                                            view.set(View::Detail);
                                        },
                                        style: "display:flex;flex-direction:column;gap:6px;padding:14px;box-sizing:border-box;height:{card_height}px;overflow:hidden;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;cursor:pointer;",

                                        if let Some(url) = art_url {
                                            img {
                                                src: "{url}",
                                                alt: "",
                                                style: "width:100%;aspect-ratio:231/87;object-fit:cover;border-radius:8px;display:block;background:#2c2c38;",
                                            }
                                        } else {
                                            div {
                                                style: "width:100%;aspect-ratio:231/87;border-radius:8px;display:flex;align-items:center;justify-content:center;background:linear-gradient(135deg,#26263a,#191922);color:#414158;font-size:26px;font-weight:600;",
                                                "{initial}"
                                            }
                                        }

                                        div {
                                            style: "font-size:14px;font-weight:600;color:#ffffff;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                            title: "{title}",
                                            "{title}"
                                        }

                                        if !status.is_empty() {
                                            div {
                                                style: "font-size:10px;text-transform:uppercase;letter-spacing:0.06em;color:#d8b070;",
                                                "{status}"
                                            }
                                        }

                                        div {
                                            style: "font-size:11px;color:#78788a;",
                                            "{meta}"
                                        }

                                        div {
                                            style: "display:flex;gap:8px;margin-top:auto;padding-top:8px;",

                                            if let Some(label) = playing {
                                                span {
                                                    style: "flex:1;align-self:center;font-size:11px;color:#8fd0a8;",
                                                    "{label}"
                                                }
                                            } else if show_play {
                                                button {
                                                    disabled: !can_play,
                                                onclick: move |event| {
                                                    // The card itself opens the detail page;
                                                    // the buttons below only do their own action.
                                                    event.stop_propagation();
                                                    // Pointer clicks are M0-only; controller
                                                    // navigation through gilrs arrives in M2.5.
                                                    notice.set(String::new());
                                                    if steam_play {
                                                        if let Err(error) = library::spawn_steam(&play_id) {
                                                            notice.set(format!("Could not ask Steam to run appid {play_id}: {error}"));
                                                            return;
                                                        }
                                                        if let Some(db) = play_db.clone() {
                                                            let id = play_game_id.clone();
                                                            spawn(async move {
                                                                if let Err(error) = persist::record_launch(&db, &id, playtime).await {
                                                                    notice.set(error);
                                                                }
                                                            });
                                                        }
                                                        return;
                                                    }
                                                    // The hand-off owns the process: the worker
                                                    // starts it and the header strip carries Stop.
                                                    let Some(sender) = play_events.read().clone() else {
                                                        notice.set("The launcher is still starting; try again in a moment.".to_string());
                                                        return;
                                                    };
                                                    if let Err(error) = play.write().begin(&play_game) {
                                                        notice.set(error);
                                                        return;
                                                    }
                                                    play::spawn(play_game.clone(), sender);
                                                },
                                                    style: button_style(can_play, "#2e3a56", "#ffffff"),
                                                    "Play"
                                                }
                                            }

                                            if let Some(label) = queued {
                                                span {
                                                    style: "flex:1;align-self:center;font-size:11px;color:#7a9ad6;",
                                                    "{label}"
                                                }
                                            } else if can_install {
                                                button {
                                                    disabled: !install_enabled,
                                                    onclick: move |event| {
                                                        // The card itself opens the detail page;
                                                        // the buttons below only do their own action.
                                                        event.stop_propagation();
                                                        notice.set(String::new());

                                                        // The CLI stores run their own client, so
                                                        // the queue owns the install and the
                                                        // Downloads board shows it.
                                                        if !steam_install {
                                                            match pitagames_stores::installer::plan(&install_game, &install_title) {
                                                                Ok(plan) => {
                                                                    if let Err(error) = installs.write().enqueue(plan.job()) {
                                                                        notice.set(error.to_string());
                                                                        return;
                                                                    }
                                                                    pump_installs(installs, install_events, notice);
                                                                    view.set(View::Downloads);
                                                                }
                                                                Err(error) => notice.set(error.to_string()),
                                                            }
                                                            return;
                                                        }

                                                        if let Err(error) = library::spawn_install(&install_id) {
                                                            notice.set(format!("Could not ask Steam to install appid {install_id}: {error}"));
                                                            return;
                                                        }
                                                        match install_sender.as_ref() {
                                                            Some(sender) => {
                                                                start_download(board, sender, &install_libraries, &install_id, &install_title);
                                                                view.set(View::Downloads);
                                                            }
                                                            None => notice.set("Downloads are not ready yet; try again in a moment.".to_string()),
                                                        }
                                                    },
                                                    style: button_style(install_enabled, "#243024", "#d8e8d0"),
                                                    "{install_label}"
                                                }
                                            } else if tracked {
                                                span {
                                                    style: "flex:1;align-self:center;font-size:11px;color:#7a9ad6;",
                                                    "in downloads"
                                                }
                                            } else if !show_play {
                                                span {
                                                    style: "flex:1;align-self:center;font-size:11px;color:#78788a;",
                                                    "installed"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn nav_style(active: bool) -> &'static str {
    if active {
        "padding:6px 14px;border-radius:999px;border:1px solid #3f5c8c;background:#2e3a56;color:#ffffff;font-size:12px;cursor:pointer;"
    } else {
        "padding:6px 14px;border-radius:999px;border:1px solid #2c2c38;background:#1e1e28;color:#a8a8b8;font-size:12px;cursor:pointer;"
    }
}

/// Card action button. A disabled action must look disabled: the grid relies on
/// it to show "Steam is busy" without a separate widget.
fn button_style(enabled: bool, background: &str, color: &str) -> String {
    if enabled {
        format!(
            "flex:1;padding:8px 0;border-radius:8px;border:1px solid #3a3a4c;background:{background};color:{color};font-size:13px;cursor:pointer;"
        )
    } else {
        "flex:1;padding:8px 0;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:#5a5a68;font-size:13px;cursor:default;"
            .to_string()
    }
}

/// Track an install: register the row, then start the polling thread.
///
/// Returns `false` when the appid is already being tracked, so a double click
/// does not spawn a second thread.
fn start_download(
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
fn pump_installs(
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
                if let Some(job) = job {
                    store_installs::spawn(job, index, sender.clone());
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

/// The single metadata worker: one client, one rate budget, many batches.
///
/// The startup scan, the owned-games refresh and any later enqueuer send
/// `Vec<u32>` into this task, which fetches them in chunks and folds each chunk
/// into the library and SQLite as it lands. One client means one token bucket,
/// so a burst of owned appids cannot multiply Steam's 200/5-minute budget.
///
/// A transport failure ends the current batch, keeps whatever was cached and
/// leaves the rest stale for the next launch; the worker then waits for more.
async fn metadata_worker(
    db: pitagames_db::Db,
    mut library_state: Signal<Option<Library>>,
    mut receiver: UnboundedReceiver<Vec<u32>>,
    queued: Arc<AtomicUsize>,
    art_context: Option<std::sync::Arc<art::ArtContext>>,
    art_revision: Signal<u64>,
) {
    let mut client = match pitagames_metadata::SteamClient::new() {
        Ok(client) => Some(client),
        Err(error) => {
            note_metadata_problem(
                library_state,
                format!("Store metadata is unavailable: {error}"),
            );
            if let Some(library) = library_state.write().as_mut() {
                library.metadata_pending = false;
            }
            None
        }
    };

    while let Some(app_ids) = receiver.next().await {
        let count = app_ids.len();
        if let Some(client) = client.as_mut() {
            fetch_batch(
                client,
                &db,
                library_state,
                &app_ids,
                art_context.as_ref(),
                art_revision,
            )
            .await;
        }

        // The batch is done. Another enqueuer may already be waiting; only when
        // nothing is queued does the "fetching…" indicator come down.
        let remaining = queued.load(Ordering::SeqCst).saturating_sub(count);
        queued.store(remaining, Ordering::SeqCst);
        if remaining == 0
            && let Some(library) = library_state.write().as_mut()
        {
            library.metadata_pending = false;
        }
    }
}

/// Fetch and persist one batch of appids, in chunks.
///
/// Each chunk is persisted immediately, so a large batch or a mid-fetch network
/// error keeps everything fetched so far instead of throwing the batch away.
async fn fetch_batch(
    client: &mut pitagames_metadata::SteamClient,
    db: &pitagames_db::Db,
    mut library_state: Signal<Option<Library>>,
    app_ids: &[u32],
    art_context: Option<&std::sync::Arc<art::ArtContext>>,
    art_revision: Signal<u64>,
) {
    const CHUNK: usize = 20;

    let requested = app_ids.len();
    let mut failures = 0usize;
    let mut stopped = false;

    for chunk in app_ids.chunks(CHUNK) {
        let fetched = match client.fetch(chunk).await {
            Ok(fetched) => fetched,
            Err(error) => {
                // Transport failures are never cached: the ids in this chunk
                // stay stale and the next launch asks again.
                note_metadata_problem(
                    library_state,
                    format!("Store metadata could not be fetched: {error}"),
                );
                stopped = true;
                break;
            }
        };

        failures += fetched.failures.len();
        let rows = metadata_rows(fetched);
        if let Err(error) = persist::save_metadata(db, &rows).await {
            note_metadata_problem(
                library_state,
                format!("Store metadata was not saved: {error}"),
            );
            stopped = true;
            break;
        }

        // Show each chunk as it lands; `metadata_pending` stays on until the
        // whole batch (and everything queued behind it) finishes.
        if let Some(library) = library_state.write().as_mut() {
            library.apply_metadata(&rows);
        }

        // Art for this chunk: requested only after the metadata is on disk so
        // a failed save cannot leave images for rows the UI will not show.
        if let Some(context) = art_context {
            let requests = art::requests_for_rows(&rows);
            if !requests.is_empty() {
                spawn(warm_art(context.clone(), requests, art_revision));
            }
        }
    }

    if !stopped && failures > 0 {
        note_metadata_problem(
            library_state,
            format!("Steam has no store data for {failures} of {requested} appid(s)."),
        );
    }
}

/// Refresh the account's owned list and fold it into the library.
///
/// One `GetOwnedGames` call returns the whole account library, so this is a
/// single request with no rate budget of its own. The answer is cached in
/// SQLite; a failure keeps the cached rows and records a warning.
///
/// Newly discovered owned appids are handed to the metadata worker, which is
/// the only place that talks to `appdetails`.
async fn refresh_owned(
    key: pitagames_secrets::SteamApiKey,
    steam_id: String,
    mut library_state: Signal<Option<Library>>,
    db: Option<pitagames_db::Db>,
    queue: Option<MetadataQueue>,
) {
    let owned = match steam_account::fetch_owned(&key, &steam_id).await {
        Ok(owned) => owned,
        Err(error) => {
            if let Some(library) = library_state.write().as_mut() {
                library.owned_failed(error);
            }
            return;
        }
    };

    let fetched_at = OffsetDateTime::now_utc();
    let total = usize::try_from(owned.game_count).unwrap_or(owned.games.len());

    if let Some(db) = db.as_ref()
        && let Err(error) = persist::save_owned(db, &owned.games, fetched_at).await
    {
        if let Some(library) = library_state.write().as_mut() {
            library.owned_failed(error);
        }
        return;
    }

    // Merge first, then ask the cache which of the new rows still need store
    // metadata. The flag is cleared here so the panel stops saying "refreshing".
    let metadata_targets = {
        let mut state = library_state.write();
        let Some(library) = state.as_mut() else {
            return;
        };
        library.apply_owned(&owned.games, total, fetched_at);
        library.owned.pending = false;
        library.metadata_candidates(
            CachePolicy::DEFAULT,
            OffsetDateTime::now_utc(),
            OWNED_METADATA_PER_RUN,
        )
    };

    if let Some(queue) = queue.as_ref()
        && queue.enqueue(metadata_targets)
        && let Some(library) = library_state.write().as_mut()
    {
        library.metadata_pending = true;
    }
}

/// Turn one fetch into cache rows.
///
/// A failure is stored as a `None` payload (a negative cache entry), so a
/// delisted app is not retried on every launch.
fn metadata_rows(fetched: pitagames_metadata::Fetched) -> Vec<StoredMetadata> {
    let now = OffsetDateTime::now_utc();
    let mut rows = Vec::with_capacity(fetched.items.len() + fetched.failures.len());
    for metadata in fetched.items {
        rows.push(StoredMetadata {
            game: metadata.game.clone(),
            metadata: Some(metadata),
            fetched_at: now,
        });
    }
    for failure in &fetched.failures {
        rows.push(StoredMetadata {
            game: GameId::new(Store::Steam, failure.app_id.to_string()),
            metadata: None,
            fetched_at: now,
        });
    }
    rows
}

/// Download missing art, four at a time, and repaint as each image lands.
///
/// Art failures are logged by [`art::ArtContext::warm`] and otherwise ignored:
/// a missing capsule is a placeholder, never an error banner.
async fn warm_art(
    context: std::sync::Arc<art::ArtContext>,
    requests: Vec<pitagames_art::ArtRequest>,
    mut revision: Signal<u64>,
) {
    stream::iter(requests)
        .map(|request| {
            let context = context.clone();
            async move { context.warm(&request).await }
        })
        .buffer_unordered(4)
        .for_each(|downloaded| {
            if downloaded {
                revision.set(revision() + 1);
            }
            async {}
        })
        .await;
}

/// Record a metadata problem as a warning.
///
/// The worker clears `metadata_pending` when its queue drains, so a warning
/// here never flips that flag: more batches may already be queued.
fn note_metadata_problem(mut library_state: Signal<Option<Library>>, message: String) {
    if let Some(library) = library_state.write().as_mut() {
        library.metadata_warnings.push(message);
    }
}

/// End-to-end wiring of the data layer behind the window: a real Steam tree on
/// disk, a real SQLite file, no Dioxus. This is the closest a container can get
/// to "open the launcher".
#[cfg(test)]
mod wiring_tests {
    use std::fs;
    use std::time::Duration;

    use pitagames_core::Profile;
    use pitagames_db::StoredMetadata;
    use pitagames_metadata::CachePolicy;
    use pitagames_stores::InstallPhase;

    use pitagames_metadata::{AppFailure, FailureReason, Fetched};

    use crate::library::Library;
    use crate::persist;

    #[test]
    fn a_fetch_becomes_positive_and_negative_cache_rows() {
        let metadata = pitagames_core::GameMetadata::new(
            pitagames_core::GameId::new(pitagames_core::Store::Steam, "620"),
            "Portal 2",
        );
        let rows = crate::metadata_rows(Fetched {
            items: vec![metadata],
            failures: vec![AppFailure {
                app_id: 999,
                reason: FailureReason::NoData,
            }],
        });

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].game.store_id, "620");
        assert!(rows[0].metadata.is_some());
        // A failure is cached as "the store has no data", not as an error.
        assert_eq!(rows[1].game.store_id, "999");
        assert!(rows[1].metadata.is_none());
        assert_eq!(
            rows[0].fetched_at, rows[1].fetched_at,
            "one fetch produces one timestamp"
        );
    }

    fn manifest(app_id: u32, name: &str, flags: u32) -> String {
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{app_id}\"\n\t\"name\"\t\t\"{name}\"\n\t\"StateFlags\"\t\"{flags}\"\n\t\"installdir\"\t\"{name}\"\n}}\n"
        )
    }

    #[tokio::test]
    async fn a_scan_survives_a_sqlite_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let steam = root.path().join("Steam");
        let steamapps = steam.join("steamapps");
        fs::create_dir_all(steamapps.join("common/Portal 2")).unwrap();
        fs::write(
            steamapps.join("appmanifest_620.acf"),
            manifest(620, "Portal 2", 4),
        )
        .unwrap();

        let data = tempfile::tempdir().unwrap();
        // SAFETY: this is the only test in the binary that touches the
        // environment, and no other test reads these variables.
        unsafe {
            std::env::set_var("PITAGAMES_STEAM_ROOT", &steam);
            std::env::set_var("XDG_DATA_HOME", data.path());
        }

        let mut library = Library::load();

        let entry = library
            .games
            .iter()
            .find(|entry| entry.app_id() == "620")
            .expect("the fixture game is in the scan")
            .clone();
        assert_eq!(entry.phase, InstallPhase::Installed);
        assert!(entry.can_play());
        assert!(!library.libraries.is_empty(), "trackers need libraries");

        let db = persist::open_default()
            .await
            .expect("the database opens under XDG_DATA_HOME");
        let stored = persist::sync(&db, &library.snapshot_games())
            .await
            .expect("the scan is saved");
        library.apply_stored(&stored);

        persist::record_launch(&db, &entry.game.id, Duration::from_secs(600))
            .await
            .expect("the launch is recorded");

        // A later launch of the app scans again and must see the playtime.
        let mut rescanned = Library::load();
        let stored = persist::sync(&db, &rescanned.snapshot_games())
            .await
            .expect("the second scan is saved");
        rescanned.apply_stored(&stored);

        let entry = rescanned
            .games
            .iter()
            .find(|entry| entry.app_id() == "620")
            .expect("the fixture game is still there");
        assert_eq!(entry.playtime(), Duration::from_secs(600));
        assert!(entry.game.last_played.is_some());

        // Profiles round-trip through the database the detail page uses.
        let game_id = entry.game.id.clone();
        persist::save_profile(&db, &Profile::new(game_id.clone(), "Default"))
            .await
            .expect("the profile is saved");
        let profiles = persist::profiles(&db, &game_id)
            .await
            .expect("the profiles load");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].profile.name, "Default");
        assert!(
            persist::delete_profile(&db, &game_id, "Default")
                .await
                .expect("the profile is deleted")
        );
        assert!(
            persist::profiles(&db, &game_id)
                .await
                .expect("the profiles reload")
                .is_empty()
        );

        // Steam's own playtime from localconfig.vdf must win on the next scan.
        fs::create_dir_all(steam.join("userdata/12345/config")).unwrap();
        fs::write(
            steam.join("userdata/12345/config/localconfig.vdf"),
            "\"localconfig\"\n{\n\t\"apps\"\n\t{\n\t\t\"620\"\n\t\t{\n\t\t\t\"Playtime\"\t\"30\"\n\t\t\t\"LastPlayed\"\t\"1700000000\"\n\t\t}\n\t}\n}\n",
        )
        .unwrap();

        let mut rescanned = Library::load();
        let stored = persist::sync(&db, &rescanned.snapshot_games())
            .await
            .expect("the third scan is saved");
        rescanned.apply_stored(&stored);

        let entry = rescanned
            .games
            .iter()
            .find(|entry| entry.app_id() == "620")
            .expect("the fixture game is still there");
        assert_eq!(entry.playtime(), Duration::from_secs(30 * 60));
        assert_eq!(
            entry.game.last_played.map(|played| played.unix_timestamp()),
            Some(1_700_000_000)
        );

        // Store metadata is cached in the same database and folded into the
        // next scan without touching the library rows.
        let game_id = entry.game.id.clone();
        assert_eq!(
            rescanned.metadata_candidates(
                CachePolicy::DEFAULT,
                time::OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
                usize::MAX,
            ),
            vec![620],
            "a fresh install has no cached metadata yet"
        );

        let fetched_at = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let mut metadata = pitagames_core::GameMetadata::new(game_id.clone(), "Portal 2");
        metadata.developers = vec!["Valve".into()];
        metadata.genres = vec!["Action".into(), "Adventure".into()];
        let row = StoredMetadata {
            game: game_id.clone(),
            metadata: Some(metadata),
            fetched_at,
        };
        persist::save_metadata(&db, std::slice::from_ref(&row))
            .await
            .expect("the metadata row is saved");
        assert_eq!(
            persist::load_metadata(&db).await.expect("metadata loads"),
            vec![row]
        );

        let mut reopened = Library::load();
        reopened.apply_metadata(&persist::load_metadata(&db).await.unwrap());
        let entry = reopened
            .games
            .iter()
            .find(|entry| entry.app_id() == "620")
            .expect("the fixture game is still there");
        let store_metadata = entry.store_metadata().expect("metadata is on the row");
        assert_eq!(store_metadata.developers, ["Valve"]);
        assert_eq!(entry.metadata_fetched_at(), Some(fetched_at));
        assert!(
            reopened
                .metadata_candidates(
                    CachePolicy::DEFAULT,
                    time::OffsetDateTime::from_unix_timestamp(1_700_000_001).unwrap(),
                    usize::MAX,
                )
                .is_empty(),
            "a fresh cache answer means no fetch is needed"
        );

        unsafe {
            std::env::remove_var("PITAGAMES_STEAM_ROOT");
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    /// The owned-games cache round trip: the API answer is saved, read back and
    /// merged onto a scanned library, so a later launch shows owned-but-uninstalled
    /// games without another request.
    #[tokio::test]
    async fn an_owned_refresh_round_trips_through_sqlite() {
        let data = tempfile::tempdir().unwrap();
        let db_path = data.path().join("pitagames.db");
        let db = pitagames_db::Db::open(&db_path).await.unwrap();

        let fetched_at = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let mut portal = pitagames_core::OwnedGame::new(
            pitagames_core::GameId::new(pitagames_core::Store::Steam, "620"),
            "Portal 2",
        );
        portal.playtime = Duration::from_secs(412 * 60);
        let tf2 = pitagames_core::OwnedGame::new(
            pitagames_core::GameId::new(pitagames_core::Store::Steam, "440"),
            "Team Fortress 2",
        );

        persist::save_owned(&db, &[portal, tf2.clone()], fetched_at)
            .await
            .expect("the owned list is saved");

        let (owned, read_back) = persist::load_owned(&db)
            .await
            .expect("the owned list loads");
        assert_eq!(read_back, Some(fetched_at));
        assert_eq!(owned.len(), 2);

        // A scan that found only Portal 2 installed gains TF2 as an owned row.
        let mut library = Library::default();
        let mut installed = pitagames_core::Game::new(
            pitagames_core::GameId::new(pitagames_core::Store::Steam, "620"),
            "Portal 2",
        );
        installed.installed = true;
        installed.playtime = Duration::from_secs(99 * 60);
        library.games.push(crate::library::GameEntry {
            game: installed,
            phase: InstallPhase::Installed,
            metadata: None,
            owned: false,
        });

        library.apply_owned(&owned, 2, fetched_at);

        let portal = library
            .games
            .iter()
            .find(|entry| entry.app_id() == "620")
            .unwrap();
        assert!(portal.owned && portal.game.installed);
        assert_eq!(
            portal.playtime(),
            Duration::from_secs(99 * 60),
            "the scan's playtime is kept"
        );

        let tf2 = library
            .games
            .iter()
            .find(|entry| entry.app_id() == "440")
            .unwrap();
        assert!(!tf2.game.installed);
        assert!(tf2.can_install());
        assert_eq!(tf2.playtime(), Duration::ZERO);

        // Forgetting the key clears the cache so the next launch is installed-only.
        assert_eq!(
            persist::clear_owned(&db).await.expect("the cache clears"),
            2
        );
        let (owned, read_back) = persist::load_owned(&db).await.unwrap();
        assert!(owned.is_empty());
        assert_eq!(read_back, None);

        library.drop_owned();
        assert!(
            library.games.iter().all(|entry| entry.app_id() == "620"),
            "the owned-only row is gone"
        );
        assert_eq!(library.owned.total, None);
    }
}
