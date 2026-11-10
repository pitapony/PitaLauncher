//! The Logs screen: list `<data>/logs`, tail one file, filter the tail.
//!
//! Reading and tailing live in `platform::logs` (framework-free); this module
//! only renders and forwards clicks. Everything is read-only — the screen never
//! writes, rotates or deletes a log.

use std::path::{Path, PathBuf};

use dioxus::prelude::*;

use pitagames_platform::logs::{self, LogFile, Tail};

use crate::copy::CopyButton;
use crate::format;

/// Lines read from the end of a file.
const TAIL_LINES: usize = 2000;
/// Lines rendered before the "showing the last …" note appears.
const MAX_SHOWN: usize = 400;

#[derive(Props, Clone, PartialEq)]
pub struct LogsViewProps {
    /// `<data>/logs`; `None` when there is no data directory (`HOME` unset).
    pub dir: Option<PathBuf>,
}

/// The log directory, a file list, and the tail of the selected file.
#[allow(non_snake_case)]
pub fn LogsView(props: LogsViewProps) -> Element {
    let dir = props.dir.clone();

    let files = use_signal(Vec::<LogFile>::new);
    let mut selected = use_signal(|| None::<String>);
    let tail = use_signal(|| None::<Tail>);
    let mut filter = use_signal(String::new);
    let error = use_signal(String::new);

    // Load once, on mount; a later Refresh re-runs the same read.
    let dir_future = dir.clone();
    use_future(move || {
        let dir = dir_future.clone();
        async move {
            if let Some(dir) = dir.as_deref() {
                reload(dir, selected, files, tail, error);
            }
        }
    });

    let dir_display = dir
        .as_ref()
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|| "(no data directory)".to_string());
    let file_rows = files();
    let current = selected();
    let error_text = error();
    let needle = filter().to_lowercase();

    // Filter in memory: `tail` already read a bounded window from disk.
    let mut lines: Vec<String> = tail
        .read()
        .as_ref()
        .map(|value| {
            value
                .lines
                .iter()
                .filter(|line| needle.is_empty() || line.to_lowercase().contains(&needle))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let matched = lines.len();
    if lines.len() > MAX_SHOWN {
        lines = lines.split_off(lines.len() - MAX_SHOWN);
    }
    let shown = lines.len();
    let tail_text = lines.join("\n");
    let truncated = tail.read().as_ref().is_some_and(|value| value.truncated);
    let summary = if current.is_none() {
        "Select a log to read it.".to_string()
    } else if shown == 0 {
        if needle.is_empty() {
            "The log is empty.".to_string()
        } else {
            format!("No line matches \"{}\".", filter())
        }
    } else if shown < matched || truncated {
        format!("Showing the last {shown} matching line(s).")
    } else {
        format!("{shown} line(s).")
    };

    let refresh_dir = dir.clone();
    let open_dir = dir.clone();
    let now = time::OffsetDateTime::now_utc();

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:12px;padding:0 32px 40px;height:100%;min-height:0;",

            div {
                style: "display:flex;flex-direction:column;gap:4px;",
                span {
                    style: "font-size:16px;font-weight:600;color:#ffffff;",
                    "Logs"
                }
                span {
                    style: "font-size:12px;color:#78788a;line-height:1.5;",
                    "Launch lines from pitagames-shim, the Proton log and DXVK's per-exe logs. Read-only: nothing here is edited or deleted."
                }
            }

            div {
                style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                code {
                    style: "font-size:11px;color:#a8a8b8;overflow-wrap:anywhere;",
                    "{dir_display}"
                }
                CopyButton { text: dir_display.clone() }
                button {
                    onclick: move |_| {
                        if let Some(dir) = refresh_dir.as_deref() {
                            reload(dir, selected, files, tail, error);
                        }
                    },
                    style: action_style(),
                    "Refresh"
                }
                if dir.is_some() {
                    button {
                        onclick: move |_| {
                            if let Some(dir) = open_dir.as_deref() {
                                open_folder(dir, error);
                            }
                        },
                        style: action_style(),
                        "Open folder"
                    }
                }
            }

            if !error_text.is_empty() {
                div {
                    style: "padding:10px 14px;border-radius:8px;background:#3a2a1e;color:#e0c090;font-size:12px;",
                    "{error_text}"
                }
            }

            if file_rows.is_empty() {
                div {
                    style: "padding:40px 0;color:#78788a;font-size:13px;",
                    "No logs yet. pitagames-shim appends a line to shim.log each time a game launches through it."
                }
            } else {
                div {
                    style: "display:flex;gap:12px;flex:1;min-height:0;",

                    div {
                        style: "display:flex;flex-direction:column;gap:6px;width:260px;flex:0 0 260px;overflow-y:auto;",
                        for file in file_rows {
                            {
                                let name = file.name.clone();
                                let is_current = current.as_deref() == Some(name.as_str());
                                let read_dir = dir.clone();
                                let modified = file
                                    .modified
                                    .map(time::OffsetDateTime::from)
                                    .map(|at| format::log_time(at, now))
                                    .unwrap_or_else(|| "unknown time".to_string());
                                let size = format::human_bytes(file.size);
                                let kind = file.kind.label();
                                rsx! {
                                    button {
                                        key: "{name}",
                                        onclick: move |_| {
                                            if let Some(dir) = read_dir.as_deref() {
                                                read_tail(dir, &name, tail, error);
                                            }
                                            selected.set(Some(name.clone()));
                                        },
                                        style: file_style(is_current),
                                        span {
                                            style: "font-size:13px;font-weight:600;color:#e8e8ee;overflow-wrap:anywhere;",
                                            "{file.name}"
                                        }
                                        span {
                                            style: "font-size:11px;color:#78788a;",
                                            "{kind} · {size} · {modified}"
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div {
                        style: "display:flex;flex-direction:column;gap:8px;flex:1;min-width:0;",

                        div {
                            style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                            input {
                                placeholder: "Filter lines",
                                value: "{filter}",
                                oninput: move |event| filter.set(event.value()),
                                style: "flex:1;min-width:180px;padding:7px 10px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:#e8e8ee;font-size:12px;outline:none;",
                            }
                            CopyButton { text: tail_text.clone(), label: "Copy tail".to_string() }
                        }

                        span {
                            style: "font-size:11px;color:#78788a;",
                            "{summary}"
                        }

                        pre {
                            style: "flex:1;min-height:0;margin:0;overflow:auto;padding:12px 14px;border-radius:10px;background:#14141b;border:1px solid #26262f;color:#c8c8d4;font-family:monospace;font-size:11px;line-height:1.5;white-space:pre-wrap;overflow-wrap:anywhere;user-select:text;",
                            "{tail_text}"
                        }
                    }
                }
            }
        }
    }
}

/// Re-list the directory and keep the selection when it still exists.
fn reload(
    dir: &Path,
    mut selected: Signal<Option<String>>,
    mut files: Signal<Vec<LogFile>>,
    mut tail: Signal<Option<Tail>>,
    mut error: Signal<String>,
) {
    let listed = logs::list(dir);
    let keep = selected.read().clone();
    let name = keep
        .filter(|name| listed.iter().any(|file| &file.name == name))
        .or_else(|| listed.first().map(|file| file.name.clone()));
    files.set(listed);
    selected.set(name.clone());

    match name {
        Some(name) => read_tail(dir, &name, tail, error),
        None => {
            tail.set(None);
            error.set(String::new());
        }
    }
}

/// Read one file's tail, turning a read failure into a visible message.
fn read_tail(dir: &Path, name: &str, mut tail: Signal<Option<Tail>>, mut error: Signal<String>) {
    let path = dir.join(name);
    match logs::tail(&path, TAIL_LINES, None) {
        Ok(value) => {
            tail.set(Some(value));
            error.set(String::new());
        }
        Err(source) => {
            tail.set(None);
            error.set(format!("Could not read {}: {source}", path.display()));
        }
    }
}

/// Ask the desktop to open the log folder; a missing file manager is a message.
fn open_folder(dir: &Path, mut error: Signal<String>) {
    match std::process::Command::new("xdg-open").arg(dir).spawn() {
        Ok(_) => error.set(String::new()),
        Err(source) => error.set(format!("Could not open {}: {source}", dir.display())),
    }
}

fn action_style() -> &'static str {
    "padding:6px 12px;border-radius:7px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:11px;cursor:pointer;"
}

fn file_style(current: bool) -> &'static str {
    if current {
        "display:flex;flex-direction:column;gap:2px;align-items:flex-start;text-align:left;padding:10px 12px;border-radius:10px;border:1px solid #3f5c8c;background:#243044;cursor:pointer;"
    } else {
        "display:flex;flex-direction:column;gap:2px;align-items:flex-start;text-align:left;padding:10px 12px;border-radius:10px;border:1px solid #2c2c38;background:#1e1e28;cursor:pointer;"
    }
}
