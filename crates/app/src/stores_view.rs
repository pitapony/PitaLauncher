//! The Stores screen: one row per CLI store, and what each needs.
//!
//! The rows come from `stores::status` (framework-free): whether the store's CLI
//! is on `PATH`, its version, the signed-in account and, on request, the game
//! counts. The probe runs each CLI, so it happens on its own thread and lands
//! back here over a channel; the screen renders whatever the last probe found.
//!
//! A signed-out store can be signed in here: the row's button starts the store's
//! own CLI over [`crate::auth`], which streams what the CLI says into the
//! terminal panel and lets the person paste the one line it asks for. Nothing on
//! this screen types an account password or holds a token — the CLI does that,
//! and this is only the PTY in front of it.

use std::rc::Rc;
use std::thread;

use dioxus::prelude::*;
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_core::Store;
use pitagames_stores::cli::CliOptions;
use pitagames_stores::status::{self, RowState, SessionState, StoreStatusReport};
use pitagames_stores::sync::{Backends, StoreLibrary};

use crate::auth::{self, AuthEvent, AuthInputs, AuthStatus, AuthTerminal};
use crate::copy::CopyButton;
use crate::library::Library;
use crate::store_sync;

/// A finished probe: the rows and the `list` flag they were fetched with.
type ProbeReply = (bool, Vec<StoreStatusReport>);

/// A finished library sync: what each store's CLI reported.
type SyncReply = Vec<(Store, StoreLibrary)>;

/// The render-side end of a worker channel.
///
/// Cloning shares one receiver. `use_hook` needs a `Clone` value and an
/// `UnboundedReceiver` is not one, so the receiver sits behind an async mutex,
/// exactly like the activation channel.
struct Channel<T> {
    sender: UnboundedSender<T>,
    receiver: Rc<AsyncMutex<Option<UnboundedReceiver<T>>>>,
}

impl<T> Clone for Channel<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            receiver: Rc::clone(&self.receiver),
        }
    }
}

impl<T> Channel<T> {
    fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver: Rc::new(AsyncMutex::new(Some(receiver))),
        }
    }

    /// Wait for the next result; `None` once the channel closes.
    async fn next(&self) -> Option<T> {
        let mut slot = self.receiver.lock().await;
        let receiver = slot.as_mut()?;
        receiver.next().await
    }
}

/// The CLI options the status probe and the library sync use: transcripts under
/// the data dir, ordinary timeouts.
fn cli_options() -> CliOptions {
    CliOptions {
        log_dir: pitagames_core::paths::logs_dir(),
        ..CliOptions::default()
    }
}

/// The CLI options a sign-in uses: no idle timeout, because the person is in a
/// browser, and the driver's auth hard cap.
fn auth_options() -> CliOptions {
    CliOptions {
        log_dir: pitagames_core::paths::logs_dir(),
        ..CliOptions::interactive()
    }
}

/// Probe every store on a worker thread. A CLI run can take seconds, so it must
/// never run on the render thread.
fn start_probe(sender: UnboundedSender<ProbeReply>, list: bool) {
    let spawned = thread::Builder::new()
        .name("pitagames-store-status".to_string())
        .spawn(move || {
            let reports = status::stores(&cli_options(), list);
            // A closed receiver only means the screen is gone.
            let _ = sender.unbounded_send((list, reports));
        });

    if let Err(error) = spawned {
        eprintln!("[pitagames] store status probe could not start: {error}");
    }
}

/// Fetch every store's library on a worker thread.
///
/// The merge and the write are async SQLite work, so they stay in the drain
/// future; this half only runs the CLIs, which block.
fn start_sync(sender: UnboundedSender<SyncReply>) {
    let spawned = thread::Builder::new()
        .name("pitagames-store-sync".to_string())
        .spawn(move || {
            let backends = Backends::new();
            let fetched = store_sync::fetch(&backends, &cli_options(), &status::CLI_STORES);
            let _ = sender.unbounded_send(fetched);
        });

    if let Err(error) = spawned {
        eprintln!("[pitagames] store sync could not start: {error}");
    }
}

/// Props for the Stores screen.
#[derive(Props, Clone, PartialEq)]
pub struct StoresViewProps {
    /// The grid's library, so a finished sync can add its rows without a restart.
    pub library: Signal<Option<Library>>,
    /// Where the synced rows are written.
    pub db: Signal<Option<pitagames_db::Db>>,
}

/// The Stores screen.
#[allow(non_snake_case)]
pub fn StoresView(props: StoresViewProps) -> Element {
    let mut rows = use_signal(|| None::<Vec<StoreStatusReport>>);
    let mut pending = use_signal(|| true);
    let mut counts = use_signal(|| false);
    let mut syncing = use_signal(|| false);
    let mut sync_message = use_signal(String::new);
    let mut sync_problems = use_signal(Vec::<String>::new);
    // The one sign-in terminal: what the running CLI said, and the handle that
    // types back into it.
    let mut auth_state = use_signal(|| None::<AuthTerminal>);
    let mut auth_inputs = use_signal(|| None::<AuthInputs>);
    let mut auth_text = use_signal(String::new);

    let probes = use_hook(Channel::<ProbeReply>::new);
    let syncs = use_hook(Channel::<SyncReply>::new);
    let auth_channel = use_hook(auth::AuthChannel::new);
    let auth_events = use_signal(|| Some(auth_channel.sender()));
    let db_state = props.db;
    let mut library_state = props.library;

    // The drain future starts the first probe, then renders every reply.
    let drain_probes = probes.clone();
    use_future(move || {
        let drain = drain_probes.clone();
        let starter = drain_probes.clone();
        async move {
            start_probe(starter.sender.clone(), false);
            while let Some((list, reports)) = drain.next().await {
                rows.set(Some(reports));
                counts.set(list);
                pending.set(false);
            }
        }
    });

    // The sync drain: the CLIs ran on a thread, so this half merges the answer
    // over the cache, writes it and folds the rows into the grid.
    let drain_syncs = syncs.clone();
    use_future(move || {
        let drain = drain_syncs.clone();
        async move {
            while let Some(fetched) = drain.next().await {
                let Some(db) = (*db_state.read()).clone() else {
                    syncing.set(false);
                    sync_message
                        .set("The library cannot be saved until the database opens.".to_string());
                    continue;
                };

                let (synced, problems) = store_sync::apply_all(&db, &fetched).await;

                let mut total_rows = 0;
                let mut applied = 0;
                {
                    let mut guard = library_state.write();
                    for store in &synced {
                        total_rows += store.report.rows;
                        if store.report.applied {
                            applied += 1;
                        }
                        if let Some(library) = guard.as_mut() {
                            library.apply_store_rows(&store.rows);
                        }
                    }
                }

                let mut message = format!(
                    "{total_rows} game(s) from {applied} of {} store(s).",
                    status::CLI_STORES.len()
                );
                if !problems.is_empty() {
                    message.push_str(&format!(" {} warning(s).", problems.len()));
                }
                sync_message.set(message);
                sync_problems.set(problems);
                syncing.set(false);
            }
        }
    });

    // The sign-in drain: every line lands in the terminal, and a stored session
    // re-probes the row so the badge and the counts catch up without a click.
    let drain_auth = auth_channel.clone();
    let reprobe = probes.clone();
    use_future(move || {
        let drain = drain_auth.clone();
        let reprobe = reprobe.clone();
        async move {
            while let Some(event) = drain.next().await {
                let finished = event.is_finished();
                let stored_session = {
                    let mut guard = auth_state.write();
                    match guard.as_mut() {
                        Some(terminal) => terminal.apply(&event),
                        None => false,
                    }
                };

                if stored_session {
                    pending.set(true);
                    start_probe(reprobe.sender.clone(), false);
                }
                if finished {
                    // Drops the input handle, which stops the run's typing
                    // thread; the CLI has already exited.
                    auth_inputs.set(None);
                    auth_text.set(String::new());
                }
            }
        }
    });

    let refresh_probes = probes.clone();
    let counts_probes = probes.clone();
    let sync_starter = syncs.clone();

    let loaded = rows();
    let is_pending = pending();
    let show_counts = counts();
    let is_syncing = syncing();
    let message = sync_message();
    let problems = sync_problems();
    let terminal = auth_state.read().clone();
    let typed = auth_text();
    let awaiting_input = terminal
        .as_ref()
        .is_some_and(|state| state.prompt().is_some());
    let placeholder = if awaiting_input {
        "Paste here and press Enter"
    } else {
        "Waiting for the browser…"
    };

    rsx! {
        div {
            style: "position:relative;display:flex;flex-direction:column;gap:14px;padding:0 32px 40px;max-width:820px;",

            div {
                style: "display:flex;flex-direction:column;gap:6px;",
                span {
                    style: "font-size:16px;font-weight:600;color:#ffffff;",
                    "Stores"
                }
                span {
                    style: "font-size:12px;color:#78788a;line-height:1.5;",
                    "PitaGames drives the Epic, Amazon and GOG stores through their command-line clients. A store whose client is missing or signed out is listed here with what it needs; the launcher keeps working for the stores that are ready."
                }
            }

            div {
                style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                button {
                    disabled: is_pending,
                    onclick: move |_| {
                        pending.set(true);
                        start_probe(refresh_probes.sender.clone(), show_counts);
                    },
                    style: action_style(!is_pending),
                    "Refresh"
                }
                button {
                    disabled: is_pending,
                    onclick: move |_| {
                        let next = !show_counts;
                        pending.set(true);
                        start_probe(counts_probes.sender.clone(), next);
                    },
                    style: action_style(!is_pending),
                    if show_counts { "Hide game counts" } else { "Show game counts" }
                }
                button {
                    disabled: is_syncing,
                    onclick: move |_| {
                        syncing.set(true);
                        sync_message.set(String::new());
                        sync_problems.set(Vec::new());
                        start_sync(sync_starter.sender.clone());
                    },
                    style: action_style(!is_syncing),
                    "Sync libraries"
                }
                if is_pending {
                    span {
                        style: "font-size:11px;color:#7a9ad6;",
                        "checking…"
                    }
                }
                if is_syncing {
                    span {
                        style: "font-size:11px;color:#7a9ad6;",
                        "syncing…"
                    }
                }
            }

            if !message.is_empty() {
                div {
                    style: "display:flex;flex-direction:column;gap:4px;padding:10px 14px;border-radius:8px;background:#1e2430;color:#9ab0d8;font-size:12px;",
                    span { "{message}" }
                }
            }

            for problem in problems.iter().take(3) {
                div {
                    key: "{problem}",
                    style: "padding:8px 12px;border-radius:8px;background:#2a2420;color:#e0b090;font-size:11px;overflow-wrap:anywhere;",
                    "{problem}"
                }
            }

            if let Some(reports) = loaded {
                for report in reports {
                    StoreRow {
                        key: "{report.store.as_str()}",
                        report: report.clone(),
                        auth: auth_state,
                        auth_inputs,
                        auth_events,
                    }
                }
                span {
                    style: "font-size:11px;color:#78788a;line-height:1.5;",
                    "Signing in runs the store's own client: the page opens in your browser and PitaGames only passes on the one line it asks for. Nothing on this screen writes to a game or a store account. See docs/m2-auth-terminal.md."
                }
            } else {
                div {
                    style: "padding:40px 0;color:#78788a;font-size:13px;",
                    "Checking each store's command-line client…"
                }
            }

            if let Some(terminal) = terminal {
                div {
                    style: "position:fixed;inset:0;z-index:30;display:flex;align-items:center;justify-content:center;background:rgba(8,8,12,0.72);",

                    div {
                        style: "width:680px;max-width:92vw;display:flex;flex-direction:column;gap:12px;padding:18px 20px;border-radius:14px;background:#16161e;border:1px solid #2c2c38;box-shadow:0 24px 60px rgba(0,0,0,0.5);",

                        div {
                            style: "display:flex;align-items:center;gap:10px;",
                            span {
                                style: "font-size:15px;font-weight:600;color:#ffffff;",
                                "Sign in to {store_label(terminal.store())}"
                            }
                            span {
                                style: "padding:2px 8px;border-radius:999px;font-size:10px;text-transform:uppercase;letter-spacing:0.06em;background:{status_background(terminal.status())};color:#e8e8ee;",
                                "{status_label(terminal.status())}"
                            }
                            button {
                                onclick: move |_| {
                                    if terminal.running() {
                                        // Ask the CLI to stop; the run then
                                        // reports itself as cancelled and the
                                        // panel switches to Close.
                                        if let Some(inputs) = auth_inputs.read().as_ref() {
                                            inputs.cancel();
                                        }
                                    } else {
                                        auth_inputs.set(None);
                                        auth_state.set(None);
                                        auth_text.set(String::new());
                                    }
                                },
                                style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#c8c8d4;font-size:11px;cursor:pointer;",
                                if terminal.running() { "Cancel" } else { "Close" }
                            }
                        }

                        div {
                            style: "height:240px;overflow-y:auto;display:flex;flex-direction:column-reverse;gap:3px;padding:10px 12px;border-radius:8px;background:#0d0d13;border:1px solid #26262f;font-family:monospace;font-size:11px;line-height:1.5;color:#c8c8d4;user-select:text;",
                            // Newest first inside a reversed column: the last
                            // line sits at the bottom and stays in view.
                            for (index, line) in terminal.lines().iter().rev().enumerate() {
                                div {
                                    key: "{index}",
                                    style: "white-space:pre-wrap;overflow-wrap:anywhere;",
                                    "{line}"
                                }
                            }
                        }

                        if let Some(hint) = terminal.prompt() {
                            span {
                                style: "font-size:12px;color:#e0c090;line-height:1.5;",
                                "{hint}"
                            }
                        }

                        if terminal.running() {
                            div {
                                style: "display:flex;align-items:center;gap:8px;",
                                input {
                                    placeholder: "{placeholder}",
                                    value: "{typed}",
                                    oninput: move |event| auth_text.set(event.value()),
                                    onkeydown: move |event| {
                                        if event.key() != Key::Enter {
                                            return;
                                        }
                                        submit_auth_line(auth_inputs, auth_text);
                                    },
                                    style: "flex:1;padding:8px 11px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#e8e8ee;font-family:monospace;font-size:12px;outline:none;",
                                }
                                button {
                                    onclick: move |_| submit_auth_line(auth_inputs, auth_text),
                                    style: action_style(true),
                                    "Send"
                                }
                            }
                        }

                        div {
                            style: "display:flex;align-items:baseline;gap:10px;flex-wrap:wrap;font-size:11px;color:#78788a;",
                            if let Some(log) = terminal.log() {
                                span { "transcript: {log.display()}" }
                            }
                            span {
                                style: "margin-left:auto;",
                                "The client stores the session itself."
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Send the typed line to the running CLI, if there is one.
///
/// Only called when the terminal is running, so a stale click (the button of a
/// finished run) is a no-op rather than a write into nothing.
fn submit_auth_line(auth_inputs: Signal<Option<AuthInputs>>, mut auth_text: Signal<String>) {
    let Some(inputs) = (*auth_inputs.read()).clone() else {
        return;
    };
    let text = auth_text.read().trim().to_string();
    if text.is_empty() {
        return;
    }
    if inputs.answer(&text) {
        // The CLI echoes the line itself, so nothing is added here.
        auth_text.set(String::new());
    }
}

fn status_label(status: &AuthStatus) -> &'static str {
    match status {
        AuthStatus::Running => "running",
        AuthStatus::SignedIn => "signed in",
        AuthStatus::Failed(_) => "failed",
        AuthStatus::Cancelled => "cancelled",
    }
}

fn status_background(status: &AuthStatus) -> &'static str {
    match status {
        AuthStatus::Running => "#2c3a56",
        AuthStatus::SignedIn => "#2e4a34",
        AuthStatus::Failed(_) => "#4a2a2a",
        AuthStatus::Cancelled => "#2c2c38",
    }
}

fn store_label(store: Store) -> &'static str {
    match store {
        Store::Epic => "Epic",
        Store::Amazon => "Amazon",
        Store::Gog => "GOG",
        Store::Steam => "Steam",
        Store::Local => "Local",
    }
}

/// One store's card: tools, session, counts and what it needs.
#[derive(Props, Clone, PartialEq)]
struct StoreRowProps {
    report: StoreStatusReport,
    /// The sign-in terminal, so a row can show that its own run is live.
    auth: Signal<Option<AuthTerminal>>,
    /// The running sign-in's input, which the button starts and the terminal
    /// types into.
    auth_inputs: Signal<Option<AuthInputs>>,
    /// Where a started sign-in reports.
    auth_events: Signal<Option<UnboundedSender<AuthEvent>>>,
}

#[allow(non_snake_case)]
fn StoreRow(props: StoreRowProps) -> Element {
    let report = props.report;
    let mut auth = props.auth;
    let mut auth_inputs = props.auth_inputs;
    let auth_events = props.auth_events;
    let state = report.state();
    let (badge, badge_background) = match state {
        RowState::Ready => ("ready", "#2e4a34"),
        RowState::CliMissing => ("not installed", "#3a2a1e"),
        RowState::SignedOut => ("sign in required", "#3a2a1e"),
        RowState::Unverified => ("status unknown", "#2c2c38"),
    };

    let tool_lines: Vec<(String, bool)> = report
        .tools
        .iter()
        .map(|tool| match tool.path() {
            Some(path) => {
                let version = tool
                    .version
                    .as_deref()
                    .map(|version| format!(" {version}"))
                    .unwrap_or_default();
                (
                    format!("{}{}  ({})", tool.program, version, path.display()),
                    true,
                )
            }
            None => (format!("{} — not found in PATH", tool.program), false),
        })
        .collect();

    let session_line = match &report.session {
        SessionState::SignedIn { account } if account.trim().is_empty() => "signed in".to_string(),
        SessionState::SignedIn { account } => format!("signed in as {account}"),
        SessionState::SignedOut => "not signed in".to_string(),
        SessionState::Unknown => "session not checked".to_string(),
    };

    let counts_line = match (report.owned, report.installed) {
        (Some(owned), Some(installed)) => Some(format!("{owned} owned · {installed} installed")),
        (Some(owned), None) => Some(format!("{owned} owned")),
        (None, Some(installed)) => Some(format!("{installed} installed")),
        (None, None) => None,
    };

    let store = report.store;
    // The CLI that owns the login is the store's first program: `legendary`,
    // `nile`, or `lgogdownloader` (never `gogdl`, which is the game runner).
    let login_cli_ready = report.tools.first().is_some_and(|tool| tool.is_ready());
    let signing_in = auth
        .read()
        .as_ref()
        .is_some_and(|terminal| terminal.store() == store && terminal.running());
    let install_hint = (state == RowState::CliMissing).then(|| install_hint(store));

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:8px;padding:14px 16px;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;",

            div {
                style: "display:flex;align-items:center;gap:10px;flex-wrap:wrap;",
                span {
                    style: "font-size:14px;font-weight:600;color:#ffffff;text-transform:capitalize;",
                    "{report.store.as_str()}"
                }
                span {
                    style: "padding:2px 8px;border-radius:999px;font-size:10px;text-transform:uppercase;letter-spacing:0.06em;background:{badge_background};color:#e8e8ee;",
                    "{badge}"
                }
                if login_cli_ready {
                    button {
                        disabled: signing_in,
                        onclick: move |_| {
                            let Some(sender) = (*auth_events.read()).clone() else {
                                return;
                            };
                            let run = auth::next_run();
                            let inputs = auth::spawn(run, store, auth_options(), sender);
                            auth_inputs.set(Some(inputs));
                            auth.set(Some(AuthTerminal::new(run, store)));
                        },
                        style: action_style(!signing_in),
                        if signing_in { "signing in…" } else { "Sign in" }
                    }
                }
            }

            for (index, (line, ok)) in tool_lines.into_iter().enumerate() {
                span {
                    key: "{index}",
                    style: if ok { "font-size:12px;color:#c8c8d4;overflow-wrap:anywhere;" } else { "font-size:12px;color:#e0a890;overflow-wrap:anywhere;" },
                    "{line}"
                }
            }

            div {
                style: "display:flex;align-items:baseline;gap:10px;flex-wrap:wrap;",
                span {
                    style: "font-size:12px;color:#a8a8b8;",
                    "{session_line}"
                }
                if let Some(counts) = counts_line {
                    span {
                        style: "font-size:11px;color:#78788a;",
                        "{counts}"
                    }
                }
            }

            if let Some(hint) = install_hint {
                span {
                    style: "font-size:12px;color:#e0c090;line-height:1.5;",
                    "{hint}"
                }
            }

            if state == RowState::SignedOut && login_cli_ready {
                div {
                    style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                    code {
                        style: "padding:5px 8px;border-radius:7px;background:#14141b;border:1px solid #26262f;color:#c8c8d4;font-size:11px;overflow-wrap:anywhere;user-select:text;",
                        "pitagames-cli {report.store.as_str()} auth"
                    }
                    CopyButton { text: format!("pitagames-cli {} auth", report.store.as_str()) }
                }
            }

            for problem in report.problems.clone() {
                div {
                    key: "{problem}",
                    style: "padding:8px 12px;border-radius:8px;background:#2a2420;color:#e0b090;font-size:11px;overflow-wrap:anywhere;",
                    "{problem}"
                }
            }
        }
    }
}

/// What to install for a store whose client is missing.
fn install_hint(store: Store) -> String {
    match store {
        Store::Epic => {
            "Install legendary to enable Epic installs (pipx install legendary-gl, or the AUR package)."
                .to_string()
        }
        Store::Amazon => "Install nile to enable Amazon installs.".to_string(),
        Store::Gog => {
            "Install lgogdownloader (library and login) and gogdl (installs and play) to enable GOG."
                .to_string()
        }
        other => format!("Install the {other} command-line client to enable this store."),
    }
}

fn action_style(enabled: bool) -> &'static str {
    if enabled {
        "padding:6px 12px;border-radius:7px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:11px;cursor:pointer;"
    } else {
        "padding:6px 12px;border-radius:7px;border:1px solid #26262f;background:#1a1a22;color:#5a5a68;font-size:11px;cursor:default;"
    }
}
