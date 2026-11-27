//! The in-app DLSSNR install panel on the Graphics screen.
//!
//! It is the pre-flight form the plan asks for: the route is shown and fixed to
//! OptiScaler, the network and processing are chosen, and the one file that is
//! not in the package — the NR model — is sourced by scanning the game and its
//! Wine prefix for a verified `nvngx_dlssnr.dll`, falling back to a picked file.
//! **Install** then runs upstream's `install.sh` in the shared terminal
//! ([`crate::terminal`]) through [`crate::nr_install`], so the installer's own
//! prompts are answered by the person, never guessed.
//!
//! All the rules live in [`pitagames_platform::nr_install`] (the form, the scan,
//! the PE version check, the argv) and [`pitagames_stores::mod_install`] (the
//! relay); this file only draws them and routes clicks.

use dioxus::prelude::*;
use std::sync::{Arc, Mutex};

use pitagames_core::i18n::t;
use pitagames_core::paths;
use pitagames_core::profile::{ModelSource, NeuralRendering, NrModel, NrProcessing};
use pitagames_mods::CapturedEnv;
use pitagames_platform::nr::NrPlan;
use pitagames_platform::nr_install::{self, DO_NOT_SHARE, InstallForm, ModelScan};
use pitagames_platform::nr_uninstall::{self, Strategy};

use crate::library::GameEntry;
use crate::nr_install as runner;
use crate::terminal::{self, Terminal, TerminalChannel, TerminalInputs, TerminalStatus};

#[derive(Props, Clone, PartialEq)]
pub struct NrInstallProps {
    /// The game the installer targets (its executable directory is the cwd).
    pub entry: GameEntry,
    /// The resolved backend and pinned release, from `platform::nr::plan`.
    pub plan: NrPlan,
    /// Called when the installer finishes successfully: the neural-rendering
    /// value to write into the game's profile, carrying the launch options the
    /// installer printed (`captured_env`). The Graphics screen folds it into its
    /// draft; nothing is persisted until the user applies.
    pub on_installed: EventHandler<NeuralRendering>,
}

/// The NR install panel.
#[allow(non_snake_case)]
pub fn NrInstallPanel(props: NrInstallProps) -> Element {
    let entry = props.entry.clone();
    let plan = props.plan.clone();
    let on_installed = props.on_installed;

    let plan_for_form = plan.clone();
    let mut form = use_signal(move || {
        InstallForm::new(plan_for_form.backend, 100, ModelSource::CachedInPackage)
    });
    let mut scanned = use_signal(|| None::<ModelScan>);
    let mut picked = use_signal(String::new);
    let mut status = use_signal(String::new);

    let default_package = default_package_path(&plan);
    let mut package = use_signal(|| default_package.to_string_lossy().into_owned());

    let mut terminal = use_signal(|| None::<Terminal>);
    let mut inputs = use_signal(|| None::<TerminalInputs>);
    let mut typed = use_signal(String::new);
    // The allowlisted env the installer printed, filled when the run ends. #30
    // is what writes it into the profile; here it is surfaced so the user sees
    // the capture happened without a copy-and-paste step.
    let mut captured = use_signal(|| None::<Arc<Mutex<Option<CapturedEnv>>>>);
    let channel = use_hook(TerminalChannel::new);
    let events = use_signal(|| Some(channel.sender()));

    // The terminal drain: fold every report into the sheet, and clear the input
    // handle when the run ends so a stale Send does nothing.
    let drain = channel.clone();
    let drain_plan = plan.clone();
    use_future(move || {
        let drain = drain.clone();
        let plan = drain_plan.clone();
        async move {
            while let Some(event) = drain.next().await {
                let finished = event.is_finished();
                let succeeded = {
                    let mut guard = terminal.write();
                    guard.as_mut().is_some_and(|sheet| sheet.apply(&event))
                };
                if succeeded {
                    let captured_env = captured
                        .read()
                        .as_ref()
                        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()))
                        .unwrap_or_default();
                    if captured_env.is_empty() {
                        status.set(t("nr.finished").to_string());
                    } else {
                        let names = captured_env
                            .env
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ");
                        status.set(t("nr.finished_captured").replace("{names}", &names));
                    }
                    // Hand the chosen network, scale and the installer's printed
                    // launch options to the Graphics screen's draft. Nothing is
                    // written until the user applies.
                    let nr = form.read().neural_rendering_with_env(
                        plan.pin.version.to_string(),
                        captured_env.env.clone(),
                    );
                    on_installed.call(nr);
                }
                if finished {
                    inputs.set(None);
                    typed.set(String::new());
                }
            }
        }
    });

    // -- actions -----------------------------------------------------------

    let scan_entry = entry.clone();
    let scan = move |_| {
        let prefix = pitagames_platform::prefix::dir(&scan_entry.game.id);
        let roots =
            nr_install::scan_roots(scan_entry.game.install_dir.as_deref(), prefix.as_deref());
        let found = nr_install::scan(&roots);
        status.set(if found.is_empty() {
            t("nr.no_model").to_string()
        } else {
            t("nr.found_models").replace("{count}", &found.found.len().to_string())
        });
        scanned.set(Some(found));
    };

    let pick = move |_| {
        let path = std::path::PathBuf::from(picked.read().as_str());
        match nr_install::inspect_model(&path) {
            Ok(model) => {
                form.write().source = model.source();
                status.set(
                    t("nr.using_file")
                        .replace("{path}", &model.path.display().to_string())
                        .replace("{version}", &model.version),
                );
            }
            Err(error) => status.set(error.to_string()),
        }
    };

    let install_entry = entry.clone();
    let install = move |_| {
        let Some(exe) = install_entry.game.exe.clone() else {
            status.set(t("nr.no_exe_install").to_string());
            return;
        };
        let form_value = form.read().clone();
        let package_dir = std::path::PathBuf::from(package.read().as_str());
        match nr_install::plan(&form_value, &package_dir, &exe) {
            Ok(plan) => {
                let Some(sender) = (*events.read()).clone() else {
                    return;
                };
                let label = runner::title(install_entry.title());
                let run = terminal::next_run();
                let started = runner::spawn(run, plan, sender);
                captured.set(Some(Arc::clone(&started.captured)));
                inputs.set(Some(started.inputs));
                terminal.set(Some(Terminal::new(run, label)));
                status.set(String::new());
            }
            Err(error) => status.set(error.to_string()),
        }
    };

    // Uninstall prefers upstream's own `remove` route (it knows files our
    // manifest cannot); when that is not on disk, the launcher's manifest
    // restores the tree. The check and the report are `platform::nr_uninstall`'s.
    let uninstall_entry = entry.clone();
    let uninstall = move |_| {
        let Some(exe) = uninstall_entry.game.exe.clone() else {
            status.set(t("nr.no_exe_uninstall").to_string());
            return;
        };
        let game_dir = exe
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| exe.clone());
        let package_dir = std::path::PathBuf::from(package.read().as_str());
        match nr_uninstall::strategy(&package_dir, &game_dir) {
            Strategy::UpstreamRemove => match nr_uninstall::remove_plan(&package_dir, &game_dir) {
                Ok(plan) => {
                    let Some(sender) = (*events.read()).clone() else {
                        return;
                    };
                    let label = runner::uninstall_title(uninstall_entry.title());
                    let run = terminal::next_run();
                    let started = runner::spawn_remove(run, plan, sender);
                    inputs.set(Some(started.inputs));
                    terminal.set(Some(Terminal::new(run, label)));
                    status.set(String::new());
                }
                Err(error) => status.set(error.to_string()),
            },
            Strategy::Manifest => {
                let mut status = status;
                spawn(async move {
                    match nr_uninstall::restore_manifest(&game_dir) {
                        Ok(report) => status.set(report.summary()),
                        Err(error) => status.set(error.to_string()),
                    }
                });
            }
            Strategy::Nothing => {
                status.set(t("nr.nothing_to_uninstall").to_string());
            }
        }
    };

    // -- render ------------------------------------------------------------

    let current = form.read().clone();
    let package_text = package.read().clone();
    let scan_snapshot = scanned.read().clone();
    let status_text = status.read().clone();
    let picked_text = picked.read().clone();
    let sheet = terminal.read().clone();
    let sheet_close_label = sheet.as_ref().map_or("", |sheet| {
        if sheet.running() {
            t("action.cancel")
        } else {
            t("action.close")
        }
    });
    let typed_text = typed.read().clone();

    let backend = pitagames_platform::nr::backend_label(plan.backend);
    let version = plan.pin.version.to_string();
    let experimental = plan.experimental;

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:10px;padding:14px 16px;border-radius:12px;background:#1a1e28;border:1px solid #2c3444;",

            div {
                style: "display:flex;align-items:baseline;gap:10px;flex-wrap:wrap;",
                h3 {
                    style: "margin:0;font-size:var(--text-heading);font-weight:600;color:#ffffff;",
                    {t("nr.title")}
                }
                span {
                    style: "font-size:var(--text-micro);color:var(--text-muted);",
                    {t("nr.backend").replace("{backend}", backend).replace("{version}", &version)}
                }
                if experimental {
                    span {
                        style: "padding:2px 8px;border-radius:999px;font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;background:#3a2a1e;color:#e0c090;",
                        {t("nr.not_tested")}
                    }
                }
            }

            span {
                style: "font-size:var(--text-caption);color:var(--text-secondary);line-height:1.5;",
                {t("nr.route_hint")}
            }

            div {
                style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                span { style: "font-size:var(--text-caption);color:var(--text-muted);", {t("nr.route")} }
                span {
                    style: "padding:3px 10px;border-radius:7px;background:#243044;color:#ffffff;font-size:var(--text-caption);",
                    "OptiScaler"
                }
            }

            div {
                style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                span { style: "font-size:var(--text-caption);color:var(--text-muted);", {t("nr.network")} }
                for (model, label) in model_choices() {
                    button {
                        key: "model-{label}",
                        onclick: move |_| form.write().model = model,
                        style: "{chip_style(current.model == model)}",
                        "{label}"
                    }
                }
            }

            div {
                style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                span { style: "font-size:var(--text-caption);color:var(--text-muted);", {t("nr.processing")} }
                for (processing, label) in processing_choices() {
                    button {
                        key: "proc-{label}",
                        onclick: move |_| form.write().processing = processing,
                        style: "{chip_style(current.processing == processing)}",
                        "{label}"
                    }
                }
            }

            div {
                style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                span { style: "font-size:var(--text-caption);color:var(--text-muted);", {t("nr.model")} }
                button {
                    onclick: scan,
                    style: "{button_style(true)}",
                    {t("nr.scan")}
                }
                if let Some(found) = scan_snapshot {
                    for (index, model) in found.found.iter().enumerate() {
                        {
                            let source = ModelSource::ScannedDll { path: model.path.clone() };
                            let selected = current.source == source;
                            let label = model.label();
                            rsx! {
                                button {
                                    key: "found-{index}",
                                    onclick: move |_| form.write().source = source.clone(),
                                    style: "{chip_style(selected)}",
                                    "{label}"
                                }
                            }
                        }
                    }
                    if found.offers_picker() {
                        span {
                            style: "font-size:var(--text-caption);color:#e0c090;",
                            {t("nr.no_verified")}
                        }
                    }
                }
            }

            div {
                style: "display:flex;align-items:center;gap:8px;",
                input {
                    placeholder: "/path/to/nvngx_dlssnr.dll",
                    value: "{picked_text}",
                    oninput: move |event| picked.set(event.value()),
                    style: "flex:1;padding:7px 10px;border-radius:8px;border:1px solid #2c2c38;background:#14141b;color:var(--text);font-family:monospace;font-size:var(--text-micro);outline:none;",
                }
                button {
                    onclick: pick,
                    style: "{button_style(true)}",
                    {t("nr.use_file")}
                }
            }

            span {
                style: "font-size:var(--text-caption);color:#e0c090;line-height:1.5;",
                "{DO_NOT_SHARE}"
            }

            div {
                style: "display:flex;align-items:center;gap:8px;",
                span { style: "font-size:var(--text-caption);color:var(--text-muted);", {t("nr.package")} }
                input {
                    value: "{package_text}",
                    oninput: move |event| package.set(event.value()),
                    style: "flex:1;padding:7px 10px;border-radius:8px;border:1px solid #2c2c38;background:#14141b;color:var(--text-secondary);font-family:monospace;font-size:var(--text-micro);outline:none;",
                }
                button {
                    onclick: install,
                    style: "{button_style(true)}",
                    {t("nr.install")}
                }
            }

            div {
                style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                button {
                    onclick: uninstall,
                    style: "{button_style(true)}",
                    {t("nr.uninstall")}
                }
                span {
                    style: "font-size:var(--text-micro);color:var(--text-muted);",
                    {t("nr.uninstall_hint")}
                }
            }

            if !status_text.is_empty() {
                div {
                    style: "padding:8px 12px;border-radius:8px;background:#1e2430;color:#9ab0d8;font-size:var(--text-micro);overflow-wrap:anywhere;",
                    "{status_text}"
                }
            }

            if let Some(sheet) = sheet {
                div {
                    style: "position:fixed;inset:0;z-index:30;display:flex;align-items:center;justify-content:center;background:rgba(8,8,12,0.72);",

                    div {
                        style: "width:680px;max-width:92vw;display:flex;flex-direction:column;gap:12px;padding:18px 20px;border-radius:14px;background:#16161e;border:1px solid #2c2c38;box-shadow:0 24px 60px rgba(0,0,0,0.5);",

                        div {
                            style: "display:flex;align-items:center;gap:10px;",
                            span {
                                style: "font-size:var(--text-heading);font-weight:600;color:#ffffff;",
                                "{sheet.title()}"
                            }
                            span {
                                style: "padding:2px 8px;border-radius:999px;font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;background:#2c3a56;color:var(--text);",
                                "{sheet_status(sheet.status())}"
                            }
                            button {
                                onclick: move |_| close_sheet(terminal, inputs, typed),
                                style: "{button_style(true)}",
                                "{sheet_close_label}"
                            }
                        }

                        div {
                            style: "height:240px;overflow-y:auto;display:flex;flex-direction:column-reverse;gap:3px;padding:10px 12px;border-radius:8px;background:#0d0d13;border:1px solid #26262f;font-family:monospace;font-size:var(--text-micro);line-height:1.5;color:var(--text-secondary);user-select:text;",
                            for (index, line) in sheet.lines().iter().rev().enumerate() {
                                div {
                                    key: "{index}",
                                    style: "white-space:pre-wrap;overflow-wrap:anywhere;",
                                    "{line}"
                                }
                            }
                        }

                        if let Some(hint) = sheet.prompt() {
                            span {
                                style: "font-size:var(--text-caption);color:#e0c090;line-height:1.5;",
                                "{hint}"
                            }
                        }

                        if sheet.running() {
                            div {
                                style: "display:flex;align-items:center;gap:8px;",
                                input {
                                    value: "{typed_text}",
                                    oninput: move |event| typed.set(event.value()),
                                    onkeydown: move |event| {
                                        if event.key() == Key::Enter {
                                            submit_line(inputs, typed);
                                        }
                                    },
                                    style: "flex:1;padding:8px 11px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text);font-family:monospace;font-size:var(--text-caption);outline:none;",
                                }
                                button {
                                    onclick: move |_| submit_line(inputs, typed),
                                    style: "{button_style(true)}",
                                    {t("nr.send")}
                                }
                            }
                        }

                        if let Some(log) = sheet.log() {
                            span {
                                style: "font-size:var(--text-micro);color:var(--text-muted);overflow-wrap:anywhere;",
                                {t("nr.transcript").replace("{path}", &log.display().to_string())}
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Send the typed line to the running installer, if there is one.
fn submit_line(inputs: Signal<Option<TerminalInputs>>, mut typed: Signal<String>) {
    let Some(handle) = (*inputs.read()).clone() else {
        return;
    };
    let text = typed.read().trim().to_string();
    if text.is_empty() {
        return;
    }
    if handle.answer(&text) {
        typed.set(String::new());
    }
}

/// Cancel a running install, or close a finished sheet.
fn close_sheet(
    mut terminal: Signal<Option<Terminal>>,
    mut inputs: Signal<Option<TerminalInputs>>,
    mut typed: Signal<String>,
) {
    let running = terminal
        .read()
        .as_ref()
        .is_some_and(|sheet| sheet.running());
    if running {
        if let Some(handle) = inputs.read().as_ref() {
            handle.cancel();
        }
    } else {
        inputs.set(None);
        terminal.set(None);
        typed.set(String::new());
    }
}

/// The default package directory for a resolved plan.
fn default_package_path(plan: &NrPlan) -> std::path::PathBuf {
    let root = paths::data_dir().unwrap_or_default().join("mods");
    nr_install::package_dir(&root, plan.backend, &plan.pin.version.to_string())
}

const fn model_choices() -> [(NrModel, &'static str); 4] {
    [
        (NrModel::CcSmallFp8, "CC small FP8"),
        (NrModel::CcSilverAardwold, "CC Silver Aardwold"),
        (NrModel::Int4Mixed, "int4 mixed"),
        (NrModel::SmallFp8Experimental, "small FP8 (experimental)"),
    ]
}

const fn processing_choices() -> [(NrProcessing, &'static str); 2] {
    [
        (NrProcessing::RenoDx, "RenoDX"),
        (NrProcessing::Wilsjo2, "wilsjo2"),
    ]
}

fn sheet_status(status: &TerminalStatus) -> &'static str {
    match status {
        TerminalStatus::Running => t("nr.status_running"),
        TerminalStatus::Succeeded => t("nr.status_installed"),
        TerminalStatus::Failed(_) => t("nr.status_failed"),
        TerminalStatus::Cancelled => t("nr.status_cancelled"),
    }
}

fn chip_style(selected: bool) -> &'static str {
    if selected {
        "padding:4px 10px;border-radius:7px;border:1px solid #3f5c8c;background:#243044;color:#ffffff;font-size:var(--text-micro);cursor:pointer;min-height:var(--hit-min);"
    } else {
        "padding:4px 10px;border-radius:7px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-micro);cursor:pointer;min-height:var(--hit-min);"
    }
}

fn button_style(enabled: bool) -> &'static str {
    if enabled {
        "padding:6px 12px;border-radius:8px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:var(--text-micro);cursor:pointer;min-height:var(--hit-min);min-width:var(--hit-min);"
    } else {
        "padding:6px 12px;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:var(--text-disabled);font-size:var(--text-micro);cursor:default;"
    }
}
