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
use pitagames_core::i18n::t;
use pitagames_stores::cli::CliOptions;
use pitagames_stores::status::{self, RowState, SessionState, StoreStatusReport};
use pitagames_stores::sync::{Backends, StoreLibrary};

use crate::View;
use crate::auth;
use crate::copy::CopyButton;
use crate::focus::{self, ActionChannel};
use crate::library::Library;
use crate::store_sync;
use crate::terminal::{
    self, Terminal, TerminalChannel, TerminalEvent, TerminalInputs, TerminalStatus,
};
use pitagames_input::Action;

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
    /// The app's one input pump posts this screen's actions here.
    pub actions: ActionChannel,
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
    // types back into it. `auth_store` remembers which store started the run, so
    // its row can show `signing in…` while it is live.
    let mut auth_state = use_signal(|| None::<Terminal>);
    let mut auth_inputs = use_signal(|| None::<TerminalInputs>);
    let mut auth_store = use_signal(|| None::<Store>);
    let mut auth_text = use_signal(String::new);

    let mut session = crate::context::use_session();
    let actions = props.actions;
    let screen_focus = focus::use_focus();

    let probes = use_hook(Channel::<ProbeReply>::new);
    let syncs = use_hook(Channel::<SyncReply>::new);
    let auth_channel = use_hook(TerminalChannel::new);
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
                    sync_message.set(t("stores.needs_db").to_string());
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

                let mut message = t("stores.sync_summary")
                    .replace("{total}", &total_rows.to_string())
                    .replace("{applied}", &applied.to_string())
                    .replace("{stores}", &status::CLI_STORES.len().to_string());
                if !problems.is_empty() {
                    message.push_str(
                        &t("stores.sync_warnings").replace("{count}", &problems.len().to_string()),
                    );
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
                    auth_store.set(None);
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
        t("stores.paste_placeholder")
    } else {
        t("stores.waiting_browser")
    };

    // The Stores screen's action drain. The app's one pump posts here; this
    // screen owns its own graph. Without the modal the cells are the three top
    // actions followed by one cell per store row; with the modal open they are
    // its Cancel/Close and Send buttons. Back leaves the screen (or closes the
    // modal when one is open).
    let action_probes = probes.clone();
    let action_syncs = syncs.clone();
    use_future(move || {
        let channel = actions.clone();
        let probes = action_probes.clone();
        let syncs = action_syncs.clone();
        async move {
            while let Some(action) = channel.next().await {
                if session.read().view() != View::Stores {
                    continue;
                }
                let modal_open = auth_state.read().is_some();
                if modal_open {
                    let running = auth_state.read().as_ref().is_some_and(|t| t.running());
                    let count = if running { 2 } else { 1 };
                    match action {
                        Action::Back => {
                            session.write().set_text_focused(false);
                            cancel_or_close_auth(auth_state, auth_inputs, auth_text);
                        }
                        action if action.is_navigation() => {
                            screen_focus.move_focus(action, count, 1);
                        }
                        Action::Confirm => match screen_focus.index() {
                            Some(0) => {
                                session.write().set_text_focused(false);
                                cancel_or_close_auth(auth_state, auth_inputs, auth_text);
                            }
                            Some(1) if running => submit_auth_line(auth_inputs, auth_text),
                            _ => {}
                        },
                        _ => {}
                    }
                    continue;
                }

                let reports = (*rows.read()).clone().unwrap_or_default();
                let count = 3 + reports.len();
                match action {
                    Action::Back => session.write().set_view(View::Library),
                    action if action.is_navigation() => {
                        screen_focus.move_focus(action, count, 1);
                    }
                    Action::Confirm => match screen_focus.index() {
                        Some(0) => {
                            pending.set(true);
                            start_probe(probes.sender.clone(), counts());
                        }
                        Some(1) => {
                            let next = !counts();
                            pending.set(true);
                            start_probe(probes.sender.clone(), next);
                        }
                        Some(2) => {
                            syncing.set(true);
                            sync_message.set(String::new());
                            sync_problems.set(Vec::new());
                            start_sync(syncs.sender.clone());
                        }
                        Some(index) if index >= 3 => {
                            if let Some(report) = reports.get(index - 3) {
                                start_sign_in(
                                    report.store,
                                    auth_state,
                                    auth_inputs,
                                    auth_store,
                                    auth_events,
                                );
                                screen_focus.set(Some(0));
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    });

    // Focus rings for the three top actions and the modal's two buttons. The
    // row rings are computed by `StoreRow` from its own cell index. Reading
    // `screen_focus` here subscribes the render to a move, so the ring repaints
    // without a click.
    let refresh_ring = focus::ring_style(screen_focus.index() == Some(0));
    let counts_ring = focus::ring_style(screen_focus.index() == Some(1));
    let sync_ring = focus::ring_style(screen_focus.index() == Some(2));
    let cancel_ring = focus::ring_style(screen_focus.index() == Some(0));
    let send_ring = focus::ring_style(screen_focus.index() == Some(1));

    rsx! {
        div {
            style: "position:relative;display:flex;flex-direction:column;gap:14px;padding:0 32px 40px;max-width:820px;",

            div {
                style: "display:flex;flex-direction:column;gap:6px;",
                span {
                    style: "font-size:var(--text-heading);font-weight:600;color:#ffffff;",
                    {t("stores.title")}
                }
                span {
                    style: "font-size:var(--text-caption);color:var(--text-muted);line-height:1.5;",
                    {t("stores.description")}
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
                    style: "{action_style(!is_pending)}{refresh_ring}",
                    {t("action.refresh")}
                }
                button {
                    disabled: is_pending,
                    onclick: move |_| {
                        let next = !show_counts;
                        pending.set(true);
                        start_probe(counts_probes.sender.clone(), next);
                    },
                    style: "{action_style(!is_pending)}{counts_ring}",
                    if show_counts { t("stores.hide_counts") } else { t("stores.show_counts") }
                }
                button {
                    disabled: is_syncing,
                    onclick: move |_| {
                        syncing.set(true);
                        sync_message.set(String::new());
                        sync_problems.set(Vec::new());
                        start_sync(sync_starter.sender.clone());
                    },
                    style: "{action_style(!is_syncing)}{sync_ring}",
                    {t("stores.sync")}
                }
                if is_pending {
                    span {
                        style: "font-size:var(--text-micro);color:var(--accent);",
                        {t("stores.checking")}
                    }
                }
                if is_syncing {
                    span {
                        style: "font-size:var(--text-micro);color:var(--accent);",
                        {t("stores.syncing")}
                    }
                }
            }

            if !message.is_empty() {
                div {
                    style: "display:flex;flex-direction:column;gap:4px;padding:10px 14px;border-radius:8px;background:#1e2430;color:#9ab0d8;font-size:var(--text-caption);",
                    span { "{message}" }
                }
            }

            for problem in problems.iter().take(3) {
                div {
                    key: "{problem}",
                    style: "padding:8px 12px;border-radius:8px;background:#2a2420;color:#e0b090;font-size:var(--text-micro);overflow-wrap:anywhere;",
                    "{problem}"
                }
            }

            if let Some(reports) = loaded {
                for (row_index, report) in reports.iter().enumerate() {
                    StoreRow {
                        key: "{report.store.as_str()}",
                        report: report.clone(),
                        focused: screen_focus.index() == Some(3 + row_index),
                        focus: screen_focus,
                        auth: auth_state,
                        auth_inputs,
                        auth_store,
                        auth_events,
                    }
                }
                span {
                    style: "font-size:var(--text-micro);color:var(--text-muted);line-height:1.5;",
                    {t("stores.auth_hint")}
                }
            } else {
                div {
                    style: "padding:40px 0;color:var(--text-muted);font-size:var(--text-body);",
                    {t("stores.checking_clients")}
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
                                style: "font-size:var(--text-heading);font-weight:600;color:#ffffff;",
                                {t("stores.sign_in_title").replace("{store}", &terminal.title())}
                            }
                            span {
                                style: "padding:2px 8px;border-radius:999px;font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;background:{status_background(terminal.status())};color:var(--text);",
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
                                        session.write().set_text_focused(false);
                                        auth_inputs.set(None);
                                        auth_state.set(None);
                                        auth_text.set(String::new());
                                    }
                                },
                                style: "margin-left:auto;padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-micro);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);{cancel_ring}",
                                if terminal.running() { t("action.cancel") } else { t("action.close") }
                            }
                        }

                        div {
                            style: "height:240px;overflow-y:auto;display:flex;flex-direction:column-reverse;gap:3px;padding:10px 12px;border-radius:8px;background:#0d0d13;border:1px solid #26262f;font-family:monospace;font-size:var(--text-micro);line-height:1.5;color:var(--text-secondary);user-select:text;",
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
                                style: "font-size:var(--text-caption);color:#e0c090;line-height:1.5;",
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
                                    onfocus: move |_| session.write().set_text_focused(true),
                                    onblur: move |_| session.write().set_text_focused(false),
                                    onkeydown: move |event| {
                                        if event.key() != Key::Enter {
                                            return;
                                        }
                                        submit_auth_line(auth_inputs, auth_text);
                                    },
                                    style: "flex:1;padding:8px 11px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text);font-family:monospace;font-size:var(--text-caption);outline:none;",
                                }
                                button {
                                    onclick: move |_| submit_auth_line(auth_inputs, auth_text),
                                    style: "{action_style(true)}{send_ring}",
                                    {t("stores.send")}
                                }
                            }
                        }

                        div {
                            style: "display:flex;align-items:baseline;gap:10px;flex-wrap:wrap;font-size:var(--text-micro);color:var(--text-muted);",
                            if let Some(log) = terminal.log() {
                                span { {t("stores.transcript").replace("{path}", &log.display().to_string())} }
                            }
                            span {
                                style: "margin-left:auto;",
                                {t("stores.client_stores_session")}
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
fn submit_auth_line(auth_inputs: Signal<Option<TerminalInputs>>, mut auth_text: Signal<String>) {
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

/// Cancel a running sign-in, or close a finished one.
///
/// The terminal's Cancel button and the controller's Back/Confirm both land
/// here, so a running CLI is asked to stop and a finished panel is dismissed
/// without leaving a dead handle behind.
fn cancel_or_close_auth(
    mut auth_state: Signal<Option<Terminal>>,
    mut auth_inputs: Signal<Option<TerminalInputs>>,
    mut auth_text: Signal<String>,
) {
    let running = auth_state
        .read()
        .as_ref()
        .is_some_and(|terminal| terminal.running());
    if running {
        if let Some(inputs) = auth_inputs.read().as_ref() {
            inputs.cancel();
        }
    } else {
        auth_inputs.set(None);
        auth_state.set(None);
        auth_text.set(String::new());
    }
}

/// Start a store's sign-in from a controller or keyboard action.
///
/// The row's button does the same steps; this is the one copy the action drain
/// calls so a controller cannot start a run the pointer could not.
fn start_sign_in(
    store: Store,
    mut auth_state: Signal<Option<Terminal>>,
    mut auth_inputs: Signal<Option<TerminalInputs>>,
    mut auth_store: Signal<Option<Store>>,
    auth_events: Signal<Option<UnboundedSender<TerminalEvent>>>,
) {
    let Some(sender) = (*auth_events.read()).clone() else {
        return;
    };
    let run = terminal::next_run();
    let inputs = auth::spawn(run, store, auth_options(), sender);
    auth_inputs.set(Some(inputs));
    auth_store.set(Some(store));
    auth_state.set(Some(Terminal::new(run, store_label(store))));
}

fn status_label(status: &TerminalStatus) -> &'static str {
    match status {
        TerminalStatus::Running => t("stores.running"),
        TerminalStatus::Succeeded => t("stores.signed_in"),
        TerminalStatus::Failed(_) => t("stores.failed"),
        TerminalStatus::Cancelled => t("stores.cancelled"),
    }
}

fn status_background(status: &TerminalStatus) -> &'static str {
    match status {
        TerminalStatus::Running => "#2c3a56",
        TerminalStatus::Succeeded => "#2e4a34",
        TerminalStatus::Failed(_) => "#4a2a2a",
        TerminalStatus::Cancelled => "#2c2c38",
    }
}

fn store_label(store: Store) -> &'static str {
    match store {
        Store::Epic => t("store.epic"),
        Store::Amazon => t("store.amazon"),
        Store::Gog => t("store.gog"),
        Store::Steam => t("store.steam"),
        Store::Local => t("store.local"),
    }
}

/// One store's card: tools, session, counts and what it needs.
#[derive(Props, Clone, PartialEq)]
struct StoreRowProps {
    report: StoreStatusReport,
    /// Whether this row's cell is the screen's focused cell.
    focused: bool,
    /// The screen's focus, so the row's own Sign in button can move the ring
    /// into the modal.
    focus: focus::ScreenFocus,
    /// The sign-in terminal, so a row can show that its own run is live.
    auth: Signal<Option<Terminal>>,
    /// The running sign-in's input, which the button starts and the terminal
    /// types into.
    auth_inputs: Signal<Option<TerminalInputs>>,
    /// Which store started the live sign-in, so this row can tell whether the
    /// terminal is *its* run.
    auth_store: Signal<Option<Store>>,
    /// Where a started sign-in reports.
    auth_events: Signal<Option<UnboundedSender<TerminalEvent>>>,
}

#[allow(non_snake_case)]
fn StoreRow(props: StoreRowProps) -> Element {
    let report = props.report;
    let focused = props.focused;
    let focus = props.focus;
    let mut auth = props.auth;
    let mut auth_inputs = props.auth_inputs;
    let mut auth_store = props.auth_store;
    let auth_events = props.auth_events;
    let ring = focus::ring_style(focused);
    let state = report.state();
    let (badge, badge_background) = match state {
        RowState::Ready => (t("stores.ready"), "#2e4a34"),
        RowState::CliMissing => (t("stores.not_installed"), "#3a2a1e"),
        RowState::SignedOut => (t("stores.sign_in_required"), "#3a2a1e"),
        RowState::Unverified => (t("stores.status_unknown"), "#2c2c38"),
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
            None => (
                t("stores.not_in_path").replace("{program}", &tool.program),
                false,
            ),
        })
        .collect();

    let session_line = match &report.session {
        SessionState::SignedIn { account } if account.trim().is_empty() => {
            t("stores.signed_in").to_string()
        }
        SessionState::SignedIn { account } => {
            t("stores.signed_in_as").replace("{account}", account)
        }
        SessionState::SignedOut => t("stores.not_signed_in").to_string(),
        SessionState::Unknown => t("stores.session_not_checked").to_string(),
    };

    let counts_line = match (report.owned, report.installed) {
        (Some(owned), Some(installed)) => Some(
            t("stores.counts_owned_installed")
                .replace("{owned}", &owned.to_string())
                .replace("{installed}", &installed.to_string()),
        ),
        (Some(owned), None) => {
            Some(t("stores.counts_owned").replace("{owned}", &owned.to_string()))
        }
        (None, Some(installed)) => Some(
            t("stores.counts_installed").replace("{installed}", &installed.to_string()),
        ),
        (None, None) => None,
    };

    let store = report.store;
    // The CLI that owns the login is the store's first program: `legendary`,
    // `nile`, or `lgogdownloader` (never `gogdl`, which is the game runner).
    let login_cli_ready = report.tools.first().is_some_and(|tool| tool.is_ready());
    let signing_in = auth_store() == Some(store)
        && auth
            .read()
            .as_ref()
            .is_some_and(|terminal| terminal.running());
    let install_hint = (state == RowState::CliMissing).then(|| install_hint(store));

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:8px;padding:14px 16px;border-radius:12px;background:#1e1e28;border:1px solid #2c2c38;{ring}",

            div {
                style: "display:flex;align-items:center;gap:10px;flex-wrap:wrap;",
                span {
                    style: "font-size:var(--text-body);font-weight:600;color:#ffffff;text-transform:capitalize;",
                    "{report.store.as_str()}"
                }
                span {
                    style: "padding:2px 8px;border-radius:999px;font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;background:{badge_background};color:var(--text);",
                    "{badge}"
                }
                if login_cli_ready {
                    button {
                        disabled: signing_in,
                        onclick: move |_| {
                            let Some(sender) = (*auth_events.read()).clone() else {
                                return;
                            };
                            let run = terminal::next_run();
                            let inputs = auth::spawn(run, store, auth_options(), sender);
                            auth_inputs.set(Some(inputs));
                            auth_store.set(Some(store));
                            auth.set(Some(Terminal::new(run, store_label(store))));
                            // The modal's cells start at Cancel; the ring
                            // follows so a controller sees where it landed.
                            focus.set(Some(0));
                        },
                        style: action_style(!signing_in),
                        if signing_in { t("stores.signing_in") } else { t("stores.sign_in") }
                    }
                }
            }

            for (index, (line, ok)) in tool_lines.into_iter().enumerate() {
                span {
                    key: "{index}",
                    style: if ok { "font-size:var(--text-caption);color:var(--text-secondary);overflow-wrap:anywhere;" } else { "font-size:var(--text-caption);color:#e0a890;overflow-wrap:anywhere;" },
                    "{line}"
                }
            }

            div {
                style: "display:flex;align-items:baseline;gap:10px;flex-wrap:wrap;",
                span {
                    style: "font-size:var(--text-caption);color:var(--text-secondary);",
                    "{session_line}"
                }
                if let Some(counts) = counts_line {
                    span {
                        style: "font-size:var(--text-micro);color:var(--text-muted);",
                        "{counts}"
                    }
                }
            }

            if let Some(hint) = install_hint {
                span {
                    style: "font-size:var(--text-caption);color:#e0c090;line-height:1.5;",
                    "{hint}"
                }
            }

            if state == RowState::SignedOut && login_cli_ready {
                div {
                    style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                    code {
                        style: "padding:5px 8px;border-radius:7px;background:#14141b;border:1px solid #26262f;color:var(--text-secondary);font-size:var(--text-micro);overflow-wrap:anywhere;user-select:text;",
                        "pitagames-cli {report.store.as_str()} auth"
                    }
                    CopyButton { text: format!("pitagames-cli {} auth", report.store.as_str()) }
                }
            }

            for problem in report.problems.clone() {
                div {
                    key: "{problem}",
                    style: "padding:8px 12px;border-radius:8px;background:#2a2420;color:#e0b090;font-size:var(--text-micro);overflow-wrap:anywhere;",
                    "{problem}"
                }
            }
        }
    }
}

/// What to install for a store whose client is missing.
fn install_hint(store: Store) -> String {
    match store {
        Store::Epic => t("stores.epic_hint").to_string(),
        Store::Amazon => t("stores.amazon_hint").to_string(),
        Store::Gog => t("stores.gog_hint").to_string(),
        other => t("stores.generic_hint").replace("{store}", other.as_str()),
    }
}

fn action_style(enabled: bool) -> &'static str {
    if enabled {
        "padding:6px 12px;border-radius:7px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:var(--text-micro);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);"
    } else {
        "padding:6px 12px;border-radius:7px;border:1px solid #26262f;background:#1a1a22;color:var(--text-disabled);font-size:var(--text-micro);cursor:default;"
    }
}
