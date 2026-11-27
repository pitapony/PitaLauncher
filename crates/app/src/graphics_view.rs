//! The Graphics screen (`View::Graphics`): the M3 per-game graphics profile.
//!
//! The screen is the one place a user picks an upscaler, a frame-gen mode, the
//! injection owner and the diagnostics. It renders both shells from one
//! component (the design tokens scale it), and it is reachable from the console
//! game page and the desktop detail page alike.
//!
//! Every rule it obeys lives in the framework-free [`crate::graphics`] model: the
//! rows are the capability matrix's available options (the rest are listed with
//! their reason), the edit stack owns apply/revert/undo/reset, and the resolved
//! command comes from [`crate::runtime::graphics_preview`] — the same
//! composition a launch uses. This file only draws that model and routes the
//! controller/keyboard actions into it.

use dioxus::prelude::*;

use pitagames_core::AntiCheat;
use pitagames_core::profile::Profile;
use pitagames_core::ui::Shell;
use pitagames_input::Action;
use pitagames_platform::compat::{CompatDb, CompatEntry};
use pitagames_platform::envc::GraphicsContext;
use pitagames_platform::gpu::{self, GpuProbe, GpuProfile};
use pitagames_platform::proton::DiscoveryReport;

use crate::View;
use crate::copy::CopyButton;
use crate::detail::DbHandle;
use crate::focus::{self, ActionChannel};
use crate::graphics::{self, Control, DurableRevert, GraphicsEditor, OptionRow, Section};
use crate::library::GameEntry;
use crate::nr_install_view::NrInstallPanel;
use crate::persist;
use crate::runtime;
use crate::shell;

#[derive(Props, Clone, PartialEq)]
pub struct GraphicsProps {
    /// The game the profile belongs to.
    pub entry: GameEntry,
    /// The profile store.
    pub db: DbHandle,
    /// Proton builds discovered on this machine.
    pub proton: Option<DiscoveryReport>,
    /// The app's one input pump posts this screen's actions here.
    pub actions: ActionChannel,
    /// Which shell the window is in.
    pub shell: Signal<Shell>,
}

/// The graphics screen for one game.
#[allow(non_snake_case)]
pub fn Graphics(props: GraphicsProps) -> Element {
    let entry = props.entry.clone();
    let db = props.db.0.clone();
    let report = props.proton.clone();
    // The channel is cloned up front: two `use_future` closures both need it,
    // and a `move` closure would otherwise move the one `props.actions` twice.
    let actions = props.actions.clone();
    let mut session = crate::context::use_session();
    let screen_focus = focus::use_focus();

    let mut editor = use_signal(|| None::<GraphicsEditor>);
    let mut gpu = use_signal(|| None::<GpuProfile>);
    let mut compat = use_signal(|| None::<CompatEntry>);
    // The effective anti-cheat (detection merged with curation) and the user's
    // acknowledgement of the injection risk, which the gate reads.
    let mut anticheat = use_signal(|| AntiCheat::Unknown);
    let mut acknowledged = use_signal(|| false);
    let mut manifest = use_signal(|| None::<DurableRevert>);
    let mut status = use_signal(String::new);

    // Read-only detection and the game's profile, once per mounted game. The
    // parent keys this component by game, so opening another remounts it.
    let load_db = db.clone();
    let load_id = entry.game.id.clone();
    let load_install = entry.game.install_dir.clone();
    use_future(move || {
        let db = load_db.clone();
        let id = load_id.clone();
        let install = load_install.clone();
        async move {
            gpu.set(Some(gpu::detect(&GpuProbe::host())));
            let db_compat = CompatDb::load();
            let curated = db_compat
                .get(&id)
                .map(|entry| entry.anticheat)
                .unwrap_or(AntiCheat::Unknown);
            compat.set(db_compat.get(&id).cloned());
            // The injection gate reads the effective anti-cheat: a real on-disk
            // marker wins, otherwise the compat DB's curated value.
            let scan = install
                .as_deref()
                .map(pitagames_platform::anticheat::detect)
                .unwrap_or_default();
            anticheat.set(pitagames_platform::anticheat::effective(curated, &scan).kind);
            match install.as_deref().map(graphics::durable_revert) {
                Some(Ok(Some(summary))) => manifest.set(Some(summary)),
                Some(Ok(None)) | None => manifest.set(None),
                Some(Err(message)) => status.set(message),
            }
            let profile = match db.as_ref() {
                Some(db) => match persist::profiles(db, &id).await {
                    Ok(rows) => rows.into_iter().next().map(|row| row.profile),
                    Err(message) => {
                        status.set(message);
                        None
                    }
                },
                None => None,
            };
            editor.set(Some(GraphicsEditor::new(
                profile.unwrap_or_else(|| Profile::new(id.clone(), "Default")),
            )));
        }
    });

    // The screen's action drain: the app's one pump posts here; this screen owns
    // its own focus graph. The live control list is recomputed on every action,
    // so a press never acts on a stale ring.
    let drain_entry = entry.clone();
    let drain_report = report.clone();
    let drain_db = db.clone();
    use_future(move || {
        let channel = actions.clone();
        let entry = drain_entry.clone();
        let report = drain_report.clone();
        let db = drain_db.clone();
        async move {
            while let Some(action) = channel.next().await {
                if session.read().view() != View::Graphics {
                    continue;
                }
                if action == Action::Back {
                    session.write().set_view(View::Detail);
                    continue;
                }

                let (controls, rows) = {
                    let editor_guard = editor.read();
                    let Some(editor) = editor_guard.as_ref() else {
                        continue;
                    };
                    let gpu_guard = gpu.read();
                    let Some(gpu) = gpu_guard.as_ref() else {
                        continue;
                    };
                    let compat_guard = compat.read();
                    let proton = report
                        .as_ref()
                        .and_then(|report| report.resolve(&editor.draft().proton).ok().flatten());
                    let rows =
                        graphics::rows(editor.draft(), gpu, compat_guard.as_ref(), proton.as_ref());
                    let has_manifest = manifest.read().is_some();
                    let controls = graphics::controls(
                        rows.len(),
                        editor.can_undo(),
                        editor.is_dirty(),
                        has_manifest,
                    );
                    (controls, rows)
                };

                if action.is_navigation() {
                    screen_focus.move_focus(action, controls.len(), 1);
                    continue;
                }
                if action != Action::Confirm {
                    continue;
                }
                let Some(control) = screen_focus
                    .index()
                    .and_then(|index| controls.get(index).copied())
                else {
                    continue;
                };

                match control {
                    Control::Row(index) => {
                        if let Some(choice) = rows
                            .get(index)
                            .filter(|row| row.is_available())
                            .map(|row| row.choice.clone())
                            && let Some(editor) = editor.write().as_mut()
                        {
                            editor.press(&choice);
                            status.set(String::new());
                        }
                    }
                    Control::Apply => {
                        // The injection gate, enforced at the controller seam
                        // too: a protected game cannot be applied unacknowledged.
                        let allows = {
                            let guard = editor.read();
                            guard.as_ref().is_some_and(|editor| {
                                graphics::injection_gate(
                                    editor.draft(),
                                    *anticheat.read(),
                                    *acknowledged.read(),
                                )
                                .allows()
                            })
                        };
                        if !allows {
                            status.set(
                                "Acknowledge the anti-cheat warning before applying.".to_string(),
                            );
                            continue;
                        }
                        // The write guard must be released before the arm reads
                        // the editor back (a live guard makes the read panic), so
                        // the outcome is computed in its own scope first.
                        let applied = { editor.write().as_mut().map(GraphicsEditor::apply) };
                        match applied {
                            Some(Ok(())) => {
                                let profile = editor.read().as_ref().map(|e| e.applied().clone());
                                if let (Some(profile), Some(report), Some(gpu)) =
                                    (profile, report.clone(), gpu.read().clone())
                                {
                                    let compat = compat.read().clone();
                                    let db = db.clone();
                                    let status = status;
                                    spawn(async move {
                                        save_and_write(db, profile, report, gpu, compat, status)
                                            .await;
                                    });
                                }
                            }
                            Some(Err(message)) => status.set(message),
                            None => {}
                        }
                    }
                    Control::Revert => {
                        if let Some(editor) = editor.write().as_mut() {
                            editor.revert();
                        }
                        status.set(String::new());
                    }
                    Control::Undo => {
                        if let Some(editor) = editor.write().as_mut() {
                            editor.undo();
                        }
                        status.set(String::new());
                    }
                    Control::Reset => {
                        if let Some(editor) = editor.write().as_mut() {
                            editor.reset_to_safe_defaults();
                        }
                        status.set(String::new());
                    }
                    Control::Restore => {
                        if let Some(dir) = entry.game.install_dir.clone() {
                            let mut status = status;
                            let mut manifest = manifest;
                            spawn(async move {
                                match pitagames_mods::ModManifest::restore(&dir) {
                                    Ok(report) => {
                                        let tree = if report.tree_hash_ok {
                                            ", the tree matches its pre-install hash"
                                        } else {
                                            ""
                                        };
                                        status.set(format!(
                                            "Restored {} ({} file(s) back{tree}).",
                                            report.mod_name,
                                            report.restored.len()
                                        ));
                                        manifest.set(None);
                                    }
                                    Err(error) => status.set(error.to_string()),
                                }
                            });
                        }
                    }
                }
            }
        }
    });

    let editor_snapshot = editor.read().clone();
    let Some(editor_state) = editor_snapshot.as_ref() else {
        return rsx! {
            div {
                style: "padding:32px;color:var(--text-muted);",
                "Loading graphics settings…"
            }
        };
    };
    let gpu_snapshot = gpu.read().clone();
    let compat_snapshot = compat.read().clone();

    let (rows, hidden, preview_command, preview_error) =
        match (gpu_snapshot.as_ref(), report.as_ref()) {
            (Some(gpu), Some(report)) => {
                let draft = editor_state.draft();
                let proton = report.resolve(&draft.proton).ok().flatten();
                let rows = graphics::rows(draft, gpu, compat_snapshot.as_ref(), proton.as_ref());
                let hidden =
                    graphics::hidden(draft, gpu, compat_snapshot.as_ref(), proton.as_ref());
                let (command, error) = match runtime::graphics_preview(
                    &entry.game,
                    entry.app_id(),
                    draft,
                    report,
                    GraphicsContext::new(gpu, compat_snapshot.as_ref()),
                ) {
                    Ok(preview) => (preview.command, None),
                    Err(message) => (String::new(), Some(message)),
                };
                (rows, hidden, command, error)
            }
            _ => (
                Vec::new(),
                Vec::new(),
                String::new(),
                Some("Waiting for the GPU and Proton discovery…".to_string()),
            ),
        };

    let manifest_snapshot = manifest.read().clone();
    let controls = graphics::controls(
        rows.len(),
        editor_state.can_undo(),
        editor_state.is_dirty(),
        manifest_snapshot.is_some(),
    );
    let focused = screen_focus
        .index()
        .and_then(|index| controls.get(index).copied());

    // The neural-rendering backend verdict, for the install panel. It is the
    // same `platform::nr::plan` the CLI uses, so the panel and the diagnostic
    // cannot disagree about whether NR is offered.
    let nr_plan = match (gpu_snapshot.as_ref(), report.as_ref()) {
        (Some(gpu), Some(report)) => {
            let draft = editor_state.draft();
            let proton = report.resolve(&draft.proton).ok().flatten();
            let registry = pitagames_platform::nr::BackendRegistry::effective(
                pitagames_platform::nr::default_user_path().as_deref(),
            )
            .unwrap_or_default();
            let game_64bit = entry
                .game
                .exe
                .as_deref()
                .and_then(|exe| pitagames_platform::nr::pe_is_64_bit(exe).ok());
            pitagames_platform::nr::plan(pitagames_platform::nr::NrRequest {
                gpu,
                profile: draft,
                registry: &registry,
                compat: compat_snapshot.as_ref(),
                proton: proton.as_ref(),
                game_64bit,
            })
            .plan()
            .cloned()
        }
        _ => None,
    };

    // Group the rows by section so the screen draws the four headings in order.
    let mut sections: Vec<(Section, Vec<(usize, OptionRow)>)> = Section::ALL
        .iter()
        .map(|section| (*section, Vec::new()))
        .collect();
    for (index, row) in rows.iter().enumerate() {
        if let Some(slot) = sections
            .iter_mut()
            .find(|(section, _)| *section == row.choice.section())
        {
            slot.1.push((index, row.clone()));
        }
    }

    let title = entry.title().to_string();
    let profile_name = editor_state.draft().name.clone();
    let dirty = editor_state.is_dirty();
    let status_text = status.read().clone();
    let console = props.shell.read().is_console();
    // The injection gate: a draft that injects into an anti-cheat-protected
    // game cannot be applied until the risk is acknowledged.
    let gate = graphics::injection_gate(
        editor_state.draft(),
        *anticheat.read(),
        *acknowledged.read(),
    );
    let gate_allows = gate.allows();
    let gate_message = gate.message();

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:16px;padding:0 32px 40px;",

            div {
                style: "display:flex;align-items:baseline;gap:12px;flex-wrap:wrap;",
                h2 {
                    style: "margin:0;font-size:var(--text-title);font-weight:600;color:#ffffff;",
                    "Graphics"
                }
                span {
                    style: "font-size:var(--text-body);color:var(--text-secondary);",
                    "{title}"
                }
                span {
                    style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--text-muted);",
                    "profile {profile_name}"
                }
                if dirty {
                    span {
                        style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--warning);",
                        "unsaved"
                    }
                }
            }

            for (section, section_rows) in sections {
                div {
                    key: "{section:?}",
                    style: "display:flex;flex-direction:column;gap:8px;padding-top:6px;",
                    div {
                        style: "display:flex;flex-direction:column;gap:2px;",
                        h3 {
                            style: "margin:0;font-size:var(--text-heading);font-weight:600;color:#ffffff;",
                            "{section.label()}"
                        }
                        span {
                            style: "font-size:var(--text-micro);color:var(--text-muted);",
                            "{section.hint()}"
                        }
                    }
                    div {
                        style: "display:flex;flex-direction:column;gap:6px;",
                        for (index, row) in section_rows {
                            {
                                let focused_row = focused == Some(Control::Row(index));
                                let ring = focus::ring_style(focused_row);
                                let selected = row.selected;
                                let available = row.is_available();
                                let reason = row.reason().map(str::to_string);
                                let label = row.choice.label();
                                let choice = row.choice.clone();
                                let style = option_style(selected, available, ring);
                                rsx! {
                                    button {
                                        key: "row-{index}",
                                        disabled: !available,
                                        onclick: move |_| {
                                            if let Some(editor) = editor.write().as_mut() {
                                                editor.press(&choice);
                                            }
                                            status.set(String::new());
                                        },
                                        style: "{style}",
                                        "{label}"
                                        if selected { "  ✓" }
                                    }
                                    if let Some(reason) = reason {
                                        span {
                                            key: "reason-{index}",
                                            style: "font-size:var(--text-micro);color:var(--warning);",
                                            "not available on this machine: {reason}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !hidden.is_empty() {
                div {
                    style: "display:flex;flex-direction:column;gap:4px;padding:12px 14px;border-radius:10px;background:#1a1a22;border:1px solid #26262f;",
                    span {
                        style: "font-size:var(--text-micro);text-transform:uppercase;letter-spacing:0.06em;color:var(--text-muted);",
                        "Not offered here"
                    }
                    for (index, item) in hidden.iter().enumerate() {
                        span {
                            key: "hidden-{index}",
                            style: "font-size:var(--text-caption);color:var(--text-muted);",
                            "{item.label} — {item.reason}"
                        }
                    }
                }
            }

            if let Some(plan) = nr_plan {
                NrInstallPanel {
                    entry: entry.clone(),
                    plan,
                }
            }

            div {
                style: "display:flex;flex-direction:column;gap:6px;padding-top:6px;",
                div {
                    style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;",
                    span {
                        style: "font-size:var(--text-caption);font-weight:600;color:var(--text);",
                        "Resolved command"
                    }
                    CopyButton { text: preview_command.clone() }
                    span {
                        style: "font-size:var(--text-micro);color:var(--text-muted);",
                        if console { "what the launcher will run" } else { "the same line the detail page's Command panel shows" }
                    }
                }
                if let Some(message) = preview_error.clone() {
                    span {
                        style: "padding:8px 12px;border-radius:8px;background:#3a2a1e;color:#e0c090;font-size:var(--text-caption);overflow-wrap:anywhere;",
                        "{message}"
                    }
                } else {
                    code {
                        style: "display:block;padding:10px 12px;border-radius:8px;background:#14141b;border:1px solid #26262f;color:#b8c8e8;font-family:monospace;font-size:var(--text-micro);overflow-wrap:anywhere;white-space:pre-wrap;user-select:text;",
                        "{preview_command}"
                    }
                }
            }

            if let Some(message) = gate_message.clone() {
                div {
                    style: "display:flex;flex-direction:column;gap:8px;padding:12px 14px;border-radius:10px;background:#3a2a1e;color:#e8c890;font-size:var(--text-caption);",
                    span {
                        style: "font-weight:600;",
                        "Anti-cheat injection warning"
                    }
                    span { "{message}" }
                    label {
                        style: "display:flex;align-items:center;gap:8px;color:var(--text);",
                        input {
                            r#type: "checkbox",
                            checked: *acknowledged.read(),
                            onchange: move |event| acknowledged.set(event.checked()),
                            style: "min-height:var(--hit-min);min-width:var(--hit-min);",
                        }
                        "I understand the risk and want to inject anyway."
                    }
                }
            }

            div {
                style: "display:flex;gap:8px;flex-wrap:wrap;align-items:center;",
                button {
                    disabled: !dirty || !gate_allows,
                    onclick: move |_| {
                        if !gate_allows {
                            return;
                        }
                        if let Some(editor) = editor.write().as_mut()
                            && editor.apply().is_err()
                        {
                            return;
                        }
                        let profile = editor.read().as_ref().map(|e| e.applied().clone());
                        if let (Some(profile), Some(report), Some(gpu)) =
                            (profile, report.clone(), gpu.read().clone())
                        {
                            let compat = compat.read().clone();
                            let db = db.clone();
                            let status = status;
                            spawn(async move {
                                save_and_write(db, profile, report, gpu, compat, status).await;
                            });
                        }
                    },
                    style: "{action_button_style(focused == Some(Control::Apply), dirty && gate_allows, \"#2e3a56\", \"#ffffff\")}",
                    "Apply & verify"
                }
                if dirty {
                    button {
                        onclick: move |_| {
                            if let Some(editor) = editor.write().as_mut() {
                                editor.revert();
                            }
                            status.set(String::new());
                        },
                        style: "{action_button_style(focused == Some(Control::Revert), true, \"#2a1e1e\", \"#e0a0a0\")}",
                        "Revert"
                    }
                }
                if editor_state.can_undo() {
                    button {
                        onclick: move |_| {
                            if let Some(editor) = editor.write().as_mut() {
                                editor.undo();
                            }
                            status.set(String::new());
                        },
                        style: "{action_button_style(focused == Some(Control::Undo), true, \"#1e1e28\", \"var(--text-secondary)\")}",
                        "Undo"
                    }
                }
                button {
                    onclick: move |_| {
                        if let Some(editor) = editor.write().as_mut() {
                            editor.reset_to_safe_defaults();
                        }
                        status.set(String::new());
                    },
                    style: "{action_button_style(focused == Some(Control::Reset), true, \"#1e1e28\", \"var(--text-secondary)\")}",
                    "Reset to safe defaults"
                }
                if manifest_snapshot.is_some() {
                    button {
                        onclick: move |_| {
                            if let Some(dir) = entry.game.install_dir.clone() {
                                let mut status = status;
                                let mut manifest = manifest;
                                spawn(async move {
                                    match pitagames_mods::ModManifest::restore(&dir) {
                                        Ok(report) => {
                                            status.set(format!(
                                                "Restored {} ({} file(s) back).",
                                                report.mod_name,
                                                report.restored.len()
                                            ));
                                            manifest.set(None);
                                        }
                                        Err(error) => status.set(error.to_string()),
                                    }
                                });
                            }
                        },
                        style: "{action_button_style(focused == Some(Control::Restore), true, \"#2a1e1e\", \"#e0a0a0\")}",
                        "Restore installed files"
                    }
                }
            }

            if let Some(revert) = manifest_snapshot {
                div {
                    style: "font-size:var(--text-micro);color:var(--text-muted);",
                    "Installed mod: {revert.mod_name} ({revert.files} file(s)). The durable revert is the mod manifest; restoring returns the game directory to its pre-install state."
                }
            }

            if !status_text.is_empty() {
                div {
                    style: "padding:10px 14px;border-radius:8px;background:#1e2430;color:#9ab0d8;font-size:var(--text-caption);overflow-wrap:anywhere;",
                    "{status_text}"
                }
            }
        }
    }
}

/// Apply the draft, persist the profile and write its shim file.
///
/// The write goes through the graphics-aware path, so a profile that asks for a
/// graphics field compiles against the detected GPU and the game's entry rather
/// than being refused.
async fn save_and_write(
    db: Option<pitagames_db::Db>,
    profile: Profile,
    report: DiscoveryReport,
    gpu: GpuProfile,
    compat: Option<CompatEntry>,
    mut status: Signal<String>,
) {
    if let Some(db) = db
        && let Err(message) = persist::save_profile(&db, &profile).await
    {
        status.set(message);
        return;
    }

    let graphics = GraphicsContext::new(&gpu, compat.as_ref());
    match runtime::write_shim_profile_with_graphics(&profile, &report, graphics) {
        Ok((path, compiled)) => status.set(format!(
            "Applied · wrote {} · {} environment entr{}",
            path.display(),
            compiled.env.len(),
            if compiled.env.len() == 1 { "y" } else { "ies" }
        )),
        Err(message) => status.set(message),
    }
}

/// The option button's inline style: a selected row reads as the accent, an
/// unavailable one is muted, and the focus ring rides on top.
fn option_style(selected: bool, available: bool, ring: &str) -> String {
    let (border, background, color) = if !available {
        ("#26262f", "#1a1a22", "var(--text-disabled)")
    } else if selected {
        ("#3f5c8c", "#243044", "#ffffff")
    } else {
        ("#2c2c38", "#1e1e28", "var(--text)")
    };
    format!(
        "text-align:left;padding:8px 14px;border-radius:8px;border:1px solid {border};background:{background};color:{color};font-size:var(--text-body);cursor:pointer;{} {ring}",
        shell::HIT_FLOOR
    )
}

/// An action button's inline style, with its focus ring.
fn action_button_style(focused: bool, enabled: bool, background: &str, color: &str) -> String {
    let ring = focus::ring_style(focused);
    if !enabled {
        return format!(
            "padding:8px 16px;border-radius:8px;border:1px solid #26262f;background:#1a1a22;color:var(--text-disabled);font-size:var(--text-body);cursor:default;{ring}"
        );
    }
    format!(
        "padding:8px 16px;border-radius:8px;border:1px solid #3a3a4c;background:{background};color:{color};font-size:var(--text-body);cursor:pointer;{} {ring}",
        shell::HIT_FLOOR
    )
}
