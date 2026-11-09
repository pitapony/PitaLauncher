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
use pitagames_core::{GameId, Store};
use pitagames_db::StoredMetadata;
use pitagames_metadata::CachePolicy;
use time::OffsetDateTime;

use downloads::{DownloadEvent, Downloads};
use library::{Library, OWNED_METADATA_PER_RUN};
use steam_account::Account;

mod account;
mod art;
mod detail;
mod downloads;
mod format;
mod library;
mod persist;
mod runtime;
mod steam_account;
mod wrappers;

fn main() {
    // A desktop window, not fullscreen: the 10-foot console shell is M2.5.
    let window = dioxus::desktop::WindowBuilder::new()
        .with_title("PitaGames")
        .with_inner_size(dioxus::desktop::LogicalSize::new(1280.0, 800.0));

    dioxus::LaunchBuilder::desktop()
        .with_cfg(dioxus::desktop::Config::new().with_window(window))
        .launch(App);
}

/// Which top-level list the window shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Library,
    Downloads,
    Detail,
    Account,
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
    // the 200/5-minute budget is global rather than per task.
    let mut metadata_queue = use_signal(|| None::<MetadataQueue>);

    // One task owns the background work: scan, persist, then drain download
    // events for the lifetime of the window. Scans are file IO only, so running
    // them here (once) is cheaper than a worker pool.
    use_future(move || async move {
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
    let active_count = downloads_snapshot.active_count();
    let nothing_tracked = downloads_snapshot.rows().is_empty();
    let has_finished = !finished_downloads.is_empty();
    let current_view = view();
    let events_sender: Option<UnboundedSender<DownloadEvent>> = (*events.read()).clone();
    let db_handle = (*db_state.read()).clone();
    let owned_summary = snapshot
        .as_ref()
        .map(|library| library.owned.clone())
        .unwrap_or_default();

    let subtitle = match snapshot.as_ref() {
        None => "Scanning…".to_string(),
        Some(library) => {
            let mut subtitle = match &library.steam_root {
                Some(root) => format!("{} Steam games · {}", library.games.len(), root.display()),
                None => "No Steam library on this machine".to_string(),
            };
            if let Some(total) = library.owned.total {
                subtitle.push_str(&format!(" · {total} owned"));
            }
            if library.metadata_pending {
                subtitle.push_str(" · fetching store metadata…");
            }
            subtitle
        }
    };

    let library_body = match snapshot {
        None => rsx! {
            div {
                style: "padding:32px;color:#78788a;",
                "Scanning Steam libraries…"
            }
        },
        Some(library) => {
            let needle = query.read().to_string();
            let matches: Vec<library::GameEntry> =
                library.filtered(&needle).into_iter().cloned().collect();
            let libraries = library.libraries.clone();
            let warning_count = library.warnings.len();
            let metadata_notes = library.metadata_warnings.clone();
            let metadata_warning_count = metadata_notes.len();
            let has_games = !library.games.is_empty();

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
                        style: "padding:48px 32px;color:#78788a;",
                        "No installed Steam games found."
                    }
                } else if matches.is_empty() {
                    div {
                        style: "padding:48px 32px;color:#78788a;",
                        "Nothing matches \"{query}\"."
                    }
                } else {
                    div {
                        style: "display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:16px;padding:0 32px 40px;align-content:start;",
                        for entry in matches {
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
                                let show_play = entry.game.installed;
                                let can_install = entry.can_install();
                                let install_label = if entry.phase == pitagames_stores::InstallPhase::UpdateAvailable {
                                    "Update"
                                } else {
                                    "Install"
                                };
                                let tracked = downloads_snapshot
                                    .get(&app_id)
                                    .is_some_and(|row| !row.state.is_finished());

                                let play_id = app_id.clone();
                                let play_db = db_handle.clone();
                                let play_game_id = game_id.clone();
                                let install_id = app_id.clone();
                                let install_title = title.clone();
                                let install_sender = events_sender.clone();
                                let install_enabled = install_sender.is_some();
                                let install_libraries = libraries.clone();
                                let open_id = game_id.clone();

                                rsx! {
                                    div {
                                        key: "{app_id}",
                                        onclick: move |_| {
                                            selected_game.set(Some(open_id.clone()));
                                            view.set(View::Detail);
                                        },
                                        style: "display:flex;flex-direction:column;gap:6px;padding:14px;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;cursor:pointer;",

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

                                            if show_play {
                                                button {
                                                    disabled: !can_play,
                                                onclick: move |event| {
                                                    // The card itself opens the detail page;
                                                    // the buttons below only do their own action.
                                                    event.stop_propagation();
                                                    // Pointer clicks are M0-only; controller
                                                    // navigation through gilrs arrives in M2.5.
                                                    notice.set(String::new());
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
                                                },
                                                    style: button_style(can_play, "#2e3a56", "#ffffff"),
                                                    "Play"
                                                }
                                            }

                                            if can_install {
                                                button {
                                                    disabled: !install_enabled,
                                                    onclick: move |event| {
                                                        event.stop_propagation();
                                                        notice.set(String::new());
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
            style: "display:flex;flex-direction:column;min-height:100vh;background:#14141c;color:#e8e8ee;font-family:sans-serif;",

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
                        view.set(View::Account);
                    },
                    style: nav_style(current_view == View::Account),
                    "Account"
                }
            }

            if !notice.read().is_empty() {
                div {
                    style: "margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#3a1e1e;color:#e8a0a0;font-size:12px;",
                    "{notice}"
                }
            }

            if current_view == View::Detail {
                if let Some(entry) = detail_entry {
                    detail::Detail {
                        key: "{entry.app_id()}",
                        entry,
                        db: detail::DbHandle(db_handle.clone()),
                        view,
                        hero: hero_url,
                        steam_root: steam_root.clone(),
                        proton: proton_report.clone(),
                    }
                }
            } else if current_view == View::Downloads {
                {downloads_body}
            } else if current_view == View::Account {
                account::AccountView {
                    account: account_state,
                    owned: owned_summary,
                    steam_root: steam_root.clone(),
                    db: detail::DbHandle(db_handle.clone()),
                    metadata: metadata_queue,
                    library: library_state,
                }
            } else {
                {library_body}
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
