//! The Settings screen (`View::Settings`): the launcher's preferences, composed
//! from the pieces that already own them.
//!
//! There is deliberately **no second store** here. Every control reads and
//! writes the same `config.toml` the shell, the supervisor and the CLI read
//! (`pitagames_core::ui::AppConfig`), the same compat database
//! (`platform::compat`), the same Steam writer (`runtime::restore`) and the same
//! input maps (the Controls screen, linked to rather than copied). The screen is
//! a composition, not a reimplementation.
//!
//! The model is framework-free: [`Control`] is the closed set of focusable
//! controls, [`apply`] turns one into a mutation of [`AppConfig`] plus an
//! [`Intent`] the component carries out (switch the live shell, run a sync,
//! restore Steam, open Controls). A headless test asserts every control writes
//! the field it names and that the built [`FocusGraph`] is trap-free.

use dioxus::prelude::*;
use pitagames_core::i18n::t;
use pitagames_core::paths;
use pitagames_core::ui::{
    AppConfig, MAX_SAFE_AREA_PCT, MAX_UI_SCALE, MIN_UI_SCALE, Shell, StartMode,
};
use pitagames_input::{Action, Band, FocusGraph};
use pitagames_platform::proton::DiscoveryReport;

use crate::View;
use crate::focus::{self, ActionChannel};
use crate::library::Library;
use crate::session::Session;
use crate::shell;

/// The step a `−`/`+` press applies to the UI scale.
pub const UI_SCALE_STEP: f32 = 0.25;
/// The step a `−`/`+` press applies to the console overscan margin.
pub const SAFE_AREA_STEP: f32 = 0.5;
/// The pointer-hide delays a `−`/`+` press cycles, in milliseconds; `0` never
/// hides the pointer.
pub const CURSOR_OPTIONS_MS: [u64; 4] = [0, 1_500, 3_000, 5_000];

/// One focusable Settings control, in reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Pick the live shell: the two themes are the two shells' palettes.
    Shell(Shell),
    /// Pick the shell that opens at startup.
    StartMode(StartMode),
    /// Step the UI scale by `direction * UI_SCALE_STEP`.
    UiScale(i32),
    /// Step the overscan margin by `direction * SAFE_AREA_STEP`.
    SafeArea(i32),
    /// Cycle the pointer-hide delay.
    Cursor(i32),
    /// Toggle reduced motion (the accessibility switch).
    ReduceAnimations,
    /// Toggle fullscreen for the console shell.
    ConsoleFullscreen,
    /// Toggle safe mode.
    SafeMode,
    /// Toggle the compat-DB sync.
    CompatSync,
    /// Copy the shipped compat database into the data directory now.
    CompatSyncNow,
    /// Restore the launcher's Steam configuration changes.
    RestoreSteam,
    /// Leave to the Controls screen for remapping.
    OpenControls,
}

/// What the component must do after a control mutated the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// The config changed and must be persisted.
    Save,
    /// The config changed; persist it and run the compat sync now.
    SaveAndSync,
    /// The config did not change; run the compat sync now.
    Sync,
    /// Switch the live shell (the shell state persists `last_shell` itself).
    SwitchShell(Shell),
    /// Restore the launcher's Steam changes now.
    RestoreSteam,
    /// Leave to the Controls screen.
    OpenControls,
}

/// The controls, grouped into the rows the screen draws.
///
/// One row per visual line; each row becomes one [`Band`] so Left/Right walk a
/// row and Up/Down walk the sections. The grouping is the single source of the
/// cell order, so [`cells`] and [`bands`] cannot disagree.
fn rows() -> Vec<Vec<Control>> {
    vec![
        vec![
            Control::Shell(Shell::Desktop),
            Control::Shell(Shell::Console),
        ],
        vec![
            Control::StartMode(StartMode::Desktop),
            Control::StartMode(StartMode::Console),
            Control::StartMode(StartMode::Remember),
        ],
        vec![Control::UiScale(-1), Control::UiScale(1)],
        vec![Control::SafeArea(-1), Control::SafeArea(1)],
        vec![Control::Cursor(-1), Control::Cursor(1)],
        vec![
            Control::ReduceAnimations,
            Control::ConsoleFullscreen,
            Control::SafeMode,
        ],
        vec![Control::CompatSync, Control::CompatSyncNow],
        vec![Control::RestoreSteam],
        vec![Control::OpenControls],
    ]
}

/// Every focusable control, in reading order (the flat cell ids).
pub fn cells() -> Vec<Control> {
    rows().into_iter().flatten().collect()
}

/// The focus graph's bands, one per row.
pub fn bands() -> Vec<Band> {
    rows()
        .iter()
        .map(|row| Band::new(row.len(), row.len()))
        .collect()
}

/// The trap-free focus graph the screen walks.
pub fn graph() -> FocusGraph {
    FocusGraph::ragged(&bands())
}

/// The cell id of one control: its position in [`cells`].
fn index_of(control: Control) -> usize {
    cells()
        .iter()
        .position(|cell| *cell == control)
        .unwrap_or(0)
}

/// Whether a chip is the currently selected one.
pub fn selected(control: Control, config: &AppConfig, live_shell: Shell) -> bool {
    match control {
        Control::Shell(shell) => shell == live_shell,
        Control::StartMode(mode) => config.ui.start_mode == mode,
        _ => false,
    }
}

/// A toggle's current state, or `None` when the control is not a toggle.
pub fn toggle_on(control: Control, config: &AppConfig) -> Option<bool> {
    match control {
        Control::ReduceAnimations => Some(config.ui.display.reduce_animations),
        Control::ConsoleFullscreen => Some(config.ui.display.console_fullscreen),
        Control::SafeMode => Some(config.ui.safe_mode),
        Control::CompatSync => Some(config.compat.sync),
        _ => None,
    }
}

/// A toggle's state for the screen, reading through [`toggle_on`] so the drawn
/// state and the tested state are the same one rule.
fn is_on(control: Control, config: &AppConfig) -> bool {
    toggle_on(control, config).unwrap_or(false)
}

/// Apply one control to the config, returning what the component must do next.
pub fn apply(control: Control, config: &mut AppConfig) -> Intent {
    match control {
        // The live shell is not a config field; the shell state remembers it.
        Control::Shell(shell) => Intent::SwitchShell(shell),
        Control::StartMode(mode) => {
            config.ui.start_mode = mode;
            Intent::Save
        }
        Control::UiScale(direction) => {
            step_ui_scale(config, direction);
            Intent::Save
        }
        Control::SafeArea(direction) => {
            step_safe_area(config, direction);
            Intent::Save
        }
        Control::Cursor(direction) => {
            cycle_cursor(config, direction);
            Intent::Save
        }
        Control::ReduceAnimations => {
            config.ui.display.reduce_animations = !config.ui.display.reduce_animations;
            Intent::Save
        }
        Control::ConsoleFullscreen => {
            config.ui.display.console_fullscreen = !config.ui.display.console_fullscreen;
            Intent::Save
        }
        Control::SafeMode => {
            config.ui.safe_mode = !config.ui.safe_mode;
            Intent::Save
        }
        Control::CompatSync => {
            config.compat.sync = !config.compat.sync;
            // Turning the sync on copies the database right away; turning it
            // off only stops future copies, so the config write is enough.
            if config.compat.sync {
                Intent::SaveAndSync
            } else {
                Intent::Save
            }
        }
        Control::CompatSyncNow => Intent::Sync,
        Control::RestoreSteam => Intent::RestoreSteam,
        Control::OpenControls => Intent::OpenControls,
    }
}

/// Step the UI scale, clamped to the accepted range.
fn step_ui_scale(config: &mut AppConfig, direction: i32) {
    let next = config.ui.display.ui_scale + direction as f32 * UI_SCALE_STEP;
    config.ui.display.ui_scale = next.clamp(MIN_UI_SCALE, MAX_UI_SCALE);
}

/// Step the overscan margin, clamped to the accepted range.
fn step_safe_area(config: &mut AppConfig, direction: i32) {
    let next = config.ui.display.safe_area_pct + direction as f32 * SAFE_AREA_STEP;
    config.ui.display.safe_area_pct = next.clamp(0.0, MAX_SAFE_AREA_PCT);
}

/// Cycle the pointer-hide delay through the offered options.
fn cycle_cursor(config: &mut AppConfig, direction: i32) {
    let current = config.ui.display.hide_cursor_ms;
    // A value a hand-edited file set that is not one of ours starts from the
    // default's position, so `+` always advances to a defined option.
    let index = CURSOR_OPTIONS_MS
        .iter()
        .position(|option| *option == current)
        .unwrap_or(2);
    let len = CURSOR_OPTIONS_MS.len() as i32;
    let next = (index as i32 + direction).rem_euclid(len) as usize;
    config.ui.display.hide_cursor_ms = CURSOR_OPTIONS_MS[next];
}

/// The cursor-hide delay as a short label.
fn cursor_label(ms: u64) -> String {
    if ms == 0 {
        t("settings.cursor_never").to_string()
    } else {
        format!("{:.1}s", ms as f32 / 1000.0)
    }
}

/// `On`/`Off` for a toggle chip.
fn on_off(on: bool) -> &'static str {
    if on {
        t("settings.on")
    } else {
        t("settings.off")
    }
}

// ---------------------------------------------------------------------------
// The Dioxus seam
// ---------------------------------------------------------------------------

/// Props for the Settings screen.
///
/// The live shell is a read signal plus its switch handler: the handler is the
/// shell state's `set`, so the settings screen persists `last_shell` exactly the
/// way the shell toggle does, instead of owning a second writer.
#[derive(Props, Clone, PartialEq)]
pub struct SettingsProps {
    /// The app's one input pump posts this screen's actions here.
    pub actions: ActionChannel,
    /// The live shell, so a chip reads the current one.
    pub shell: Signal<Shell>,
    /// Switch the live shell (and remember it).
    pub on_shell: EventHandler<Shell>,
    /// The library, for the Steam restore and the runtime paths.
    pub library: Signal<Option<Library>>,
    /// The Proton builds discovery found, shown read-only.
    pub proton: Option<DiscoveryReport>,
}

/// A section's small uppercase label.
#[derive(Props, Clone, PartialEq)]
struct SectionLabelProps {
    label: &'static str,
}

#[allow(non_snake_case)]
fn SectionLabel(props: SectionLabelProps) -> Element {
    rsx! {
        span {
            style: "font-size:var(--text-caption);text-transform:uppercase;letter-spacing:0.06em;color:var(--text-muted);",
            "{props.label}"
        }
    }
}

/// A read-only label/path line.
#[derive(Props, Clone, PartialEq)]
struct PathLineProps {
    label: &'static str,
    path: String,
}

#[allow(non_snake_case)]
fn PathLine(props: PathLineProps) -> Element {
    rsx! {
        div {
            style: "display:flex;gap:10px;font-size:var(--text-caption);",
            span {
                style: "min-width:9rem;color:var(--text-muted);",
                "{props.label}"
            }
            span {
                style: "color:var(--text-secondary);overflow-wrap:anywhere;font-family:monospace;",
                "{props.path}"
            }
        }
    }
}

/// The Settings screen.
#[allow(non_snake_case)]
pub fn SettingsView(props: SettingsProps) -> Element {
    let actions = props.actions.clone();
    let shell_signal = props.shell;
    let on_shell = props.on_shell;
    let library = props.library;
    let proton = props.proton;
    let session = crate::context::use_session();
    let screen_focus = focus::use_focus();

    // The one config this screen reads and writes. It is the same file the
    // shell, the supervisor and the CLI read; loading it here cannot drift.
    let config = use_signal(|| AppConfig::load_default().0);
    let status = use_signal(String::new);
    let status_error = use_signal(|| false);

    use_future(move || {
        let channel = actions.clone();
        async move {
            while let Some(action) = channel.next().await {
                if session.read().view() != View::Settings {
                    continue;
                }
                handle_action(
                    action,
                    screen_focus,
                    config,
                    on_shell,
                    library,
                    session,
                    status,
                    status_error,
                );
            }
        }
    });

    let live_shell = (shell_signal)();
    let current = config.read().clone();
    let focused = screen_focus.index();
    let status_text = status();
    let status_bad = status_error();
    let runtime = runtime_lines(proton.as_ref(), library.read().as_ref());
    let compat_target = paths::data_dir().map(|dir| dir.join("compat"));
    let backup_dir = paths::steam_backup_dir();

    rsx! {
        div {
            style: "display:flex;flex-direction:column;gap:18px;padding:0 32px 40px;max-width:900px;",

            div {
                style: "display:flex;flex-direction:column;gap:6px;",
                span {
                    style: "font-size:var(--text-heading);font-weight:600;color:var(--text);",
                    {t("settings.title")}
                }
                span {
                    style: "font-size:var(--text-caption);color:var(--text-muted);line-height:1.5;",
                    {t("settings.description")}
                }
            }

            // -- Theme (the live shell) -----------------------------------
            div {
                style: "display:flex;flex-direction:column;gap:8px;",
                SectionLabel { label: t("settings.theme") }
                div {
                    style: "display:flex;flex-wrap:wrap;gap:8px;",
                    for shell_choice in [Shell::Desktop, Shell::Console] {
                        button {
                            key: "theme-{shell_choice.as_str()}",
                            onclick: move |_| activate(Control::Shell(shell_choice), config, on_shell, library, session, status, status_error),
                            style: "{chip_style(selected(Control::Shell(shell_choice), &current, live_shell))}{focus::ring_style(focused == Some(index_of(Control::Shell(shell_choice))))}",
                            "{shell_choice.label()}"
                        }
                    }
                }
                span {
                    style: "{hint_style()}",
                    {t("settings.theme_hint")}
                }
            }

            // -- Start mode -----------------------------------------------
            div {
                style: "display:flex;flex-direction:column;gap:8px;",
                SectionLabel { label: t("settings.start_mode") }
                div {
                    style: "display:flex;flex-wrap:wrap;gap:8px;",
                    for mode in [StartMode::Desktop, StartMode::Console, StartMode::Remember] {
                        button {
                            key: "start-{mode.as_str()}",
                            onclick: move |_| activate(Control::StartMode(mode), config, on_shell, library, session, status, status_error),
                            style: "{chip_style(selected(Control::StartMode(mode), &current, live_shell))}{focus::ring_style(focused == Some(index_of(Control::StartMode(mode))))}",
                            "{mode.label()}"
                        }
                    }
                }
            }

            // -- Display numbers ------------------------------------------
            div {
                style: "display:flex;flex-direction:column;gap:10px;",
                SectionLabel { label: t("settings.display") }
                div {
                    style: "{stepper_style()}",
                    span {
                        style: "{stepper_label_style()}",
                        {t("settings.ui_scale")}
                    }
                    button {
                        onclick: move |_| activate(Control::UiScale(-1), config, on_shell, library, session, status, status_error),
                        style: "{step_style()}{focus::ring_style(focused == Some(index_of(Control::UiScale(-1))))}",
                        "−"
                    }
                    span {
                        style: "{stepper_value_style()}",
                        "{current.ui.display.ui_scale:.2}×"
                    }
                    button {
                        onclick: move |_| activate(Control::UiScale(1), config, on_shell, library, session, status, status_error),
                        style: "{step_style()}{focus::ring_style(focused == Some(index_of(Control::UiScale(1))))}",
                        "+"
                    }
                }
                div {
                    style: "{stepper_style()}",
                    span {
                        style: "{stepper_label_style()}",
                        {t("settings.safe_area")}
                    }
                    button {
                        onclick: move |_| activate(Control::SafeArea(-1), config, on_shell, library, session, status, status_error),
                        style: "{step_style()}{focus::ring_style(focused == Some(index_of(Control::SafeArea(-1))))}",
                        "−"
                    }
                    span {
                        style: "{stepper_value_style()}",
                        "{current.ui.display.safe_area_pct}%"
                    }
                    button {
                        onclick: move |_| activate(Control::SafeArea(1), config, on_shell, library, session, status, status_error),
                        style: "{step_style()}{focus::ring_style(focused == Some(index_of(Control::SafeArea(1))))}",
                        "+"
                    }
                }
                div {
                    style: "{stepper_style()}",
                    span {
                        style: "{stepper_label_style()}",
                        {t("settings.cursor")}
                    }
                    button {
                        onclick: move |_| activate(Control::Cursor(-1), config, on_shell, library, session, status, status_error),
                        style: "{step_style()}{focus::ring_style(focused == Some(index_of(Control::Cursor(-1))))}",
                        "−"
                    }
                    span {
                        style: "{stepper_value_style()}",
                        "{cursor_label(current.ui.display.hide_cursor_ms)}"
                    }
                    button {
                        onclick: move |_| activate(Control::Cursor(1), config, on_shell, library, session, status, status_error),
                        style: "{step_style()}{focus::ring_style(focused == Some(index_of(Control::Cursor(1))))}",
                        "+"
                    }
                }
            }

            // -- Motion, window and safe mode -----------------------------
            div {
                style: "display:flex;flex-direction:column;gap:8px;",
                SectionLabel { label: t("settings.behaviour") }
                div {
                    style: "display:flex;flex-wrap:wrap;gap:8px;",
                    button {
                        onclick: move |_| activate(Control::ReduceAnimations, config, on_shell, library, session, status, status_error),
                        style: "{toggle_style(is_on(Control::ReduceAnimations, &current))}{focus::ring_style(focused == Some(index_of(Control::ReduceAnimations)))}",
                        span { style: "font-weight:600;", {t("settings.reduce_animations")} }
                        span { style: "opacity:0.8;", {on_off(is_on(Control::ReduceAnimations, &current))} }
                    }
                    button {
                        onclick: move |_| activate(Control::ConsoleFullscreen, config, on_shell, library, session, status, status_error),
                        style: "{toggle_style(is_on(Control::ConsoleFullscreen, &current))}{focus::ring_style(focused == Some(index_of(Control::ConsoleFullscreen)))}",
                        span { style: "font-weight:600;", {t("settings.console_fullscreen")} }
                        span { style: "opacity:0.8;", {on_off(is_on(Control::ConsoleFullscreen, &current))} }
                    }
                    button {
                        onclick: move |_| activate(Control::SafeMode, config, on_shell, library, session, status, status_error),
                        style: "{toggle_style(is_on(Control::SafeMode, &current))}{focus::ring_style(focused == Some(index_of(Control::SafeMode)))}",
                        span { style: "font-weight:600;", {t("settings.safe_mode")} }
                        span { style: "opacity:0.8;", {on_off(is_on(Control::SafeMode, &current))} }
                    }
                }
                span {
                    style: "{hint_style()}",
                    {t("settings.safe_mode_hint")}
                }
            }

            // -- Compat database ------------------------------------------
            div {
                style: "display:flex;flex-direction:column;gap:8px;",
                SectionLabel { label: t("settings.compat") }
                div {
                    style: "display:flex;flex-wrap:wrap;gap:8px;",
                    button {
                        onclick: move |_| activate(Control::CompatSync, config, on_shell, library, session, status, status_error),
                        style: "{toggle_style(is_on(Control::CompatSync, &current))}{focus::ring_style(focused == Some(index_of(Control::CompatSync)))}",
                        span { style: "font-weight:600;", {t("settings.compat_sync")} }
                        span { style: "opacity:0.8;", {on_off(is_on(Control::CompatSync, &current))} }
                    }
                    button {
                        onclick: move |_| activate(Control::CompatSyncNow, config, on_shell, library, session, status, status_error),
                        style: "{action_style()}{focus::ring_style(focused == Some(index_of(Control::CompatSyncNow)))}",
                        {t("settings.sync_now")}
                    }
                }
                span {
                    style: "{hint_style()}",
                    {t("settings.compat_sync_hint")}
                }
                if let Some(target) = compat_target.clone() {
                    PathLine {
                        label: t("settings.compat_target"),
                        path: target.display().to_string(),
                    }
                }
            }

            // -- Steam configuration --------------------------------------
            div {
                style: "display:flex;flex-direction:column;gap:8px;",
                SectionLabel { label: t("settings.steam") }
                button {
                    onclick: move |_| activate(Control::RestoreSteam, config, on_shell, library, session, status, status_error),
                    style: "{action_style()}{focus::ring_style(focused == Some(index_of(Control::RestoreSteam)))}",
                    {t("settings.restore_steam")}
                }
                span {
                    style: "{hint_style()}",
                    {t("settings.restore_steam_hint")}
                }
                if let Some(dir) = backup_dir.clone() {
                    PathLine {
                        label: t("settings.backup_dir"),
                        path: dir.display().to_string(),
                    }
                }
            }

            // -- Runtime paths --------------------------------------------
            div {
                style: "display:flex;flex-direction:column;gap:6px;",
                SectionLabel { label: t("settings.runtime") }
                for (label, value) in runtime {
                    div {
                        style: "display:flex;gap:10px;font-size:var(--text-caption);",
                        span {
                            style: "min-width:9rem;color:var(--text-muted);",
                            "{label}"
                        }
                        span {
                            style: "color:var(--text-secondary);overflow-wrap:anywhere;font-family:monospace;",
                            "{value}"
                        }
                    }
                }
            }

            // -- Controls (the link, not a copy) --------------------------
            div {
                style: "display:flex;flex-direction:column;gap:8px;",
                SectionLabel { label: t("settings.controls") }
                button {
                    onclick: move |_| activate(Control::OpenControls, config, on_shell, library, session, status, status_error),
                    style: "{action_style()}{focus::ring_style(focused == Some(index_of(Control::OpenControls)))}",
                    {t("settings.open_controls")}
                }
                span {
                    style: "{hint_style()}",
                    {t("settings.controls_hint")}
                }
            }

            if !status_text.is_empty() {
                div {
                    style: if status_bad { "padding:8px 12px;border-radius:8px;background:#3a1e1e;color:var(--danger);font-size:var(--text-caption);" } else { "padding:8px 12px;border-radius:8px;background:#1e2a24;color:var(--success);font-size:var(--text-caption);" },
                    "{status_text}"
                }
            }
        }
    }
}

/// Carry out one control: mutate the config, persist it, and run any extra
/// intent (switch the shell, sync the database, restore Steam, open Controls).
///
/// The config is re-read from disk before the write, so this screen cannot
/// clobber a field another writer (the shell's remembered `last_shell`) changed
/// since the screen opened: the file is the one source of truth.
#[allow(clippy::too_many_arguments)]
fn activate(
    control: Control,
    mut config: Signal<AppConfig>,
    on_shell: EventHandler<Shell>,
    library: Signal<Option<Library>>,
    mut session: Signal<Session>,
    status: Signal<String>,
    status_error: Signal<bool>,
) {
    let (mut next, _warnings) = AppConfig::load_default();
    let intent = apply(control, &mut next);
    match intent {
        Intent::Save => {
            config.set(next.clone());
            save(&next, status, status_error);
        }
        Intent::SaveAndSync => {
            config.set(next.clone());
            save(&next, status, status_error);
            sync_compat(status, status_error);
        }
        Intent::Sync => sync_compat(status, status_error),
        Intent::SwitchShell(choice) => on_shell.call(choice),
        Intent::RestoreSteam => restore_steam(library, status, status_error),
        Intent::OpenControls => session.write().go(View::Controls),
    }
}

/// Apply one action to the Settings screen's own graph.
#[allow(clippy::too_many_arguments)]
fn handle_action(
    action: Action,
    screen_focus: focus::ScreenFocus,
    config: Signal<AppConfig>,
    on_shell: EventHandler<Shell>,
    library: Signal<Option<Library>>,
    mut session: Signal<Session>,
    status: Signal<String>,
    status_error: Signal<bool>,
) {
    if action == Action::Back {
        session.write().go(View::Library);
        return;
    }
    if action.is_navigation() {
        screen_focus.move_in(&graph(), action);
        return;
    }
    if action != Action::Confirm {
        return;
    }
    let Some(index) = screen_focus.index() else {
        return;
    };
    let Some(control) = cells().get(index).copied() else {
        return;
    };
    activate(
        control,
        config,
        on_shell,
        library,
        session,
        status,
        status_error,
    );
}

/// Persist the config to the same file the shell and the CLI read.
fn save(config: &AppConfig, mut status: Signal<String>, mut status_error: Signal<bool>) {
    match config.save_default() {
        Ok(()) => {
            status.set(t("settings.saved").to_string());
            status_error.set(false);
        }
        Err(error) => {
            status.set(t("settings.save_error").replace("{error}", &error.to_string()));
            status_error.set(true);
        }
    }
}

/// Copy the shipped compat database into the data directory (the sync).
fn sync_compat(mut status: Signal<String>, mut status_error: Signal<bool>) {
    let Some(target) = paths::data_dir().map(|dir| dir.join("compat")) else {
        status.set(t("settings.no_data_dir").to_string());
        status_error.set(true);
        return;
    };
    // The source is the packaged database, never the copy being written: a
    // stale copy would otherwise win the loader's later-source merge.
    let sources = pitagames_platform::compat::sync_sources(&target);
    let report = pitagames_platform::compat::sync_shipped(&sources, &target);
    if report.is_ok() {
        status.set(t("settings.synced").replace("{count}", &report.synced().to_string()));
        status_error.set(false);
    } else {
        status.set(report.summary());
        status_error.set(true);
    }
}

/// Restore the launcher's Steam changes for every Steam row, through the same
/// writer the detail page's Runtime panel uses.
fn restore_steam(
    library: Signal<Option<Library>>,
    mut status: Signal<String>,
    mut status_error: Signal<bool>,
) {
    let snapshot = library.read().clone();
    let Some(steam_root) = snapshot
        .as_ref()
        .and_then(|library| library.steam_root.clone())
    else {
        status.set(t("settings.no_steam_root").to_string());
        status_error.set(true);
        return;
    };
    let ids: Vec<String> = snapshot
        .as_ref()
        .map(|library| {
            library
                .games
                .iter()
                .filter(|entry| entry.is_steam())
                .map(|entry| entry.app_id().to_string())
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        status.set(t("settings.no_steam_games").to_string());
        status_error.set(false);
        return;
    }
    let mut restored = 0usize;
    let mut first_error: Option<String> = None;
    for app_id in &ids {
        match crate::runtime::restore(&steam_root, app_id, false) {
            Ok(_) => restored += 1,
            Err(message) => {
                first_error.get_or_insert(message);
            }
        }
    }
    if let Some(error) = first_error {
        status.set(
            t("settings.restore_steam_error")
                .replace("{restored}", &restored.to_string())
                .replace("{error}", &error),
        );
        status_error.set(true);
    } else {
        status.set(t("settings.restored").replace("{count}", &restored.to_string()));
        status_error.set(false);
    }
}

/// The read-only runtime lines: the discovered Proton builds and the paths.
fn runtime_lines(
    proton: Option<&DiscoveryReport>,
    library: Option<&Library>,
) -> Vec<(String, String)> {
    let mut lines = Vec::new();
    match proton {
        Some(report) if !report.installs.is_empty() => {
            for install in &report.installs {
                lines.push((
                    t("settings.proton_builds").to_string(),
                    install.path.display().to_string(),
                ));
            }
        }
        _ => lines.push((
            t("settings.proton_builds").to_string(),
            t("settings.no_proton").to_string(),
        )),
    }
    if let Some(root) = library.and_then(|library| library.steam_root.clone()) {
        lines.push((
            t("settings.steam_root").to_string(),
            root.display().to_string(),
        ));
    }
    for (label, path) in [
        (t("settings.prefixes_dir"), paths::prefixes_dir()),
        (t("settings.games_dir"), paths::games_dir()),
        (t("settings.logs_dir"), paths::logs_dir()),
    ] {
        if let Some(path) = path {
            lines.push((label.to_string(), path.display().to_string()));
        }
    }
    lines
}

fn chip_style(active: bool) -> String {
    shell::pill_style(
        active,
        "calc(6px * var(--ui-scale, 1)) calc(14px * var(--ui-scale, 1))",
        "cursor:pointer;",
        shell::PILL_INACTIVE,
    )
}

fn toggle_style(on: bool) -> String {
    let state = if on {
        "border:1px solid #3a5a48;background:#243a2e;color:var(--success);"
    } else {
        "border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);"
    };
    format!(
        "display:inline-flex;align-items:center;gap:8px;padding:8px 14px;border-radius:8px;font-size:var(--text-caption);cursor:pointer;{state}{}",
        shell::HIT_FLOOR
    )
}

fn action_style() -> String {
    format!(
        "align-self:flex-start;padding:8px 14px;border-radius:8px;border:1px solid #3a3a4c;background:#242432;color:var(--text);font-size:var(--text-caption);cursor:pointer;{}",
        shell::HIT_FLOOR
    )
}

fn step_style() -> String {
    format!(
        "padding:6px 12px;border-radius:8px;border:1px solid #2c2c38;background:#1e1e28;color:var(--text);font-size:var(--text-caption);cursor:pointer;{}",
        shell::HIT_FLOOR
    )
}

fn stepper_style() -> &'static str {
    "display:flex;align-items:center;gap:10px;"
}

fn stepper_label_style() -> &'static str {
    "min-width:10rem;color:var(--text-secondary);font-size:var(--text-caption);"
}

fn stepper_value_style() -> &'static str {
    "min-width:4rem;text-align:center;color:var(--text);font-variant-numeric:tabular-nums;"
}

fn hint_style() -> &'static str {
    "font-size:var(--text-micro);color:var(--text-disabled);line-height:1.5;"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_safe_mode_toggle_writes_the_one_config_file() {
        let mut config = AppConfig::default();
        assert!(!config.ui.safe_mode);

        // The control flips the same `[ui] safe_mode` the supervisor reads.
        assert_eq!(apply(Control::SafeMode, &mut config), Intent::Save);
        assert!(config.ui.safe_mode);

        // And it survives the file the CLI and the shell read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config.save(&path).unwrap();
        let (loaded, _) = AppConfig::load(&path);
        assert!(loaded.ui.safe_mode);
    }

    #[test]
    fn the_compat_sync_toggle_turns_the_sync_on_and_asks_for_a_copy() {
        let mut config = AppConfig::default();
        assert!(!config.compat.sync);

        // Turning it on mutates the shared config *and* runs the sync.
        assert_eq!(apply(Control::CompatSync, &mut config), Intent::SaveAndSync);
        assert!(config.compat.sync);

        // Turning it off only writes the config.
        assert_eq!(apply(Control::CompatSync, &mut config), Intent::Save);
        assert!(!config.compat.sync);
    }

    #[test]
    fn the_start_mode_chip_sets_the_start_mode() {
        let mut config = AppConfig::default();
        assert_eq!(config.ui.start_mode, StartMode::Remember);
        assert_eq!(
            apply(Control::StartMode(StartMode::Console), &mut config),
            Intent::Save
        );
        assert_eq!(config.ui.start_mode, StartMode::Console);
        assert!(selected(
            Control::StartMode(StartMode::Console),
            &config,
            Shell::Desktop
        ));
    }

    #[test]
    fn the_shell_chip_switches_the_live_shell_without_touching_the_config() {
        let mut config = AppConfig::default();
        let before = config.clone();
        assert_eq!(
            apply(Control::Shell(Shell::Console), &mut config),
            Intent::SwitchShell(Shell::Console)
        );
        // The live shell is the shell state's business, not a config write.
        assert_eq!(config, before);
        assert!(selected(
            Control::Shell(Shell::Console),
            &config,
            Shell::Console
        ));
        assert!(!selected(
            Control::Shell(Shell::Console),
            &config,
            Shell::Desktop
        ));
    }

    #[test]
    fn the_number_steps_clamp_to_the_accepted_range() {
        let mut config = AppConfig::default();
        for _ in 0..50 {
            apply(Control::UiScale(1), &mut config);
            apply(Control::SafeArea(1), &mut config);
        }
        assert_eq!(config.ui.display.ui_scale, MAX_UI_SCALE);
        assert_eq!(config.ui.display.safe_area_pct, MAX_SAFE_AREA_PCT);

        for _ in 0..50 {
            apply(Control::UiScale(-1), &mut config);
            apply(Control::SafeArea(-1), &mut config);
        }
        assert_eq!(config.ui.display.ui_scale, MIN_UI_SCALE);
        assert_eq!(config.ui.display.safe_area_pct, 0.0);
    }

    #[test]
    fn the_cursor_step_cycles_through_the_offered_options() {
        let mut config = AppConfig::default();
        let default = config.ui.display.hide_cursor_ms;
        assert!(CURSOR_OPTIONS_MS.contains(&default));

        let mut seen = BTreeSet::new();
        seen.insert(config.ui.display.hide_cursor_ms);
        for _ in 0..CURSOR_OPTIONS_MS.len() {
            apply(Control::Cursor(1), &mut config);
            seen.insert(config.ui.display.hide_cursor_ms);
        }
        // One full cycle visits every option and returns to the start.
        assert_eq!(seen.len(), CURSOR_OPTIONS_MS.len());
        assert_eq!(config.ui.display.hide_cursor_ms, default);

        // Stepping back is the inverse.
        apply(Control::Cursor(-1), &mut config);
        assert_ne!(config.ui.display.hide_cursor_ms, default);
    }

    #[test]
    fn every_toggle_maps_to_a_real_config_field() {
        // Each toggle control is readable back from the config, so the screen
        // and the file cannot disagree.
        let mut config = AppConfig::default();
        for control in [
            Control::ReduceAnimations,
            Control::ConsoleFullscreen,
            Control::SafeMode,
            Control::CompatSync,
        ] {
            let before = toggle_on(control, &config).expect("a toggle has a state");
            apply(control, &mut config);
            let after = toggle_on(control, &config).expect("a toggle has a state");
            assert_ne!(before, after, "{control:?} did not flip a field");
        }
    }

    #[test]
    fn every_control_has_a_stable_cell_id() {
        // `index_of` is what the screen and the action drain both use; a
        // duplicate control would fold two cells onto one id.
        let cells = cells();
        for (position, control) in cells.iter().enumerate() {
            assert_eq!(index_of(*control), position, "{control:?}");
        }
    }

    #[test]
    fn the_cells_and_bands_are_the_same_layout() {
        let cells = cells();
        let bands = bands();
        assert_eq!(
            cells.len(),
            bands.iter().map(|band| band.count).sum::<usize>(),
            "the flat cell ids and the bands describe one layout"
        );
        // Every band lays its cells on one row.
        for band in &bands {
            assert_eq!(band.count, band.columns);
        }
        // Every control appears exactly once.
        let unique: BTreeSet<String> = cells.iter().map(|c| format!("{c:?}")).collect();
        assert_eq!(unique.len(), cells.len());
    }

    #[test]
    fn the_focus_graph_reaches_every_cell_from_every_cell() {
        // The one property the shared graph must hold: no control is stranded.
        let graph = graph();
        let count = graph.len();
        assert_eq!(count, cells().len());
        let directions = [
            Action::NavigateUp,
            Action::NavigateDown,
            Action::NavigateLeft,
            Action::NavigateRight,
        ];
        for start in 0..count {
            let mut reached = BTreeSet::new();
            reached.insert(start);
            let mut frontier = vec![start];
            while let Some(current) = frontier.pop() {
                for direction in directions {
                    let mut walk = graph.clone();
                    walk.focus_index(Some(current));
                    if let Some(next) = walk.move_focus(direction)
                        && reached.insert(next.get() as usize)
                    {
                        frontier.push(next.get() as usize);
                    }
                }
            }
            assert_eq!(reached.len(), count, "cell {start} cannot reach every cell");
        }
    }

    #[test]
    fn the_cursor_label_reads_never_or_seconds() {
        assert_eq!(cursor_label(0), t("settings.cursor_never"));
        assert_eq!(cursor_label(3_000), "3.0s");
    }
}
