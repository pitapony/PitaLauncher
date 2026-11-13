//! The Stores screen: one row per CLI store, and what each needs.
//!
//! The rows come from `stores::status` (framework-free): whether the store's CLI
//! is on `PATH`, its version, the signed-in account and, on request, the game
//! counts. The probe runs each CLI, so it happens on its own thread and lands
//! back here over a channel; the screen renders whatever the last probe found.
//!
//! Sign-in is deliberately not offered here: the auth flows are interactive
//! PTY sessions, and the in-app terminal is M2.5. The row names the CLI command
//! to run instead, with a copy button.

use std::rc::Rc;
use std::thread;

use dioxus::prelude::*;
use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_core::Store;
use pitagames_stores::cli::CliOptions;
use pitagames_stores::status::{self, RowState, SessionState, StoreStatusReport};

use crate::copy::CopyButton;

/// A finished probe: the rows and the `list` flag they were fetched with.
type ProbeReply = (bool, Vec<StoreStatusReport>);

/// The render-side end of the store-status channel.
///
/// Cloning shares one receiver. `use_hook` needs a `Clone` value and an
/// `UnboundedReceiver` is not one, so the receiver sits behind an async mutex,
/// exactly like the activation channel.
#[derive(Clone)]
struct Probes {
    sender: UnboundedSender<ProbeReply>,
    receiver: Rc<AsyncMutex<Option<UnboundedReceiver<ProbeReply>>>>,
}

impl Probes {
    fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver: Rc::new(AsyncMutex::new(Some(receiver))),
        }
    }

    /// Probe every store on a worker thread. A CLI run can take seconds, so it
    /// must never run on the render thread.
    fn start(&self, list: bool) {
        let sender = self.sender.clone();
        let spawned = thread::Builder::new()
            .name("pitagames-store-status".to_string())
            .spawn(move || {
                let options = CliOptions {
                    log_dir: pitagames_core::paths::logs_dir(),
                    ..CliOptions::default()
                };
                let reports = status::stores(&options, list);
                // A closed receiver only means the screen is gone.
                let _ = sender.unbounded_send((list, reports));
            });

        if let Err(error) = spawned {
            eprintln!("[pitagames] store status probe could not start: {error}");
        }
    }

    /// Wait for the next probe result; `None` once the channel closes.
    async fn next(&self) -> Option<ProbeReply> {
        let mut slot = self.receiver.lock().await;
        let receiver = slot.as_mut()?;
        receiver.next().await
    }
}

/// The Stores screen.
#[allow(non_snake_case)]
pub fn StoresView() -> Element {
    let mut rows = use_signal(|| None::<Vec<StoreStatusReport>>);
    let mut pending = use_signal(|| true);
    let mut counts = use_signal(|| false);

    let probes = use_hook(Probes::new);

    // The drain future starts the first probe, then renders every reply.
    let drain_probes = probes.clone();
    use_future(move || {
        let drain = drain_probes.clone();
        let starter = drain_probes.clone();
        async move {
            starter.start(false);
            while let Some((list, reports)) = drain.next().await {
                rows.set(Some(reports));
                counts.set(list);
                pending.set(false);
            }
        }
    });

    let refresh_probes = probes.clone();
    let counts_probes = probes.clone();

    let loaded = rows();
    let is_pending = pending();
    let show_counts = counts();

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:14px;padding:0 32px 40px;max-width:820px;",

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
                        refresh_probes.start(show_counts);
                    },
                    style: action_style(!is_pending),
                    "Refresh"
                }
                button {
                    disabled: is_pending,
                    onclick: move |_| {
                        let next = !show_counts;
                        pending.set(true);
                        counts_probes.start(next);
                    },
                    style: action_style(!is_pending),
                    if show_counts { "Hide game counts" } else { "Show game counts" }
                }
                if is_pending {
                    span {
                        style: "font-size:11px;color:#7a9ad6;",
                        "checking…"
                    }
                }
            }

            if let Some(reports) = loaded {
                for report in reports {
                    StoreRow { key: "{report.store.as_str()}", report: report.clone() }
                }
                span {
                    style: "font-size:11px;color:#78788a;line-height:1.5;",
                    "Nothing on this screen writes to a game or a store account. Sign-in runs the CLI's own browser flow; see docs/m2-store-status.md."
                }
            } else {
                div {
                    style: "padding:40px 0;color:#78788a;font-size:13px;",
                    "Checking each store's command-line client…"
                }
            }
        }
    }
}

/// One store's card: tools, session, counts and what it needs.
#[derive(Props, Clone, PartialEq)]
struct StoreRowProps {
    report: StoreStatusReport,
}

#[allow(non_snake_case)]
fn StoreRow(props: StoreRowProps) -> Element {
    let report = props.report;
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

    let install_hint = (state == RowState::CliMissing).then(|| install_hint(report.store));
    let auth_command =
        (state == RowState::SignedOut).then(|| format!("pitagames-cli {} auth", report.store));

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

            if let Some(command) = auth_command {
                div {
                    style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                    code {
                        style: "padding:5px 8px;border-radius:7px;background:#14141b;border:1px solid #26262f;color:#c8c8d4;font-size:11px;overflow-wrap:anywhere;user-select:text;",
                        "{command}"
                    }
                    CopyButton { text: command.clone() }
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
