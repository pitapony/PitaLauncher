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

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dioxus::desktop::use_wry_event_handler;
use dioxus::html::geometry::PixelsVector2D;
use dioxus::html::{MountedData, ScrollBehavior};
use dioxus::prelude::*;
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures_util::stream::{self, StreamExt};
use pitagames_core::ui::Shell;
use pitagames_core::{GameId, Store};
use pitagames_core::i18n::t;
use pitagames_db::StoredMetadata;
use pitagames_metadata::CachePolicy;
use pitagames_stores::queue::{InstallJob, JobState};
use time::OffsetDateTime;

use downloads::{DownloadEvent, Downloads};
use handoff::Handoff;
use library::{Library, OWNED_METADATA_PER_RUN};
use overlay::{Overlay, OverlayChoice, OverlayContext, OverlayGame};
use play::{Play, PlayEvent};
use steam_account::Account;
use store_installs::{InstallEvent, Installs};

mod account;
mod activation;
mod anticheat;
mod art;
mod auth;
mod context;
mod controls;
mod copy;
mod desktop_entry;
mod detail;
mod diagnostics;
mod downloads;
mod filter;
mod focus;
mod format;
mod game_page;
mod gamepad;
mod grid;
mod handoff;
mod history_view;
mod home;
mod home_view;
mod icon;
mod launch_start;
mod library;
mod log_view;
mod navigation;
mod osk;
mod overlay;
mod overlay_view;
mod persist;
mod play;
mod pumps;
mod runtime;
mod safe_mode;
mod session;
mod shell;
mod steam_account;
mod store_installs;
mod store_sync;
mod stores_view;
mod supervisor;
mod terminal;
mod tray;
mod wrappers;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = shell::parse_args(argv.iter().cloned());

    // `--version` and `-V` are for the package: a bug report or a support
    // question can name the build without opening a webview.
    if args.version {
        println!("pitagames {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // The GUI child the supervisor spawned owns the window; it must not take
    // the lock or spawn another supervisor.
    if safe_mode::is_child() {
        run_gui(&args);
        return;
    }

    for unknown in &args.unknown {
        eprintln!("[pitagames] ignoring unknown argument {unknown}");
    }

    // The persisted preference is read here so the supervisor knows whether it
    // starts in safe mode; the GUI child reads it again for the shell and CSS.
    let (preferences, warnings) = pitagames_core::ui::AppConfig::load_default();
    for warning in &warnings {
        eprintln!("[pitagames] config: {warning}");
    }

    // One launcher at a time. Two instances would scan the same libraries and
    // race over Steam's config files; the lock is held for the whole run and
    // released by the kernel when this process exits.
    let _lock = take_instance_lock();

    // The supervisor runs the GUI as a child so it can retry into safe mode when
    // the webview fails to initialize and restart it when it crashes.
    let safe = safe_mode::enabled(args.safe_mode, preferences.ui.safe_mode);
    std::process::exit(supervisor::run(safe, &argv));
}

/// Build the window and run the GUI.
///
/// This is the supervisor's child process ([`safe_mode::CHILD_ENV`]): it owns
/// the activation socket and the webview, and writes the ready file once the
/// first render is up so the supervisor can tell a real exit from a failed
/// start.
fn run_gui(args: &shell::ShellArgs) {
    let (preferences, _warnings) = pitagames_core::ui::AppConfig::load_default();

    // Safe mode: disable the DMABUF renderer before WebKit starts and drop
    // animations and blur for the run. `--safe-mode`, `config.toml` and the
    // supervisor's retry (the environment) all resolve here.
    let safe = safe_mode::enabled(args.safe_mode, preferences.ui.safe_mode);
    let shell = preferences.ui.resolve_start(args.shell);
    let mut display = preferences.ui.display;
    if safe {
        safe_mode::apply_webkit_env();
        display.reduce_animations = true;
        eprintln!("[pitagames] safe mode: DMABUF renderer disabled, animations and blur off");
    }

    // A test hook for the smoke run: pretend the webview failed to initialize so
    // the supervisor's safe-mode retry can be exercised without a broken GPU. It
    // fires only when safe mode is off, so the retried run still succeeds.
    if safe_mode::fake_webview_failure() && !safe {
        eprintln!("[pitagames] test hook: simulating a webview init failure");
        std::process::exit(70);
    }

    // The socket a second launch connects to. Started here, in the process that
    // owns the window, so a request raises *this* window.
    activation::start();
    // A checkout has no packaged menu entry; write one that points here.
    desktop_entry::ensure();

    // The window is built for the resolved shell: decorated and windowed for the
    // desktop, undecorated and fullscreen for the console.
    let window = shell::window_builder(&shell::window_config(shell, &display));

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
        .with_context(shell::ShellStart {
            shell,
            display,
            safe_mode: safe,
            ready_path: safe_mode::ready_path(),
            config_path: pitagames_core::paths::config_file(),
        })
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
                Some(pid) => eprintln!("{}", t("app.already_running_pid").replace("{pid}", &pid.to_string())),
                None => eprintln!("{}", t("app.already_running")),
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

// `View` lives in `navigation` now; re-export it so existing `crate::View`
// paths and `use crate::View` keep working (ADR-0027).
pub(crate) use navigation::View;
// The console top bar draws this ring and `Tab` cycles it, so both read one list.
pub(crate) use navigation::DESTINATIONS;

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
    // The shell the window was opened in and the display numbers both shells
    // read. `main` resolved them from the config and the command line; the app
    // only toggles and remembers.
    let start = use_context::<shell::ShellStart>();
    let shell_state = use_hook(|| shell::ShellState::new(&start));
    // Record the live window so the game's worker thread can map it again when
    // a game ends (an unmapped window would freeze this virtualdom).
    use_hook(shell::install_window);
    // Tell the supervisor the window is up. A webview that failed to initialize
    // never reaches the first render, so a missing ready file makes the
    // supervisor retry in safe mode instead of treating the crash as a normal
    // exit.
    let ready_path = start.ready_path.clone();
    use_hook(move || {
        if let Some(path) = ready_path {
            let _ = std::fs::write(path, std::process::id().to_string());
        }
    });
    let library_state = use_signal(|| None::<Library>);
    let db_state = use_signal(|| None::<pitagames_db::Db>);
    let mut board = use_signal(Downloads::default);
    let events = use_signal(|| None::<UnboundedSender<DownloadEvent>>);
    let mut notice = use_signal(String::new);
    // Art is optional: until the cache/server is up, cards show placeholders.
    let art_state = use_signal(|| None::<std::sync::Arc<art::ArtContext>>);
    // Bumped whenever a new image lands on disk; re-renders the <img> sources.
    let art_revision = use_signal(|| 0u64);
    // Proton builds on this machine; read once, shown on the detail page.
    let proton_state = use_signal(|| None::<pitagames_platform::proton::DiscoveryReport>);
    // The Steam account: key presence, SteamID and what the owned list knows.
    let account_state = use_signal(|| None::<Account>);
    // The single metadata fetch channel; its worker holds the only client, so
    // the 200/5-minute budget is global rather than per-task.
    let metadata_queue = use_signal(|| None::<MetadataQueue>);
    // The CLI stores' install queue and the worker reports feeding it.
    let mut installs = use_signal(Installs::default);
    let install_events = use_signal(|| None::<UnboundedSender<InstallEvent>>);
    // The one game the launcher is running (non-Steam) and its worker reports.
    // Steam is started by its own client, so it never lands here.
    let mut play = use_signal(Play::default);
    let play_events = use_signal(|| None::<UnboundedSender<PlayEvent>>);
    // Bumped whenever a session is written to the launch history, so the detail
    // page's card and the History screen reload without a restart.
    let history_revision = use_signal(|| 0u64);
    // The launch hand-off: the console shell hides its window while a supervised
    // non-Steam game runs and comes back when it ends. `handoff_deadline` is
    // when the return trip stops waiting for focus and shows the fallback badge
    // (a Wayland compositor may refuse to hand focus back).
    let mut handoff = use_signal(Handoff::new);
    let handoff_deadline = use_hook(|| Rc::new(Cell::new(None::<Instant>)));
    // The console overlay: `None` is closed, `Some` is the open menu. It is a
    // signal rather than a route, so the screen beneath keeps its focus and
    // scroll. `M`/the pad's `Start` opens it, and every action is routed into it
    // while it is open.
    let mut overlay = use_signal(|| None::<Overlay>);
    // The last power state per pad, for the overlay's controller line. `gilrs`
    // has no battery event, so the gamepad worker polls and reports changes.
    let battery_map =
        std::collections::BTreeMap::<pitagames_input::DeviceId, pitagames_input::Battery>::new();
    let batteries = use_signal(move || battery_map);
    // The overlay's wall clock; refreshed by the timer below. Local time is read
    // through `overlay::now_local`.
    let clock = use_signal(|| crate::overlay::format_clock(crate::overlay::now_local()));

    // Activation: the socket thread forwards "focus" requests here, and this
    // future raises the window on the event-loop thread (tao window calls are
    // only safe there). With no socket the receiver is `None` and the future
    // returns immediately.
    let requests = use_hook(activation::Requests::take);
    // A second launch asks this instance to come to the front; the pump
    // lives with the other drains.
    pumps::activation_requests(requests);

    // The CLI-store install queue's channel: one future owns the receiver, and
    // the grid holds a sender once the window is up.
    let install_channel = use_hook(store_installs::InstallChannel::new);

    // The play slot's channel, same shape: the grid and the detail page hold a
    // sender, one drain future below owns the receiver.
    let play_channel = use_hook(play::PlayChannel::new);

    // The one input pump's per-screen action channels. The keyboard handler and
    // the gamepad drain below post into whichever screen is showing; each screen
    // owns its own `ScreenFocus` and drains its channel. The Downloads screen's
    // focus lives here because its body is rendered inline in `App`.
    let home_actions = use_hook(focus::ActionChannel::new);
    let library_actions = use_hook(focus::ActionChannel::new);
    let downloads_actions = use_hook(focus::ActionChannel::new);
    let stores_actions = use_hook(focus::ActionChannel::new);
    let detail_actions = use_hook(focus::ActionChannel::new);
    let controls_actions = use_hook(focus::ActionChannel::new);
    let downloads_focus = focus::use_focus();

    // The persisted input maps. The keyboard pump and the gamepad worker both
    // read these, so a remap in `input.toml` applies to every screen's graph.
    // They live in a signal because the Controls screen saves new ones and the
    // hint row, the keyboard handler and the worker all have to follow.
    let seed_maps = use_hook(|| match pitagames_input::InputMaps::load_default() {
        Ok(maps) => maps,
        Err(error) => {
            eprintln!("[pitagames] ignoring input.toml: {error}");
            pitagames_input::InputMaps::new()
        }
    });
    let input_maps = use_signal({
        let seed = seed_maps.clone();
        move || seed
    });
    // The one remap session the Controls screen edits and the input pumps
    // consult. It starts on the keyboard; the screen switches device.
    let remap = use_signal({
        let seed = seed_maps.clone();
        move || {
            Some(pitagames_input::RemapSession::new(
                pitagames_input::RemapTarget::keyboard(),
                &seed,
            ))
        }
    });
    // The pads the worker has named, for the Controls screen's target list.
    let pads = use_signal(Vec::<pitagames_input::Device>::new);

    // The read-only pad-permission probe. It opens `/dev/input/event*` nodes
    // read-only and never grabs, so a game keeps receiving input; the Controls
    // screen shows the one-line fix when the platform denies the user a pad.
    let pad_access = use_signal(|| pitagames_input::assess(&pitagames_input::DevInput::new()));

    // The last device that produced input, for glyph hints and cursor hiding.
    // The keyboard and gamepad pumps set it; a pointer move counts as the
    // desktop side. `pad_active` is `false` until the first pad action.
    let pad_active = use_signal(|| false);
    // When the pointer last moved. A plain `Cell` so tracking it never
    // re-renders the window; the timer below reads it.
    let last_pointer = use_hook(|| Rc::new(Cell::new(Instant::now())));
    let cursor_hidden = use_signal(|| false);

    // The gamepad pump: one `gilrs` source for the whole window. It posts an
    // action into the active screen's channel; the thread exits when the app's
    // receiver drops. A machine with no pad reports `Unavailable`, never a
    // crash, so the keyboard path stays usable. The returned handle lets the
    // Controls screen push a saved remap into the live worker.
    let pad = use_hook({
        let seed = seed_maps.clone();
        move || {
            let channel = gamepad::GamepadChannel::new();
            let reload = gamepad::spawn(seed, channel.sender());
            (channel, reload)
        }
    });
    let (pad_channel, pad_reload) = pad;
    // The reload handle goes into a signal so the Controls screen can pass it
    // into as many event handlers as it needs; a `MapReload` is `Clone` but not
    // `Copy`, and a `Signal` is `Copy`.
    let map_reload = use_signal(move || pad_reload);
    // The session handle owns navigation and every concern handle a screen
    // reaches through it (ADR-0027). `App` creates the signals above and hands
    // them to the one provider; every screen below shares the same handle, so a
    // new screen reads them from here instead of `App` growing a prop field.
    let mut session = context::provide_session(
        if start.shell.is_console() {
            View::Home
        } else {
            View::Library
        },
        context::SessionHandles {
            play,
            play_events,
            installs,
            install_events,
            downloads: board,
            download_events: events,
            input_maps,
            map_reload,
            pad_access,
            notice,
            history_revision,
        },
    );
    // The pad pump lives with the other drains (`pumps`).
    pumps::gamepad_actions(
        pad_channel.clone(),
        (
            home_actions.clone(),
            library_actions.clone(),
            downloads_actions.clone(),
            stores_actions.clone(),
            detail_actions.clone(),
            controls_actions.clone(),
        ),
        shell_state.clone(),
        pad_active,
        handoff,
        overlay,
        session,
        remap,
        pads,
        batteries,
        library_state,
        notice,
        play,
    );

    // Hide the cursor when the pad is the last input and the pointer has been
    // idle for 2 s. The timer polls a plain `Cell`, so a pointer move never
    // wakes the event loop; a mouse move resets the cell and shows the cursor
    // again. The plan's "auto-hide the cursor after 2 s" lives here.
    {
        let last_pointer = Rc::clone(&last_pointer);
        use_future(move || {
            let last_pointer = Rc::clone(&last_pointer);
            let mut cursor_hidden = cursor_hidden;
            async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let hide =
                        *pad_active.peek() && last_pointer.get().elapsed() > Duration::from_secs(2);
                    if hide != *cursor_hidden.peek() {
                        cursor_hidden.set(hide);
                    }
                }
            }
        });
    }

    // The overlay's wall clock. It only ticks while the menu is open, so a
    // closed overlay never re-renders the window for a minute change.
    {
        use_future(move || {
            let mut clock = clock;
            let overlay = overlay;
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    if overlay.read().is_none() {
                        continue;
                    }
                    let now = crate::overlay::format_clock(crate::overlay::now_local());
                    if *clock.peek() != now {
                        clock.set(now);
                    }
                }
            }
        });
    }

    // The hand-off's return trip. tao tells us when the window actually holds
    // focus; that cancels the pending fallback and clears a badge already on
    // screen (the user has the window back). A focus event when nothing is
    // pending is ignored, so the first focus at startup is a no-op.
    let mut focus_handoff = handoff;
    let focus_deadline = Rc::clone(&handoff_deadline);
    use_wry_event_handler(move |event, _| {
        let dioxus::desktop::tao::event::Event::WindowEvent {
            event: dioxus::desktop::tao::event::WindowEvent::Focused(true),
            ..
        } = event
        else {
            return;
        };
        let pending = {
            let state = focus_handoff.read();
            state.awaiting_focus() || state.badge().is_some()
        };
        if pending {
            focus_handoff.write().focus_granted();
            focus_deadline.set(None);
        }
    });

    // The hand-off's fallback: if the window came back but the compositor never
    // handed focus over within `FOCUS_GRACE`, flag the taskbar entry and show
    // the "press Guide" badge. The deadline is a plain `Cell`, so waiting on it
    // never re-renders the window.
    {
        let deadline = Rc::clone(&handoff_deadline);
        use_future(move || {
            let deadline = Rc::clone(&deadline);
            let mut handoff = handoff;
            async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let Some(at) = deadline.get() else {
                        continue;
                    };
                    if Instant::now() >= at && handoff.read().awaiting_focus() {
                        handoff.write().focus_timeout();
                        deadline.set(None);
                        shell::request_attention();
                        eprintln!("[pitagames] the window did not take focus; showing the badge");
                    }
                }
            }
        });
    }

    // The window key pump lives with the other drains (`pumps`).
    pumps::keyboard(
        (
            home_actions.clone(),
            library_actions.clone(),
            downloads_actions.clone(),
            stores_actions.clone(),
            detail_actions.clone(),
            controls_actions.clone(),
        ),
        shell_state.clone(),
        pad_active,
        handoff,
        input_maps,
        remap,
        overlay,
        session,
        notice,
        play,
        library_state,
    );

    // The Downloads screen's action drain. Its body is inline in `App`, so its
    // focus graph is too; the future only runs while that view is showing.
    pumps::downloads_actions(
        downloads_actions.clone(),
        downloads_focus,
        installs,
        install_events,
        session,
        notice,
    );

    // Drive the install queue: fold each worker report in, flip the row a
    // finished install turned into a real directory, and hand the next queued
    // job to a thread. The queue is serial by design, so this is the only place
    // that starts work.
    pumps::install_events(
        install_channel.clone(),
        installs,
        install_events,
        library_state,
        db_state,
        notice,
    );

    // A finished game writes its session into the row (playtime, last played),
    // appends it to the launch history, and moves the header strip from
    // "playing" to its verdict. The writes are SQLite, so they happen off the
    // event loop. A *start* and an *end* also drive the launch hand-off: the
    // console shell hides its window while a supervised game runs and brings it
    // back when the game ends (`handoff`).
    pumps::play_events(
        play_channel.clone(),
        shell_state.clone(),
        handoff,
        Rc::clone(&handoff_deadline),
        play,
        overlay,
        library_state,
        db_state,
        history_revision,
    );

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
    pumps::bootstrap(
        install_channel.clone(),
        play_channel.clone(),
        proton_state,
        db_state,
        account_state,
        metadata_queue,
        art_state,
        events,
        install_events,
        play_events,
        library_state,
        art_revision,
        board,
    );

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
    let selected_id = session.read().selected();
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
    // Library folders the detail page's Steam-install tracker polls.
    let detail_libraries = snapshot
        .as_ref()
        .map(|library| library.libraries.clone())
        .unwrap_or_default();
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
    // The Downloads focus graph's cell order: the "Clear finished" row first
    // when there is one, then active store jobs, then finished ones. The same
    // order is applied by `handle_downloads_action`.
    let downloads_clear_cell = usize::from(!store_finished.is_empty());
    let downloads_active_start = downloads_clear_cell;
    let downloads_finished_start = downloads_active_start + store_active.len();
    let active_count = downloads_snapshot.active_count() + install_snapshot.active_count();
    let nothing_tracked = downloads_snapshot.rows().is_empty()
        && store_active.is_empty()
        && store_finished.is_empty();
    let has_finished = !finished_downloads.is_empty();
    let current_view = session.read().view();
    let db_handle = (*db_state.read()).clone();
    let owned_summary = snapshot
        .as_ref()
        .map(|library| library.owned.clone())
        .unwrap_or_default();
    // The one running game and the last verdict, for the strip under the nav.
    let play_snapshot: Play = (*play.read()).clone();
    let running_play = play_snapshot.running().cloned();
    let last_play = play_snapshot.last().cloned();
    // The hand-off's fallback: shown when a game ended but the compositor never
    // gave the window focus back. Reading it here makes the badge appear and
    // disappear as the model changes.
    let handoff_badge = handoff.read().badge().map(str::to_string);
    // The open overlay: its rows (already built by the model) and the controller
    // line under them. The clock is a signal so the panel ticks on its own timer
    // rather than the render cadence.
    let overlay_snapshot = overlay.read().clone();
    let overlay_clock = clock();
    let overlay_status = overlay_status(&pads.read(), &batteries.read());

    let subtitle = match snapshot.as_ref() {
        None => t("library.scanning").to_string(),
        Some(library) => {
            let steam_games = library
                .games
                .iter()
                .filter(|entry| entry.is_steam())
                .count();
            let mut subtitle = match &library.steam_root {
                Some(root) => t("library.subtitle_steam")
                    .replace("{count}", &steam_games.to_string())
                    .replace("{root}", &root.display().to_string()),
                None => t("library.no_steam_library").to_string(),
            };
            // The CLI stores' rows come from the cache, not a scan; saying so
            // keeps the count honest when only some of them are synced.
            let other_stores = library.games.len() - steam_games;
            if other_stores > 0 {
                subtitle.push_str(
                    &t("library.subtitle_other_stores")
                        .replace("{count}", &other_stores.to_string()),
                );
            }
            if let Some(total) = library.owned.total {
                subtitle.push_str(
                    &t("library.subtitle_owned").replace("{count}", &total.to_string()),
                );
            }
            if library.metadata_pending {
                subtitle.push_str(t("library.subtitle_fetching"));
            }
            subtitle
        }
    };

    // The shell's CSS. Both branches name the same properties so the style
    // setter cannot keep a console-only value after a switch back.
    //
    // Safe mode adds no global rule: there is no motion to switch off (see
    // `docs/m2.5-design.md`), and the renderer boundary forbids the JavaScript a
    // stylesheet injection would need. `display.reduce_animations` records the
    // preference for a future animated surface; nothing reads it today.
    let mut root_style = shell::root_style(shell_state.shell(), &shell_state.display);
    // A hidden cursor is the whole-window answer to "the pad is driving": the
    // hint row shows the pad's glyphs and the pointer disappears until the next
    // mouse move. `cursor:none` is appended so the base style stays unchanged.
    if *cursor_hidden.read() {
        root_style.push_str("cursor:none;");
    }
    let header_style = shell::header_style(shell_state.shell(), &shell_state.display);
    let shell_label = shell_state.shell().label();
    let shell_hint = if shell_state.shell().is_console() {
        t("shell.hint_console")
    } else {
        t("shell.hint_desktop")
    };

    // The hint row follows the device last used: the pad's labels while it is
    // active, the keyboard's otherwise. Both read the same maps the pumps do,
    // so a remap changes the hints without a second table. For a pad the app now
    // knows the GUID (the worker names every device), so the row reads *that*
    // pad's remap rather than the built-in layout.
    let hint_map = if *pad_active.read() {
        let maps = input_maps.read();
        match pads.read().last().filter(|device| device.is_gamepad) {
            Some(device) => maps.for_device(&device.guid, true),
            None => maps.for_device("", true),
        }
    } else {
        input_maps.read().keyboard()
    };
    let hint_row = if shell_state.shell().is_console() && current_view == View::Detail {
        // The console game page advertises only the actions it exposes, in its
        // own words: Play (Confirm), Overlay (Menu), Back.
        pitagames_input::hints_for(&hint_map, &pitagames_input::GAME_PAGE_HINTS)
    } else {
        pitagames_input::hints(&hint_map)
    };

    // The grid is a child component so scroll-position changes re-render only
    // it, never the whole window (and never clone the library per scroll tick).
    let library_body = rsx! {
        LibraryGrid {
            library: library_state,
            shell: shell_state.shell,
            actions: library_actions,
            art_state,
            art_revision,
            db: db_state,
        }
    };

    let downloads_body = rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:10px;padding:0 32px 40px;",

            if nothing_tracked {
                div {
                    style: "padding:40px 0;color:var(--text-muted);",
                    {t("downloads.nothing")}
                }
            }

            if !store_active.is_empty() || !store_finished.is_empty() {
                div {
                    style: "display:flex;align-items:center;gap:12px;margin-top:8px;",
                    span {
                        style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--text-muted);",
                        {t("downloads.store_installs")}
                    }
                    if !store_finished.is_empty() {
                        {
                            let focused = downloads_focus.index() == Some(0);
                            let ring = focus::ring_style(focused);
                            rsx! {
                                button {
                                    onclick: move |_| {
                                        installs.write().clear_finished();
                                    },
                                    style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);{ring}",
                                    {t("action.clear_finished")}
                                }
                            }
                        }
                    }
                }

                for (position, (index, job)) in store_active.into_iter().enumerate() {
                    {
                        let title = job.title.clone();
                        let store = job.id.store.as_str();
                        let store_id = job.id.store_id.clone();
                        let cancelling = install_snapshot.is_cancelling(&job.id);
                        let state = if cancelling {
                            t("store_installs.cancelling")
                        } else {
                            job.state.label()
                        };
                        let percent = job.progress.percent;
                        let width = percent.unwrap_or(0.0);
                        let percent_label = percent
                            .map(|percent| format!("{percent:.0}%"))
                            .unwrap_or_default();
                        let stage = job.progress.stage.clone().unwrap_or_default();
                        let target = job.target.display().to_string();
                        // A queued job has no process yet; a running one is
                        // stopped through the token its worker registered.
                        let cancellable = job.state == JobState::Queued
                            || (job.state == JobState::Running && !cancelling);
                        let focused = downloads_focus.index()
                            == Some(downloads_active_start + position);
                        let ring = focus::ring_style(focused);

                        rsx! {
                            div {
                                key: "store-active-{index}",
                                style: "display:flex;flex-direction:column;gap:8px;padding:14px;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;{ring}",

                                div {
                                    style: "display:flex;align-items:baseline;gap:12px;",
                                    span {
                                        style: "font-size:var(--text-body);font-weight:600;color:#ffffff;",
                                        "{title}"
                                    }
                                    span {
                                        style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--accent);",
                                        "{state}"
                                    }
                                    span {
                                        style: "margin-left:auto;font-size:var(--text-micro);color:var(--text-muted);",
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
                                    style: "display:flex;align-items:baseline;gap:10px;font-size:var(--text-micro);color:var(--text-muted);",
                                    span { "{store} · {store_id}" }
                                    if !stage.is_empty() {
                                        span {
                                            style: "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                            "{stage}"
                                        }
                                    }
                                    if cancellable {
                                        button {
                                            onclick: move |_| {
                                                installs.write().cancel(index);
                                            },
                                            style: "margin-left:auto;padding:5px 10px;border-radius:7px;border:1px solid #4a2c2c;background:#2a1e1e;color:#e0a0a0;font-size:var(--text-micro);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                                            {t("action.cancel")}
                                        }
                                    }
                                }

                                span {
                                    style: "font-size:var(--text-micro);color:var(--text-disabled);overflow-wrap:anywhere;",
                                    "{target}"
                                }
                            }
                        }
                    }
                }

                for (position, (index, job)) in store_finished.into_iter().enumerate() {
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
                        let focused = downloads_focus.index()
                            == Some(downloads_finished_start + position);
                        let ring = focus::ring_style(focused);

                        rsx! {
                            div {
                                key: "store-done-{index}",
                                style: "display:flex;flex-direction:column;gap:4px;padding:12px 14px;border-radius:12px;background:#1a1a22;border:1px solid #26262f;{ring}",
                                div {
                                    style: "display:flex;align-items:center;gap:12px;",
                                    span {
                                        style: "font-size:var(--text-body);color:var(--text-secondary);",
                                        "{title}"
                                    }
                                    span {
                                        style: "font-size:var(--text-micro);color:var(--text-muted);",
                                        "{meta}"
                                    }
                                    if retry {
                                        button {
                                            onclick: move |_| {
                                                if installs.write().retry(index) {
                                                    launch_start::pump_installs(installs, install_events, notice);
                                                }
                                            },
                                            style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                                            {t("action.retry")}
                                        }
                                    }
                                    button {
                                        onclick: move |_| {
                                            installs.write().remove(index);
                                        },
                                        style: "padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);margin-left:auto;",
                                        {t("action.remove")}
                                    }
                                }
                                if !message.is_empty() {
                                    span {
                                        style: "font-size:var(--text-micro);color:var(--danger);overflow-wrap:anywhere;",
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
                                    style: "font-size:var(--text-body);font-weight:600;color:#ffffff;",
                                    "{title}"
                                }
                                span {
                                    style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--accent);",
                                    "{state}"
                                }
                                span {
                                    style: "margin-left:auto;font-size:var(--text-micro);color:var(--text-muted);",
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
                                style: "display:flex;justify-content:space-between;font-size:var(--text-micro);color:var(--text-muted);",
                                span { {t("downloads.steam_appid").replace("{id}", &app_id)} }
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
                        style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--text-muted);",
                        {t("downloads.finished")}
                    }
                    button {
                        onclick: move |_| {
                            board.write().dismiss_finished();
                        },
                        style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                        {t("action.clear")}
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
                                    style: "font-size:var(--text-body);color:var(--text-secondary);",
                                    "{title}"
                                }
                                span {
                                    style: "font-size:var(--text-micro);color:var(--text-muted);",
                                    {t("downloads.steam_appid_state").replace("{id}", &app_id).replace("{state}", &state)}
                                }
                                button {
                                    onclick: move |_| {
                                        board.write().dismiss(&dismiss_id);
                                    },
                                    style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                                    {t("action.dismiss")}
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    // The pointer handler's captures. The `Rc` is cloned per render; the
    // signals are Copy. A pointer move is desktop input: it shows the cursor
    // and switches the hint row back to the keyboard glyphs.
    let pointer_cell = Rc::clone(&last_pointer);
    let mut pointer_pad_active = pad_active;
    let mut pointer_cursor_hidden = cursor_hidden;
    // A mutable shell clone for the overlay panel's pointer handler, which takes
    // the same `apply_overlay_choice` path the d-pad does (the shell signal is
    // shared, so toggling through the clone updates the window).
    let mut overlay_shell = shell_state.clone();

    rsx! {
        div {
            style: "{root_style}",
            onmousemove: move |_| {
                pointer_cell.set(Instant::now());
                if *pointer_pad_active.peek() {
                    pointer_pad_active.set(false);
                }
                if *pointer_cursor_hidden.peek() {
                    pointer_cursor_hidden.set(false);
                }
            },

            // Safe mode adds no global rule: there is no motion to switch off
            // (see `shell::motion_off_css`), and the renderer boundary forbids
            // the JavaScript a stylesheet injection would need.

            header {
                style: "{header_style}",
                h1 {
                    style: "font-size:var(--text-title);font-weight:600;margin:0;color:#ffffff;",
                    {t("app.name")}
                }
                span {
                    style: "font-size:inherit;color:var(--text-muted);",
                    "{subtitle}"
                }
                span {
                    style: "font-size:inherit;text-transform:uppercase;letter-spacing:0.08em;color:var(--accent);",
                    title: "{shell_hint}",
                    "{shell_label}"
                }
                if start.safe_mode {
                    span {
                        style: "font-size:inherit;text-transform:uppercase;letter-spacing:0.08em;color:var(--warning);border:1px solid #5a4a2a;border-radius:999px;padding:1px 8px;",
                        title: t("app.safe_mode_title"),
                        {t("app.safe_mode")}
                    }
                }
                if shell_state.shell().is_console() {
                    span {
                        style: "font-size:inherit;color:var(--text-muted);",
                        "{shell_hint}"
                    }
                    // The console top bar: a passive destination strip, in the
                    // desktop pill order. `Tab` cycles it; it is not focusable
                    // and has no hover rule, so it is an indicator rather than a
                    // control. No text field is mounted in the console shell.
                    div {
                        style: "display:flex;flex-wrap:wrap;align-items:center;gap:0.5rem;margin-left:auto;",
                        for destination in DESTINATIONS {
                            span {
                                style: shell::destination_strip_style(current_view == destination),
                                "{destination.label()}"
                            }
                        }
                    }
                } else {
                    input {
                        placeholder: t("osk.search_library"),
                        value: "{session.read().filter().query}",
                        oninput: move |event| session.write().filter_mut().query = event.value(),
                        onfocus: move |_| session.write().set_text_focused(true),
                        onblur: move |_| session.write().set_text_focused(false),
                        style: "margin-left:auto;padding:8px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text);font-size:inherit;outline:none;",
                    }
                }
            }

            // The desktop pill nav. The console shell shows the passive strip in
            // the header instead: destinations stay reachable there through `Tab`.
            if !shell_state.shell().is_console() {
                div {
                    style: "display:flex;gap:8px;padding:0 32px 16px;",
                    button {
                        onclick: move |_| {
                            session.write().go(View::Home);
                        },
                        style: nav_style(current_view == View::Home),
                        "{View::Home.label()}"
                    }
                    button {
                        onclick: move |_| {
                            session.write().go(View::Library);
                        },
                        style: nav_style(current_view == View::Library),
                        "{View::Library.label()}"
                    }
                    button {
                        onclick: move |_| {
                            session.write().go(View::Downloads);
                        },
                        style: nav_style(current_view == View::Downloads),
                        "{View::Downloads.label()}"
                        if active_count > 0 {
                            " ({active_count})"
                        }
                    }
                    button {
                        onclick: move |_| {
                            session.write().go(View::History);
                        },
                        style: nav_style(current_view == View::History),
                        "{View::History.label()}"
                    }
                    button {
                        onclick: move |_| {
                            session.write().go(View::Stores);
                        },
                        style: nav_style(current_view == View::Stores),
                        "{View::Stores.label()}"
                    }
                    button {
                        onclick: move |_| {
                            session.write().go(View::Account);
                        },
                        style: nav_style(current_view == View::Account),
                        "{View::Account.label()}"
                    }
                    button {
                        onclick: move |_| {
                            session.write().go(View::Controls);
                        },
                        style: nav_style(current_view == View::Controls),
                        "{View::Controls.label()}"
                    }
                    button {
                        onclick: move |_| {
                            session.write().go(View::Logs);
                        },
                        style: nav_style(current_view == View::Logs),
                        "{View::Logs.label()}"
                    }
                }
            }

            if let Some(running) = running_play.clone() {
                div {
                    style: "display:flex;align-items:center;gap:12px;margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#1e2a24;color:var(--success);font-size:var(--text-caption);",
                    span {
                        style: "font-weight:600;",
                        {t("app.playing").replace("{title}", &running.title)}
                    }
                    if let Some(log) = running.log.clone() {
                        span {
                            style: "color:#6f8f7c;font-family:monospace;font-size:var(--text-micro);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                            "{log.display()}"
                        }
                    }
                    button {
                        onclick: move |_| {
                            if !play.write().stop() {
                                notice.set(t("play.not_started").to_string());
                            }
                        },
                        style: "margin-left:auto;padding:6px 14px;border-radius:8px;border:1px solid #3a5a48;background:#243a2e;color:var(--success);font-size:var(--text-caption);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                        {t("action.stop")}
                    }
                }
            } else if let Some(last) = last_play.clone() {
                div {
                    style: "display:flex;align-items:center;gap:12px;margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-caption);",
                    span {
                        style: "font-weight:600;color:var(--text);",
                        "{last.title}"
                    }
                    span { "{last.summary}" }
                    button {
                        onclick: move |_| {
                            play.write().clear_last();
                        },
                        style: "margin-left:auto;padding:4px 10px;border-radius:8px;border:1px solid #2c2c38;background:#14141c;color:var(--text-muted);font-size:var(--text-micro);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                        {t("action.dismiss")}
                    }
                }
            }

            if !notice.read().is_empty() {
                div {
                    style: "margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#3a1e1e;color:var(--danger);font-size:var(--text-caption);",
                    "{notice}"
                }
            }

            // The hand-off's fallback: the game ended but the compositor did not
            // give the window focus back, so a controller user could otherwise
            // sit in front of a window nothing responds to. Guide (or a click)
            // clears it.
            if let Some(badge) = handoff_badge {
                div {
                    style: "display:flex;align-items:center;gap:12px;margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#262038;color:#c8b8e8;font-size:var(--text-caption);",
                    span {
                        style: "font-weight:600;",
                        "{badge}"
                    }
                    button {
                        onclick: move |_| handoff.write().dismiss(),
                        style: "margin-left:auto;padding:4px 10px;border-radius:8px;border:1px solid #3a3354;background:#1a1626;color:#a898c8;font-size:var(--text-micro);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);",
                        {t("action.dismiss")}
                    }
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
                            actions: detail_actions,
                            hero: hero_url,
                            steam_root: steam_root.clone(),
                            proton: proton_report.clone(),
                            shell: shell_state.shell,
                            libraries: detail_libraries.clone(),
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
                        actions: stores_actions,
                    }
                }
            } else if current_view == View::History {
                div {
                    style: "flex:1;min-height:0;overflow-y:auto;",
                    history_view::HistoryView {
                        db: detail::DbHandle(db_handle.clone()),
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
            } else if current_view == View::Controls {
                div {
                    style: "flex:1;min-height:0;overflow-y:auto;",
                    controls::ControlsView {
                        actions: controls_actions,
                        remap,
                        pads,
                    }
                }
            } else if current_view == View::Logs {
                div {
                    style: "flex:1;min-height:0;overflow-y:auto;",
                    log_view::LogsView { dir: pitagames_core::paths::logs_dir() }
                }
            } else if current_view == View::Home {
                home_view::HomeView {
                    library: library_state,
                    actions: home_actions,
                    art_state,
                    art_revision,
                    db: db_state,
                }
            } else {
                {library_body}
            }

            // The hint row: the device's labels for the actions the shell
            // offers. It sits at the bottom of the window so a couch user can
            // read what the pad does without a pointer. A screen that hides an
            // action (a dialog without Menu) filters `hint_row` before it draws.
            footer {
                style: "display:flex;flex-wrap:wrap;gap:1.25rem;align-items:center;padding:0.5rem 2rem;border-top:1px solid #26262f;color:var(--text-muted);font-size:var(--text-body);",
                for hint in hint_row.iter() {
                    span {
                        style: "display:inline-flex;align-items:center;gap:0.4rem;",
                        span {
                            style: "min-width:1.4rem;padding:0.05rem 0.35rem;border:1px solid #3a3a4a;border-radius:0.3rem;background:#1e1e28;color:#e8e8f0;text-align:center;font-weight:600;",
                            "{hint.label}"
                        }
                        span { "{hint.name}" }
                    }
                }
            }

            // The console overlay is a modal, so it is the last child and a
            // fixed full-window backdrop: the screen beneath keeps its focus and
            // scroll, and a pointer click takes the same `apply_overlay_choice`
            // path the d-pad does.
            if let Some(model) = overlay_snapshot {
                {
                    let entries = model.entries().to_vec();
                    let focused = model.focused();
                    rsx! {
                        overlay_view::OverlayView {
                            entries,
                            focused,
                            status: overlay_status.clone(),
                            clock: overlay_clock.clone(),
                            on_choose: move |choice: OverlayChoice| {
                                apply_overlay_choice(
                                    choice,
                                    overlay,
                                    &mut overlay_shell,
                                    session,
                                    notice,
                                    play,
                                );
                            },
                            on_close: move |_| overlay.set(None),
                        }
                    }
                }
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
    /// Which shell the window is in, so the grid picks its per-shell tile floor
    /// and the console tile carries its store/status line.
    pub shell: Signal<Shell>,
    /// The app's one input pump posts this screen's actions here.
    pub actions: focus::ActionChannel,
    pub art_state: Signal<Option<std::sync::Arc<art::ArtContext>>>,
    pub art_revision: Signal<u64>,
    pub db: Signal<Option<pitagames_db::Db>>,
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
    let shell = props.shell;
    let art_state = props.art_state;
    let art_revision = props.art_revision;
    // The session handle owns navigation and every concern handle (ADR-0027):
    // the grid reads the filter, the Downloads board, the install queue, the
    // play slot and the notice line through the one seam instead of seven props.
    let mut session = crate::context::use_session();
    let board = session.read().downloads();
    let notice = session.read().notice();
    let events = session.read().download_events();
    let db = props.db;
    let installs = session.read().installs();
    let install_events = session.read().install_events();
    let play = session.read().play();
    let play_events = session.read().play_events();

    let mut scroll_top = use_signal(|| 0.0f64);
    let mut viewport_width = use_signal(|| 0.0f64);
    let mut viewport_height = use_signal(|| 0.0f64);

    // Reading the revision makes a capsule that just finished downloading
    // repaint without re-rendering the whole window.
    let _ = art_revision();

    // Focus lives here, not in the DOM: cards scroll in and out of existence
    // with the window, but keyboard/controller focus must be able to walk past
    // the drawn edge and scroll it into view. The graph is rebuilt from the
    // logical list on every move (`focus::ScreenFocus`).
    let screen_focus = focus::use_focus();
    // The on-screen keyboard, opened by the pad's `Search` button. The sheet
    // owns every editing rule; this screen only seeds it from the filter and
    // applies the committed text on Done.
    let mut osk_open = use_signal(|| false);
    let mut osk = use_signal(pitagames_input::OnScreenKeyboard::new);
    // The scroll container's mounted handle. Scrolling is imperative; the
    // handle is kept here so the action drain can move the viewport.
    let scroller = use_hook(|| Rc::new(RefCell::new(None::<Rc<MountedData>>)));
    let action_scroller = Rc::clone(&scroller);

    // The app's one input pump posts this screen's actions here. The grid owns
    // its own focus and scroll geometry, so it applies the action itself; the
    // `view` check keeps a late action from a previous screen out.
    let actions = props.actions;
    use_future(move || {
        let channel = actions.clone();
        // Cloned outside the async block: the outer closure is `FnMut`, so it
        // must not move the `Rc` the future would consume.
        let scroller = Rc::clone(&action_scroller);
        async move {
            while let Some(action) = channel.next().await {
                if session.read().view() != View::Library {
                    continue;
                }
                // While the sheet is open it owns the keys: the typed text is
                // the screen's, so a direction walks the key grid instead of
                // the cards, and `Back` cancels the edit before it leaves.
                if osk_open() {
                    let press = osk.write().handle(action);
                    if press == pitagames_input::OskPress::Submit {
                        let text = osk.read().text().to_string();
                        commit_search(session, screen_focus, osk_open, text);
                    } else if press == pitagames_input::OskPress::Cancel {
                        osk_open.set(false);
                    }
                    continue;
                }
                // `Search` is the pad's text-entry button: seed the keyboard
                // with the current filter so editing never starts from nothing.
                if action == pitagames_input::Action::Search {
                    osk.set(pitagames_input::OnScreenKeyboard::with_text(
                        session.read().filter().query.clone(),
                    ));
                    osk_open.set(true);
                    continue;
                }
                perform_action(
                    action,
                    library,
                    shell,
                    session,
                    screen_focus,
                    viewport_width,
                    viewport_height,
                    scroll_top,
                    Rc::clone(&scroller),
                );
            }
        }
    });

    let library_guard = library.read();
    let Some(state) = library_guard.as_ref() else {
        return rsx! {
            div {
                style: "padding:32px;color:var(--text-muted);",
                {t("library.scanning_steam")}
            }
        };
    };

    let matches: Vec<&library::GameEntry> = session
        .read()
        .filter()
        .apply(state)
        .into_iter()
        .map(|index| &state.games[index])
        .collect();
    let console = shell().is_console();
    let geometry = grid::GridGeometry::for_shell(shell(), viewport_width());
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
    let filter_snapshot = session.read().filter().clone();
    // The chip row and the tiles share one ragged graph; the chips sit above
    // the scroll area, so their width is the scroll container's minus the same
    // padding the grid uses.
    let chip_row = filter::ChipRow::build(state, viewport_width() - 2.0 * grid::H_PADDING);
    let chip_columns = chip_row.columns();
    let focused_id = screen_focus.index();
    let empty_message = filter::empty_message(&filter_snapshot);

    // Bound as locals so the RSX format strings only interpolate identifiers.
    let columns = geometry.columns;
    let row_gap = grid::ROW_GAP;
    let column_gap = grid::COLUMN_GAP;
    let h_padding = grid::H_PADDING;
    let card_height = geometry.art_height() + geometry.chrome;
    let total_height = window.total_height;
    let offset_y = window.offset_y;

    rsx! {
        if warning_count > 0 {
            div {
                style: "margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#2a2420;color:#e0b090;font-size:var(--text-caption);",
                "{warning_count} scan warning(s) — run `pitagames-cli scan --json` for detail."
            }
        }

        if metadata_warning_count > 0 {
            div {
                style: "margin:0 32px 12px;padding:10px 14px;border-radius:8px;background:#1e2430;color:#9ab0d8;font-size:var(--text-caption);display:flex;flex-direction:column;gap:4px;",
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
                style: "flex:1;padding:48px 32px;color:var(--text-muted);",
                "No games yet. PitaGames reads Steam's own libraries; open Stores and choose \"Sync libraries\" to fetch your Epic, Amazon and GOG games."
            }
        } else {
            // The chip row sits above the grid in both shells, so the two shells
            // cannot drift. It wraps onto more lines on a narrow window.
            div {
                style: "display:flex;flex-direction:column;flex:1;min-height:0;",

                div {
                    style: "display:grid;grid-template-columns:repeat({chip_columns},minmax(0,1fr));gap:8px;padding:0 32px 12px;",
                    for (index, chip) in chip_row.chips().iter().enumerate() {
                        {
                            let chip = *chip;
                            let label = chip.label(&filter_snapshot);
                            let active = chip.is_active(&filter_snapshot);
                            let ring = focus::ring_style(focused_id == Some(index));
                            let style = chip_style(active);
                            let mut session = session;
                            rsx! {
                                button {
                                    key: "chip-{index}",
                                    onclick: move |_| {
                                        chip.toggle(session.write().filter_mut());
                                    },
                                    style: "{style}{ring}",
                                    "{label}"
                                }
                            }
                        }
                    }
                }

                if window.is_empty() {
                    div {
                        style: "flex:1;padding:48px 32px;color:var(--text-muted);",
                        "{empty_message}"
                    }
                } else {
                    // The scroll viewport fills the rest of the wrapper; the page
                    // itself never scrolls, so `onscroll` reports this offset.
                    div {
                        style: "flex:1;min-height:0;overflow-y:auto;overflow-x:hidden;",
                onmounted: move |event| {
                    // Keep the handle: the focus pump scrolls the container
                    // imperatively so the focused row is drawn next render.
                    *scroller.borrow_mut() = Some(event.data());
                },
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

                        for (offset, entry) in visible.iter().enumerate() {
                            {
                                // Every closure below needs owned data: rsx
                                // children outlive this iteration.
                                let card_index = window.item_start + offset;
                                let focus_id = chip_row.tile_id(card_index).get() as usize;
                                let focused = screen_focus.index() == Some(focus_id);
                                let ring = focus::ring_style(focused);
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
                                // The console tile names its store and its
                                // install/update state explicitly; the desktop
                                // card keeps them inside the meta line.
                                let store_label = crate::home::store_label(entry.store());
                                let install_status = entry.install_status();
                                let can_play = entry.can_play();
                                // Steam launches through its own client; every
                                // other store goes through the umu hand-off,
                                // which needs the listing's executable.
                                let show_play = entry.game.installed && entry.play_is_wired();
                                let playing = play.read().label(&game_id);
                                let offers_install = entry.offers_install();
                                let install_label = entry.install_action();
                                let card_label = entry.card_label();
                                // A queued/running CLI-store install owns the
                                // card's action slot, so a second click cannot
                                // queue a duplicate.
                                let queued = installs_snapshot.active_label(&game_id);
                                let steam_install = entry.is_steam();
                                let tracked = downloads_snapshot
                                    .get(&app_id)
                                    .is_some_and(|row| !row.state.is_finished());

                                let play_db = db_handle.clone();
                                let play_game = entry.game.clone();
                                let install_game = game_id.clone();
                                let install_title = title.clone();
                                let card_key = format!("{}-{app_id}", entry.store().as_str());
                                // The Steam install path needs the downloads
                                // channel; the CLI stores need the install queue's.
                                let install_enabled = if steam_install {
                                    events_sender.is_some()
                                } else {
                                    install_ready
                                };
                                let install_libraries = libraries.clone();
                                let open_id = game_id.clone();

                                rsx! {
                                    div {
                                        key: "{card_key}",
                                        onclick: move |_| {
                                            screen_focus.set(Some(focus_id));
                                            session.write().open_detail(open_id.clone(), View::Library);
                                        },
                                        style: "display:flex;flex-direction:column;gap:6px;padding:14px;box-sizing:border-box;height:{card_height}px;overflow:hidden;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);{ring}",

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
                                            style: "font-size:var(--text-body);font-weight:600;color:#ffffff;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                            title: "{title}",
                                            "{title}"
                                        }

                                        if console {
                                            div {
                                                style: "display:flex;align-items:baseline;gap:10px;font-size:var(--text-micro);",
                                                span {
                                                    style: "color:var(--text-secondary);font-weight:600;",
                                                    "{store_label}"
                                                }
                                                span {
                                                    style: "color:var(--warning);",
                                                    "{install_status}"
                                                }
                                            }
                                        }

                                        if !status.is_empty() {
                                            div {
                                                style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--warning);",
                                                "{status}"
                                            }
                                        }

                                        div {
                                            style: "font-size:var(--text-micro);color:var(--text-muted);",
                                            "{meta}"
                                        }

                                        div {
                                            style: "display:flex;gap:8px;margin-top:auto;padding-top:8px;",

                                            if let Some(label) = playing {
                                                span {
                                                    style: "flex:1;align-self:center;font-size:var(--text-micro);color:var(--success);",
                                                    "{label}"
                                                }
                                            } else if show_play {
                                                button {
                                                    disabled: !can_play,
                                                    onclick: move |event| {
                                                        // The card itself opens the detail
                                                        // page; the buttons only do their own
                                                        // action.
                                                        event.stop_propagation();
                                                        launch_start::start_play(
                                                            &play_game,
                                                            playtime,
                                                            play,
                                                            play_events,
                                                            notice,
                                                            play_db.clone(),
                                                        );
                                                    },
                                                    style: button_style(can_play, "#2e3a56", "#ffffff"),
                                                    {t("action.play")}
                                                }
                                            }

                                            if let Some(label) = queued {
                                                span {
                                                    style: "flex:1;align-self:center;font-size:var(--text-micro);color:var(--accent);",
                                                    "{label}"
                                                }
                                            } else if offers_install {
                                                button {
                                                    disabled: !install_enabled,
                                                    onclick: move |event| {
                                                        // The card itself opens the detail
                                                        // page; the buttons only do their own
                                                        // action.
                                                        event.stop_propagation();
                                                        launch_start::start_install(
                                                            &install_game,
                                                            &install_title,
                                                            installs,
                                                            install_events,
                                                            events,
                                                            board,
                                                            &install_libraries,
                                                            notice,
                                                            session,
                                                        );
                                                    },
                                                    style: button_style(install_enabled, "#243024", "#d8e8d0"),
                                                    "{install_label}"
                                                }
                                            } else if tracked {
                                                span {
                                                    style: "flex:1;align-self:center;font-size:var(--text-micro);color:var(--accent);",
                                                    {t("library.in_downloads")}
                                                }
                                            } else if !show_play {
                                                span {
                                                    style: "flex:1;align-self:center;font-size:var(--text-micro);color:var(--text-muted);",
                                                    "{card_label}"
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

        // The on-screen keyboard is an overlay, so it sits outside the scroll
        // container: the grid behind it never scrolls while typing.
        if osk_open() {
            osk::OskSheet {
                title: t("osk.search_library").to_string(),
                keyboard: osk,
                on_done: move |text| commit_search(session, screen_focus, osk_open, text),
                on_cancel: move |_| osk_open.set(false),
            }
        }
    }
}

/// Apply the on-screen keyboard's text to the library filter.
///
/// Both the action drain and the sheet's pointer path commit through here, so a
/// controller and a mouse cannot leave the filter and the focus in different
/// states. The focused card is cleared because the new filter may not contain
/// it; the next navigation re-enters at the first match.
fn commit_search(
    mut session: Signal<crate::session::Session>,
    screen_focus: focus::ScreenFocus,
    mut osk_open: Signal<bool>,
    text: String,
) {
    session.write().search(text);
    screen_focus.set(None);
    osk_open.set(false);
}

/// Apply one action to the library's focus graph.
///
/// Both the keyboard pump and the gamepad pump call this, so `Confirm` opens the
/// same card and a direction resolves through the same [`pitagames_input::FocusGraph`]
/// regardless of which device produced the action. It is a free function rather
/// than a shared closure because its two callers are a window event handler and
/// an async future, which cannot capture one closure without wrapping it in an
/// `Rc`.
#[allow(clippy::too_many_arguments)]
fn perform_action(
    action: pitagames_input::Action,
    library: Signal<Option<Library>>,
    shell: Signal<Shell>,
    mut session: Signal<crate::session::Session>,
    screen_focus: focus::ScreenFocus,
    viewport_width: Signal<f64>,
    viewport_height: Signal<f64>,
    mut scroll_top: Signal<f64>,
    scroller: Rc<RefCell<Option<Rc<MountedData>>>>,
) {
    // Back on the library clears a narrowing filter before it leaves; with
    // nothing left to clear the console shell returns to its couch home (the
    // desktop grid is the shell's root, so it stays put).
    if action == pitagames_input::Action::Back {
        let back = filter::back_action(session.read().filter());
        match back {
            filter::BackAction::ClearFilters => session.write().clear_filter(),
            filter::BackAction::Leave => {
                if shell().is_console() {
                    session.write().set_view(View::Home);
                }
            }
        }
        return;
    }

    // The chip row and the tiles share one ragged graph: the chips are row 0 and
    // the tiles follow, so `Up` from the top tile row reaches the chips and the
    // row wraps when it overflows.
    let (chip_row, indices) = {
        let guard = library.read();
        let Some(state) = guard.as_ref() else {
            return;
        };
        // With no games at all the screen draws its empty message and no chips,
        // so there is nothing for a direction or a press to reach.
        if state.games.is_empty() {
            return;
        }
        let available = viewport_width() - 2.0 * grid::H_PADDING;
        let chip_row = filter::ChipRow::build(state, available);
        let indices = session.read().filter().apply(state);
        (chip_row, indices)
    };
    let columns = grid::GridGeometry::for_shell(shell(), viewport_width()).columns;
    let graph = chip_row.graph(indices.len(), columns);
    if graph.is_empty() {
        return;
    }

    if action.is_navigation() {
        let Some(id) = screen_focus.move_in(&graph, action) else {
            return;
        };
        // A chip is pinned above the scroll area; only a tile scrolls.
        let Some(tile) = chip_row.tile_index(id) else {
            return;
        };
        let geometry = grid::GridGeometry::for_shell(shell(), viewport_width());
        let target = grid::scroll_into_view(
            scroll_top(),
            viewport_height(),
            geometry,
            geometry.row_of(tile),
            grid::ROW_GAP,
        );
        if (target - scroll_top()).abs() > 0.5 {
            // Update the window model now so the next render draws the focused
            // row; the DOM scroll keeps the container in sync.
            scroll_top.set(target);
            let future = scroller
                .borrow()
                .as_ref()
                .map(|data| data.scroll(PixelsVector2D::new(0.0, target), ScrollBehavior::Instant));
            if let Some(future) = future {
                spawn(async move {
                    let _ = future.await;
                });
            }
        }
        return;
    }

    if action == pitagames_input::Action::Confirm {
        // A focused chip toggles its filter; a focused tile opens its page.
        if let Some(id) = screen_focus.id()
            && let Some(chip) = chip_row.chip(id)
        {
            chip.toggle(session.write().filter_mut());
            return;
        }
        let open = screen_focus
            .id()
            .and_then(|id| chip_row.tile_index(id))
            .filter(|index| *index < indices.len())
            .and_then(|index| {
                let guard = library.read();
                guard
                    .as_ref()
                    .map(|state| state.games[indices[index]].game.id.clone())
            });
        if let Some(id) = open {
            session.write().open_detail(id, View::Library);
        }
    }
}

/// `Guide`'s app-level job: dismiss the hand-off's badge, else toggle the shell.
///
/// While the "game ended — press Guide" badge is on screen, Guide means "take me
/// back", not "switch shells", so the first press clears the badge instead.
pub(crate) fn guide_action(
    shell: &mut shell::ShellState,
    session: Signal<crate::session::Session>,
    mut handoff: Signal<Handoff>,
) {
    let showing = handoff.read().badge().is_some();
    if showing {
        handoff.write().dismiss();
        return;
    }
    toggle_shell(shell, session);
}

/// `Tab`'s app-level job: cycle the current view through the destinations.
///
/// Handled at the app level, not by a screen, so it works wherever the player is
/// — the console strip and the desktop pills read the one ring in
/// [`navigation::DESTINATIONS`], so a `Tab` step can never land on a screen the
/// strip does not show.
pub(crate) fn tab_action(mut session: Signal<crate::session::Session>, forward: bool) {
    session.write().tab(forward);
}

/// Toggle the shell and land the player on the shell's home screen.
///
/// Entering the console shell shows the couch home; leaving it returns to the
/// library grid. A screen other than the home is left alone when going back to
/// the desktop, so a pointer user who opened Settings and pressed `F11` twice
/// is not thrown out of it.
fn toggle_shell(shell: &mut shell::ShellState, mut session: Signal<crate::session::Session>) {
    shell.toggle();
    if shell.shell().is_console() {
        session.write().set_view(View::Home);
    } else if session.read().view() == View::Home {
        session.write().set_view(View::Library);
    }
}

/// The game the overlay is about: the running one, else the open detail page's.
fn current_game_id(play: Signal<Play>, session: Signal<crate::session::Session>) -> Option<GameId> {
    play.read()
        .running()
        .map(|running| running.id.clone())
        .or(session.read().selected())
}

/// Build the overlay's context from the running (or opened) game.
fn overlay_context(
    shell: pitagames_core::ui::Shell,
    library: Signal<Option<Library>>,
    session: Signal<crate::session::Session>,
    play: Signal<Play>,
) -> OverlayContext {
    let game = current_game_id(play, session).and_then(|id| {
        let guard = library.read();
        let entry = guard
            .as_ref()?
            .games
            .iter()
            .find(|entry| entry.game.id == id)?;
        Some(OverlayGame {
            id: entry.game.id.clone(),
            title: entry.title().to_string(),
            steam: entry.is_steam(),
        })
    });
    OverlayContext {
        console: shell.is_console(),
        game,
    }
}

/// Open the console overlay, built for the current game and shell.
///
/// `Menu` opens it in either shell, so a desktop pointer that never touches a
/// pad still has a place for the shell switch and the (v1.1) power rows.
pub(crate) fn open_overlay(
    mut overlay: Signal<Option<Overlay>>,
    shell: &shell::ShellState,
    library: Signal<Option<Library>>,
    session: Signal<crate::session::Session>,
    play: Signal<Play>,
) {
    let context = overlay_context(shell.shell(), library, session, play);
    overlay.set(Some(Overlay::build(&context)));
}

/// Apply one action to the open overlay.
///
/// The overlay is modal, so this owns every action while it is open: navigation
/// moves its ring, `Confirm` runs the focused row, and `Back`/`Menu`/`Guide`
/// close it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_overlay(
    action: pitagames_input::Action,
    mut overlay: Signal<Option<Overlay>>,
    shell: &mut shell::ShellState,
    session: Signal<crate::session::Session>,
    notice: Signal<String>,
    play: Signal<Play>,
) {
    use pitagames_input::Action as A;

    // A menu is a toggle: any of the shell keys closes it.
    if matches!(action, A::Back | A::Menu | A::Guide) {
        overlay.set(None);
        return;
    }

    let moved = {
        let mut guard = overlay.write();
        guard.as_mut().is_some_and(|model| model.move_focus(action))
    };
    if moved {
        return;
    }
    if action != A::Confirm {
        return;
    }

    let choice = overlay
        .read()
        .as_ref()
        .and_then(|model| model.focused_choice());
    let Some(choice) = choice else {
        // A disabled row (a power placeholder): keep the menu open.
        return;
    };
    apply_overlay_choice(choice, overlay, shell, session, notice, play);
}

/// Perform an overlay choice, whether a controller or a pointer picked it.
#[allow(clippy::too_many_arguments)]
fn apply_overlay_choice(
    choice: OverlayChoice,
    mut overlay: Signal<Option<Overlay>>,
    shell: &mut shell::ShellState,
    mut session: Signal<crate::session::Session>,
    mut notice: Signal<String>,
    play: Signal<Play>,
) {
    // Every choice closes the menu first, so the screen it lands on is visible.
    overlay.set(None);
    match choice {
        OverlayChoice::Close => {}
        OverlayChoice::Details => {
            if let Some(id) = current_game_id(play, session) {
                // Back returns to whatever the overlay was opened over.
                let from = session.read().view();
                session.write().open_detail(id, from);
            }
        }
        OverlayChoice::StorePage => {
            if let Some(id) = current_game_id(play, session)
                && id.store == Store::Steam
                && let Err(error) = library::spawn_store_page(&id.store_id)
            {
                notice.set(t("app.store_page_error").replace("{error}", &error.to_string()));
            }
        }
        OverlayChoice::ToggleShell => toggle_shell(shell, session),
    }
}

/// The overlay's controller line: the last pad's name and battery, or keyboard.
///
/// `gilrs` has no battery event, so the worker reports each change and this reads
/// the latest; a pad that has never reported power shows its name alone.
fn overlay_status(
    pads: &[pitagames_input::Device],
    batteries: &std::collections::BTreeMap<pitagames_input::DeviceId, pitagames_input::Battery>,
) -> String {
    let Some(device) = pads.iter().rev().find(|device| device.is_gamepad) else {
        return t("overlay.keyboard").to_string();
    };
    let battery = batteries.get(&device.id).copied().and_then(|state| {
        state
            .percent()
            .map(|percent| (percent, state.is_charging()))
    });
    match battery {
        Some((percent, charging)) => {
            let suffix = if charging { t("overlay.charging") } else { "" };
            format!("{} · {percent}%{suffix}", device.name)
        }
        None => device.name.clone(),
    }
}

/// Route one action from the app's one input pump to the active screen.
///
/// The pump lives in `App`; each screen owns its own `ScreenFocus` and applies
/// the action in its own future. A screen without a graph (History, Account,
/// Logs) only gets `Back`, which returns to the library. The channel list is
/// one parameter per screen, which keeps the match readable; the call sites
/// pass the same tuple shape the two pumps own.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_action(
    action: pitagames_input::Action,
    mut session: Signal<crate::session::Session>,
    home: &focus::ActionChannel,
    library: &focus::ActionChannel,
    downloads: &focus::ActionChannel,
    stores: &focus::ActionChannel,
    detail: &focus::ActionChannel,
    controls: &focus::ActionChannel,
) {
    // Read the current view first: the match scrutinee's guard would live for
    // the whole match and clash with the `set_view` in the fallback arm.
    let current = session.read().view();
    let channel = match current {
        View::Home => home,
        View::Library => library,
        View::Downloads => downloads,
        View::Stores => stores,
        View::Detail => detail,
        View::Controls => controls,
        View::History | View::Account | View::Logs => {
            // These screens have no graph yet; Back still leaves them so the
            // keyboard and a pad cannot trap the user.
            if action == pitagames_input::Action::Back {
                session.write().set_view(View::Library);
            }
            return;
        }
    };
    let _ = channel.send(action);
}

/// Filter chip style. An active chip reads like the active nav pill; an
/// inactive one is the plain card. The hit floor keeps it pad-reachable.
fn chip_style(active: bool) -> String {
    shell::pill_style(
        active,
        "calc(5px * var(--ui-scale, 1)) calc(12px * var(--ui-scale, 1))",
        "cursor:pointer;text-align:center;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
        shell::PILL_INACTIVE,
    )
}

fn nav_style(active: bool) -> String {
    // The type is the shell's caption token, so the console shell's navigation
    // reads from a couch without a second navigation rule.
    shell::pill_style(
        active,
        "calc(6px * var(--ui-scale, 1)) calc(14px * var(--ui-scale, 1))",
        "cursor:pointer;",
        shell::PILL_INACTIVE,
    )
}

/// Card action button. A disabled action must look disabled: the grid relies on
/// it to show "Steam is busy" without a separate widget.
fn button_style(enabled: bool, background: &str, color: &str) -> String {
    if enabled {
        format!(
            "flex:1;padding:8px 0;border-radius:8px;border:1px solid #3a3a4c;background:{background};color:{color};font-size:var(--text-body);cursor:pointer;{}",
            shell::HIT_FLOOR
        )
    } else {
        "flex:1;padding:8px 0;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:var(--text-disabled);font-size:var(--text-body);cursor:default;"
            .to_string()
    }
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
pub(crate) async fn metadata_worker(
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
pub(crate) async fn refresh_owned(
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
pub(crate) async fn warm_art(
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
