//! The two shells: the window state and the style each one wears.
//!
//! `pitagames_core::ui` owns the *values* — which shell, the display numbers, the
//! persisted preference. This module owns the Dioxus/tao seam: the window flags a
//! shell asks for, the CSS its root wears, and the toggle that reconfigures the
//! same window without restarting the process.
//!
//! Two shells, one process. The desktop shell is a windowed, decorated frame at
//! 1280×800; the console shell is undecorated and fullscreen with a TV overscan
//! margin and a larger base type. Toggling only changes the window flags and the
//! CSS — the same components, the same focus graphs, the same session.

use std::path::PathBuf;
use std::rc::Rc;

use dioxus::desktop::tao::dpi::{LogicalSize, PhysicalPosition};
use dioxus::desktop::tao::monitor::MonitorHandle;
use dioxus::desktop::tao::window::{Fullscreen, Window, WindowBuilder};
use dioxus::prelude::*;
use pitagames_core::design::{self, DesignTokens};
use pitagames_core::ui::{
    AppConfig, DESKTOP_HEIGHT, DESKTOP_MIN_HEIGHT, DESKTOP_MIN_WIDTH, DESKTOP_WIDTH, DisplayConfig,
    MonitorInfo, Placement, Shell,
};

/// The base type size of each shell, before the UI scale is applied.
pub const DESKTOP_BASE_PX: f32 = 16.0;
pub const CONSOLE_BASE_PX: f32 = 20.0;

/// The shell the command line asked for, plus anything it did not understand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellArgs {
    /// `--console` or `--desktop`; `None` means "use the preference".
    pub shell: Option<Shell>,
    /// `--safe-mode`: the DMABUF renderer off, animations and blur off.
    pub safe_mode: bool,
    /// `-V` / `--version`.
    pub version: bool,
    /// Flags the launcher does not recognise, so `main` can warn about them.
    pub unknown: Vec<String>,
}

/// Parse the launcher's command line.
///
/// Deliberately tiny: the launcher takes no positional arguments. A later flag
/// can be added without a parser crate, and the whole thing is testable.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> ShellArgs {
    let mut parsed = ShellArgs::default();
    for arg in args {
        match arg.as_str() {
            "--console" => parsed.shell = Some(Shell::Console),
            "--desktop" => parsed.shell = Some(Shell::Desktop),
            "--safe-mode" => parsed.safe_mode = true,
            "-V" | "--version" => parsed.version = true,
            _ => parsed.unknown.push(arg),
        }
    }
    parsed
}

/// The startup values the app needs from `main`.
///
/// Passed as a Dioxus context because the window is built before the first
/// render: `main` resolves the shell, builds the window, and hands the app the
/// same values so the first render already agrees with the window.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellStart {
    /// Which shell the window was opened in.
    pub shell: Shell,
    /// The display numbers both shells read. Safe mode forces
    /// `reduce_animations`, so a shell cannot re-enable motion by accident.
    pub display: DisplayConfig,
    /// Whether this run is in safe mode (the header shows a badge).
    pub safe_mode: bool,
    /// Where the GUI writes a line once the window is up, so the supervisor can
    /// tell a webview init failure from a normal exit. `None` when the process
    /// is not supervised.
    pub ready_path: Option<PathBuf>,
    /// Where the preference is remembered; `None` when `HOME` is unset.
    pub config_path: Option<PathBuf>,
}

/// The window geometry and flags a shell asks for.
///
/// Pure data so the shell logic is testable without a window. [`apply_to_window`]
/// turns it into tao calls and [`window_builder`] into a startup builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowConfig {
    pub title: String,
    pub fullscreen: bool,
    pub decorations: bool,
    pub maximized: bool,
    pub monitor: Option<String>,
    pub width: u32,
    pub height: u32,
    pub min_width: u32,
    pub min_height: u32,
}

impl WindowConfig {
    /// The desktop shell's frame: frameless, 1280×800, resizable.
    ///
    /// The desktop shell is undecorated to match the plan; the titlebar it would
    /// otherwise lose is drawn in the app header (drag to move, plus minimize
    /// and close), so a frameless window cannot trap the player.
    pub fn desktop() -> Self {
        Self {
            title: "PitaGames".to_string(),
            fullscreen: false,
            decorations: false,
            maximized: false,
            monitor: None,
            width: DESKTOP_WIDTH as u32,
            height: DESKTOP_HEIGHT as u32,
            min_width: DESKTOP_MIN_WIDTH as u32,
            min_height: DESKTOP_MIN_HEIGHT as u32,
        }
    }

    /// The console shell's frame: undecorated, fullscreen unless the config says
    /// otherwise, with the same fallback geometry so a windowed console shell is
    /// still a usable size.
    pub fn console(display: &DisplayConfig) -> Self {
        Self {
            title: "PitaGames".to_string(),
            fullscreen: display.console_fullscreen,
            decorations: false,
            maximized: !display.console_fullscreen,
            monitor: display.monitor.clone(),
            width: DESKTOP_WIDTH as u32,
            height: DESKTOP_HEIGHT as u32,
            min_width: DESKTOP_MIN_WIDTH as u32,
            min_height: DESKTOP_MIN_HEIGHT as u32,
        }
    }

    /// Where a windowed frame should open, resolved against the attached
    /// monitors.
    ///
    /// Pure, so the display model is testable without a window. The fullscreen
    /// console shell picks its monitor handle in [`place_at_startup`] instead,
    /// because a `Fullscreen` needs the handle, not a position.
    pub fn startup_placement(&self, monitors: &[MonitorInfo]) -> Placement {
        pitagames_core::ui::placement(self.monitor.as_deref(), self.width, self.height, monitors)
    }
}

/// The window config for a shell.
pub fn window_config(shell: Shell, display: &DisplayConfig) -> WindowConfig {
    match shell {
        Shell::Desktop => WindowConfig::desktop(),
        Shell::Console => WindowConfig::console(display),
    }
}

/// A startup [`WindowBuilder`] for a config.
///
/// A named monitor cannot be resolved here (monitor handles come from the event
/// loop, and this runs before the window exists), so the builder opens on the
/// primary; [`place_at_startup`] re-places the built window on the configured
/// monitor through Dioxus's `with_on_window` hook.
pub fn window_builder(config: &WindowConfig) -> WindowBuilder {
    let mut builder = WindowBuilder::new()
        .with_title(config.title.clone())
        .with_inner_size(LogicalSize::new(config.width as f64, config.height as f64))
        .with_min_inner_size(LogicalSize::new(
            config.min_width as f64,
            config.min_height as f64,
        ))
        .with_decorations(config.decorations)
        .with_resizable(true);
    // Naming an explicit position stops Dioxus's debug-only window-state restore
    // from overriding the monitor placement below (`place_at_startup` moves the
    // window to the configured monitor either way; with no monitor the window
    // is left where the window manager puts it).
    if config.monitor.is_some() {
        builder = builder.with_position(PhysicalPosition::new(0, 0));
    }
    if config.fullscreen {
        builder = builder.with_fullscreen(Some(Fullscreen::Borderless(None)));
    }
    if config.maximized {
        builder = builder.with_maximized(true);
    }
    builder
}

/// Place a freshly built window on the configured monitor.
///
/// A monitor handle only exists once the window is built, so the named monitor
/// cannot be resolved in [`window_builder`] (which runs before the event loop
/// reports any monitor). `main` runs this from Dioxus's `with_on_window` hook,
/// which fires after `Window::build` and before the webview is created, so the
/// window is placed before it is ever painted. With no configured monitor — or
/// a name that is not attached — the window is left on the primary, which is
/// the platform default.
///
/// The centring compares the logical window size against the monitor's physical
/// geometry, so it assumes a scale factor of 1. A HiDPI display is part of the
/// deferred 4K pass (`docs/m2.5-shells.md`).
pub fn place_at_startup(window: &Window, config: &WindowConfig) {
    // Keep the handle beside its geometry: the fullscreen path needs the handle
    // itself, and re-enumerating would assume the two calls agree on order.
    let attached: Vec<(MonitorHandle, MonitorInfo)> = window
        .available_monitors()
        .map(|handle| {
            let position = handle.position();
            let size = handle.size();
            let info = MonitorInfo::new(
                handle.name(),
                position.x,
                position.y,
                size.width,
                size.height,
            );
            (handle, info)
        })
        .collect();
    let infos: Vec<MonitorInfo> = attached.iter().map(|(_, info)| info.clone()).collect();

    let Some(index) = pitagames_core::ui::select_monitor(config.monitor.as_deref(), &infos) else {
        return;
    };

    // A fullscreen window is put on a monitor by handle, not by position.
    if config.fullscreen {
        let handle = attached[index].0.clone();
        window.set_fullscreen(Some(Fullscreen::Borderless(Some(handle))));
        return;
    }

    let (x, y) = attached[index].1.center(config.width, config.height);
    window.set_outer_position(PhysicalPosition::new(x, y));
}

/// Reconfigure the live window for a config.
///
/// Called from the event-loop thread (the key/pad pumps and the toggle), which is
/// where tao's window calls belong.
pub fn apply_to_window(config: &WindowConfig) {
    let desktop = dioxus::desktop::window();
    // `DesktopService` has its own `set_fullscreen(bool)`; the named monitor
    // needs tao's `Window::set_fullscreen`, reached through the `window` field.
    desktop.window.set_decorations(config.decorations);
    desktop.window.set_resizable(true);
    if config.fullscreen {
        desktop
            .window
            .set_fullscreen(Some(Fullscreen::Borderless(monitor(config))));
    } else {
        desktop.window.set_fullscreen(None);
        desktop.window.set_maximized(config.maximized);
    }
}

/// The monitor the config names, if it is attached.
fn monitor(config: &WindowConfig) -> Option<MonitorHandle> {
    let name = config.monitor.as_deref()?;
    let window = dioxus::desktop::window();
    window
        .available_monitors()
        .find(|handle| handle.name().is_some_and(|attached| attached == name))
}

/// Hide the window while a supervised game runs.
///
/// The console shell's hand-off: the game owns the screen, so the launcher
/// unmaps itself rather than sitting behind it. Best effort — a compositor that
/// refuses costs nothing, since the game is in front anyway.
///
/// An unmapped WebKitGTK window stops running its render loop, which also
/// freezes the Dioxus virtualdom, so the return trip cannot be a future: it is
/// [`reveal_window`], called from the game's worker thread.
pub fn hide_window() {
    dioxus::desktop::window().set_visible(false);
}

/// The live window, installed once at startup so a worker thread can reach it.
///
/// One window per process, so one slot. [`reveal_window`] is the only reader.
static WINDOW: std::sync::OnceLock<std::sync::Arc<dioxus::desktop::tao::window::Window>> =
    std::sync::OnceLock::new();

/// Record the live window so [`reveal_window`] can reach it from a worker.
///
/// Called once from `App` (where the Dioxus context is available).
pub fn install_window() {
    let _ = WINDOW.set(dioxus::desktop::window().window.clone());
}

/// Map the window again from any thread, for the hand-off's return trip.
///
/// tao turns every window call into a request queued to the event loop, so this
/// is safe off the main thread (`tao::Window` is `Send + Sync` on Linux). It has
/// to run off the main thread: an unmapped WebKitGTK window freezes the Dioxus
/// virtualdom, so the drain future that would normally show the window cannot
/// run until the window is mapped again — mapping it is what unfreezes it.
///
/// Returns whether a window was installed.
pub fn reveal_window() -> bool {
    let Some(window) = WINDOW.get() else {
        return false;
    };
    if window.is_minimized() {
        window.set_minimized(false);
    }
    window.set_visible(true);
    window.set_focus();
    true
}

/// Bring the window back and ask the compositor for focus.
///
/// Used to return from a game and to raise the window when a second launch asks
/// for it. Best effort by design: a Wayland compositor may refuse (focus
/// stealing prevention) and the window is still raised. The calls are cheap and
/// idempotent, so they run every time rather than tracking window state.
pub fn show_and_focus() {
    let window = dioxus::desktop::window();
    if window.is_minimized() {
        window.set_minimized(false);
    }
    window.set_visible(true);
    window.set_focus();
}

/// Nudge the user's attention to a window the compositor would not focus.
///
/// The hand-off's fallback: when focus is refused the window still has to be
/// noticed, so the taskbar/dock entry is flagged critical. Harmless when it is
/// already focused.
pub fn request_attention() {
    use dioxus::desktop::tao::window::UserAttentionType;
    dioxus::desktop::window().request_user_attention(Some(UserAttentionType::Critical));
}

/// The live shell state the app toggles.
///
/// Cheap to clone: the signal is `Copy`, the display numbers are behind an `Rc`,
/// and the config path is only read when the preference is written.
#[derive(Clone)]
pub struct ShellState {
    /// The shell the window is currently in.
    pub shell: Signal<Shell>,
    /// The display numbers both shells read.
    pub display: Rc<DisplayConfig>,
    /// Where the preference is remembered.
    pub config_path: Option<PathBuf>,
}

impl ShellState {
    /// Build the state from the startup values.
    pub fn new(start: &ShellStart) -> Self {
        Self {
            shell: Signal::new(start.shell),
            display: Rc::new(start.display.clone()),
            config_path: start.config_path.clone(),
        }
    }

    /// The shell the window is in.
    pub fn shell(&self) -> Shell {
        (self.shell)()
    }

    /// Switch to the other shell and remember it.
    ///
    /// The window is reconfigured first so a failed window call cannot leave the
    /// CSS claiming a shell the window is not in.
    pub fn toggle(&mut self) {
        let next = self.shell().toggled();
        apply_to_window(&window_config(next, &self.display));
        self.shell.set(next);
        self.persist(next);
    }

    /// Write the shell the player chose, so `Remember` opens there next time.
    ///
    /// A failure is a warning: a read-only config directory must not stop the
    /// toggle from working in this session.
    fn persist(&self, shell: Shell) {
        let Some(path) = &self.config_path else {
            return;
        };
        let (mut config, _warnings) = AppConfig::load(path);
        config.ui.remember(shell);
        if let Err(error) = config.save(path) {
            eprintln!(
                "pitagames: could not remember the shell in {}: {error}",
                path.display()
            );
        }
    }
}

/// The base type size a shell's chrome uses, after the UI scale.
pub fn base_font_px(shell: Shell, display: &DisplayConfig) -> f32 {
    let base = if shell.is_console() {
        CONSOLE_BASE_PX
    } else {
        DESKTOP_BASE_PX
    };
    base * display.ui_scale
}

/// The app root's inline style for a shell.
///
/// Both branches name the same properties on purpose: Dioxus's style setter
/// re-adds an inline property a new style string omits, so a console-only
/// `padding` would survive the switch back to the desktop shell. The design
/// tokens are emitted as custom properties so a component asks for a role, not
/// a pixel size, and the console shell gets its 10-foot scale for free.
pub fn root_style(shell: Shell, display: &DisplayConfig) -> String {
    let padding = if shell.is_console() {
        display.safe_area_css()
    } else {
        "0".to_string()
    };
    let background = if shell.is_console() {
        design::CONSOLE_BACKGROUND
    } else {
        design::BACKGROUND
    };
    let tokens = DesignTokens::for_shell(shell, display);
    format!(
        "--ui-scale:{};{}--bg:{};--card:{};--text:{};--text-secondary:{};--text-muted:{};--text-disabled:{};--accent:{};--warning:{};--danger:{};--success:{};box-sizing:border-box;display:flex;flex-direction:column;height:100vh;overflow:hidden;padding:{padding};background:{};color:var(--text);font-family:sans-serif;font-size:{}px;",
        display.ui_scale,
        tokens.css_vars(),
        background.hex(),
        design::CARD.hex(),
        design::TEXT.hex(),
        design::TEXT_SECONDARY.hex(),
        design::TEXT_MUTED.hex(),
        design::TEXT_DISABLED.hex(),
        design::ACCENT.hex(),
        design::WARNING.hex(),
        design::DANGER.hex(),
        design::SUCCESS.hex(),
        background.hex(),
        base_font_px(shell, display)
    )
}

/// The hit floor every interactive style names, as an inline declaration.
///
/// There is no global stylesheet: this Dioxus build ignores a `<style>` element
/// in RSX (an Xvfb probe confirmed it) and the renderer boundary rules out
/// injecting CSS with JavaScript, so the floor travels inline on each control,
/// reading the shell's `--hit-min` token from the root style.
pub const HIT_FLOOR: &str = "min-height:var(--hit-min);min-width:var(--hit-min);";

/// A frameless titlebar control (minimize, close) for the desktop shell.
///
/// The desktop window has no decoration, so these are the only way to minimize
/// or close it. They carry the shell's hit floor, so a pointer cannot miss the
/// one control that stops the window being trapped.
pub fn window_control_style() -> String {
    format!(
        "display:inline-flex;align-items:center;justify-content:center;padding:0 12px;border:1px solid #2c2c38;border-radius:8px;background:#1e1e28;color:var(--text-secondary);font-size:var(--text-caption);cursor:pointer;{HIT_FLOOR}"
    )
}

/// The app header's inline style for a shell.
///
/// The wordmark reads `--text-title` and the subtitle `--text-caption`, so the
/// console shell's scale applies without a second header rule.
pub fn header_style(_shell: Shell, _display: &DisplayConfig) -> String {
    "display:flex;flex-wrap:wrap;align-items:baseline;gap:16px;row-gap:4px;padding:20px 32px 12px;font-size:var(--text-caption);"
        .to_string()
}

/// The highlight a selected pill wears.
///
/// Shared by the desktop nav, the filter chips and the console destination
/// strip, so "selected" reads the same wherever it appears.
pub const PILL_ACTIVE: &str = "border:1px solid #3f5c8c;background:#2e3a56;color:#ffffff;";

/// The resting look of a *control* pill (the nav and the filter chips): the
/// plain card, so an unselected control reads as a card rather than a hole.
pub const PILL_INACTIVE: &str =
    "border:1px solid #2c2c38;background:#1e1e28;color:var(--text-secondary);";

/// A pill's inline style, shared by the nav, the filter chips and the console
/// destination strip.
///
/// `padding` and `extra` are the row's own layout (a chip is tighter than a nav
/// pill; the strip is a passive inline-flex indicator); `inactive` is the
/// resting look — the plain card for a control, transparent for the strip. The
/// type is the shell's caption token and the hit floor travels inline, so the
/// shape is identical in both shells.
pub fn pill_style(active: bool, padding: &str, extra: &str, inactive: &str) -> String {
    let state = if active { PILL_ACTIVE } else { inactive };
    format!(
        "padding:{padding};border-radius:999px;{state}font-size:var(--text-caption);{extra}{HIT_FLOOR}"
    )
}

/// An action button that must look disabled when it cannot work.
///
/// `layout` is everything before the state colours (padding, radius, type) and
/// `enabled_colors` is the border/fill/text an enabled button wears. The
/// disabled branch is one shared grey with no hit area, so a dead control cannot
/// be focused or clicked. The detail page's Play, hero Play/Install and Save
/// buttons all share this pair, so "disabled" reads the same across the page.
pub fn action_style(layout: &str, enabled_colors: &str, enabled: bool) -> String {
    if enabled {
        format!("{layout}{enabled_colors}cursor:pointer;{HIT_FLOOR}")
    } else {
        format!(
            "{layout}border:1px solid #26262f;background:#1a1a22;color:var(--text-disabled);cursor:default;"
        )
    }
}

/// The console destination strip's per-destination style.
///
/// The strip is a *passive* indicator: it wears the shared pill shape and the
/// active highlight, but its resting look is transparent and it is not a
/// control — no `cursor:pointer`, no hover rule, and it never enters a focus
/// graph. `Tab` moves the highlight; the strip only reports where the window is.
pub fn destination_strip_style(active: bool) -> String {
    pill_style(
        active,
        "calc(6px * var(--ui-scale, 1)) calc(14px * var(--ui-scale, 1))",
        "display:inline-flex;align-items:center;white-space:nowrap;",
        "border:1px solid transparent;background:transparent;color:var(--text-muted);",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_command_line_picks_a_shell() {
        let args = parse_args(["--console".to_string()]);
        assert_eq!(args.shell, Some(Shell::Console));
        assert!(!args.version);
        assert!(args.unknown.is_empty());

        let args = parse_args(["--desktop".to_string()]);
        assert_eq!(args.shell, Some(Shell::Desktop));

        let args = parse_args([]);
        assert_eq!(args.shell, None);
    }

    #[test]
    fn the_version_flag_is_recognised() {
        let args = parse_args(["-V".to_string()]);
        assert!(args.version);
        let args = parse_args(["--version".to_string()]);
        assert!(args.version);
    }

    #[test]
    fn unknown_flags_are_collected_for_a_warning() {
        let args = parse_args(["--nope".to_string(), "--console".to_string()]);
        assert_eq!(args.shell, Some(Shell::Console));
        assert_eq!(args.unknown, vec!["--nope".to_string()]);
    }

    #[test]
    fn the_safe_mode_flag_is_recognised() {
        let args = parse_args(["--safe-mode".to_string()]);
        assert!(args.safe_mode);
        assert!(!args.version);
        assert!(args.unknown.is_empty());

        let args = parse_args(["--console".to_string(), "--safe-mode".to_string()]);
        assert_eq!(args.shell, Some(Shell::Console));
        assert!(args.safe_mode);
    }

    #[test]
    fn safe_mode_documents_the_motion_rule() {
        // Safe mode's motion rule is not a shipped stylesheet: this Dioxus build
        // ignores a `<style>` element in RSX (an Xvfb probe with a magenta `div`
        // rule confirmed it), and injecting CSS would need JavaScript, which the
        // renderer boundary forbids. The rule holds by construction instead — no
        // screen uses `animation`, `transition`, `backdrop-filter` or
        // `scroll-behavior`, which `no_screen_ships_motion` above enforces.
        // This test is the written record of that decision.
        let rule = "animation:none;transition:none;scroll-behavior:auto;backdrop-filter:none";
        assert!(rule.contains("animation:none"));
        assert!(rule.contains("transition:none"));
        assert!(rule.contains("scroll-behavior:auto"));
        assert!(rule.contains("backdrop-filter:none"));
    }

    #[test]
    fn the_desktop_frame_is_frameless_and_windowed() {
        let config = window_config(Shell::Desktop, &DisplayConfig::default());
        assert!(!config.fullscreen);
        // The desktop shell is undecorated; the app header is the titlebar.
        assert!(!config.decorations);
        assert!(!config.maximized);
        assert_eq!(config.width, 1_280);
        assert_eq!(config.height, 800);
        assert_eq!(config.min_width, 900);
        assert_eq!(config.min_height, 560);
    }

    #[test]
    fn the_desktop_window_opens_on_the_configured_monitor() {
        let monitors = [
            MonitorInfo::new(Some("eDP-1".to_string()), 0, 0, 1920, 1080),
            MonitorInfo::new(Some("HDMI-A-1".to_string()), 1920, 0, 2560, 1440),
        ];
        let config = window_config(
            Shell::Desktop,
            &DisplayConfig {
                monitor: Some("HDMI-A-1".to_string()),
                ..DisplayConfig::default()
            },
        );
        assert_eq!(
            config.startup_placement(&monitors),
            Placement::Center {
                x: 1920 + (2560 - 1280) / 2,
                y: (1440 - 800) / 2,
            }
        );

        // The default desktop frame names no monitor, so it stays on the
        // primary.
        assert_eq!(
            window_config(Shell::Desktop, &DisplayConfig::default()).startup_placement(&monitors),
            Placement::Default
        );
    }

    #[test]
    fn a_configured_monitor_pins_an_explicit_startup_position() {
        // The explicit position exists only to stop Dioxus's debug-only window
        // state restore from fighting the monitor placement; the primary case
        // leaves the position to the window manager.
        assert!(
            window_builder(&window_config(Shell::Desktop, &DisplayConfig::default()))
                .window
                .position
                .is_none()
        );
        let named = window_config(
            Shell::Console,
            &DisplayConfig {
                monitor: Some("HDMI-A-1".to_string()),
                ..DisplayConfig::default()
            },
        );
        assert!(window_builder(&named).window.position.is_some());
    }

    #[test]
    fn the_window_controls_carry_the_hit_floor() {
        let style = window_control_style();
        assert!(style.contains("cursor:pointer;"), "{style}");
        assert!(style.contains(HIT_FLOOR), "{style}");
        assert!(style.contains("font-size:var(--text-caption);"), "{style}");
    }

    #[test]
    fn the_console_frame_is_undecorated_and_fullscreen_by_default() {
        let config = window_config(Shell::Console, &DisplayConfig::default());
        assert!(config.fullscreen);
        assert!(!config.decorations);
        assert!(!config.maximized);
    }

    #[test]
    fn a_windowed_console_shell_is_maximised_and_undecorated() {
        let display = DisplayConfig {
            console_fullscreen: false,
            ..DisplayConfig::default()
        };
        let config = window_config(Shell::Console, &display);
        assert!(!config.fullscreen);
        assert!(config.maximized);
        assert!(!config.decorations);
    }

    #[test]
    fn the_console_shell_is_larger_and_pads_its_edges() {
        let display = DisplayConfig::default();
        let desktop = root_style(Shell::Desktop, &display);
        let console = root_style(Shell::Console, &display);

        assert!(desktop.contains("padding:0;"));
        assert!(console.contains("padding:2.5vw;"));
        // Both branches name the same properties so the style setter cannot
        // keep a stale value across a switch.
        for property in ["padding:", "background:", "font-size:", "--ui-scale:"] {
            assert!(desktop.contains(property), "desktop lost {property}");
            assert!(console.contains(property), "console lost {property}");
        }
        assert!(console.contains("font-size:20px"));
        assert!(desktop.contains("font-size:16px"));
    }

    #[test]
    fn the_ui_scale_multiplies_the_base_type() {
        let display = DisplayConfig {
            ui_scale: 1.5,
            ..DisplayConfig::default()
        };
        assert_eq!(base_font_px(Shell::Desktop, &display), 24.0);
        assert_eq!(base_font_px(Shell::Console, &display), 30.0);
        assert!(root_style(Shell::Console, &display).contains("--ui-scale:1.5;"));
    }

    #[test]
    fn the_header_reads_the_caption_token() {
        let display = DisplayConfig::default();
        let header = header_style(Shell::Console, &display);
        assert!(header.contains("font-size:var(--text-caption)"));
    }

    #[test]
    fn the_destination_strip_is_a_passive_indicator() {
        let active = destination_strip_style(true);
        let inactive = destination_strip_style(false);
        for style in [&active, &inactive] {
            // Readable at 10 feet and consistent with the nav: the shell's
            // caption token and the shell's hit floor, never a literal size.
            assert!(style.contains("font-size:var(--text-caption)"), "{style}");
            assert!(style.contains("var(--hit-min)"), "{style}");
            // Passive: an indicator, not a control. No pointer affordance, so
            // it can never read as a hover-only action.
            assert!(!style.contains("cursor:pointer"), "{style}");
        }
        assert!(active.contains("color:#ffffff"), "{active}");
        assert!(inactive.contains("var(--text-muted)"), "{inactive}");
    }

    #[test]
    fn the_pill_shape_is_shared_by_every_pill_row() {
        // The nav, the filter chips and the destination strip all wear this
        // shape, so "selected" reads the same wherever it appears.
        let active = pill_style(true, "8px", "cursor:pointer;", PILL_INACTIVE);
        assert!(active.contains(PILL_ACTIVE), "{active}");
        assert!(active.contains("border-radius:999px;"), "{active}");
        assert!(
            active.contains("font-size:var(--text-caption);"),
            "{active}"
        );
        assert!(active.contains(HIT_FLOOR), "{active}");

        let inactive = pill_style(false, "8px", "cursor:pointer;", PILL_INACTIVE);
        assert!(inactive.contains(PILL_INACTIVE), "{inactive}");
        assert!(!inactive.contains(PILL_ACTIVE), "{inactive}");
    }

    #[test]
    fn the_action_style_shares_one_disabled_look() {
        let colors = "border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;";
        let enabled = action_style("padding:1px;", colors, true);
        assert!(enabled.contains(colors), "{enabled}");
        assert!(enabled.contains("cursor:pointer;"), "{enabled}");
        assert!(enabled.contains(HIT_FLOOR), "{enabled}");

        // A dead control looks disabled and offers no hit area.
        let disabled = action_style("padding:1px;", colors, false);
        assert!(disabled.contains("background:#1a1a22;"), "{disabled}");
        assert!(
            disabled.contains("color:var(--text-disabled);"),
            "{disabled}"
        );
        assert!(!disabled.contains("cursor:pointer"), "{disabled}");
        assert!(!disabled.contains("var(--hit-min)"), "{disabled}");
    }

    #[test]
    fn the_root_style_carries_every_design_token_in_both_shells() {
        let display = DisplayConfig::default();
        for shell in [Shell::Desktop, Shell::Console] {
            let css = root_style(shell, &display);
            for name in [
                "--text-title:",
                "--text-heading:",
                "--text-body:",
                "--text-caption:",
                "--text-micro:",
                "--hit-min:",
                "--ring:",
                "--bg:",
                "--card:",
                "--text:",
                "--text-secondary:",
                "--text-muted:",
                "--text-disabled:",
                "--accent:",
                "--warning:",
                "--danger:",
                "--success:",
            ] {
                assert!(css.contains(name), "{shell} lost {name}: {css}");
            }
        }
    }

    #[test]
    fn the_console_root_uses_the_console_palette_and_tokens() {
        let display = DisplayConfig::default();
        let console = root_style(Shell::Console, &display);
        let desktop = root_style(Shell::Desktop, &display);
        assert!(console.contains(&format!("--bg:{};", design::CONSOLE_BACKGROUND.hex())));
        assert!(desktop.contains(&format!("--bg:{};", design::BACKGROUND.hex())));
        assert!(console.contains("--text-micro:18px;"));
        assert!(desktop.contains("--text-micro:11px;"));
        assert!(console.contains("--hit-min:48px;"));
        assert!(desktop.contains("--hit-min:28px;"));
    }

    #[test]
    fn the_hit_floor_is_named_inline_by_the_interactive_styles() {
        // The floor cannot be a global rule (the `<style>` element is ignored),
        // so it travels inline; these are the two shared control styles.
        assert_eq!(
            HIT_FLOOR,
            "min-height:var(--hit-min);min-width:var(--hit-min);"
        );
        let display = DisplayConfig::default();
        let console = root_style(Shell::Console, &display);
        assert!(
            console.contains("--hit-min:48px;"),
            "the token the floor reads"
        );
    }

    #[test]
    fn no_screen_ships_motion() {
        // Safe mode promises "animations and blur off". There is no global
        // stylesheet to switch them off with, so the guarantee is by
        // construction: nothing animates. A motion property is allowed only
        // with a `none`/`auto` value (the written rule itself); a real value
        // (`animation:spin`) fails this guard.
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let benign = |value: &str| value.starts_with("none") || value.starts_with("auto");
        for entry in std::fs::read_dir(&source).expect("read the app sources") {
            let path = entry.expect("a source entry").path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read a source file");
            // Production code only: a test may name a motion token to assert on.
            let text = text.split("#[cfg(test)]").next().unwrap_or(&text);
            for needle in [
                "animation:",
                "transition:",
                "backdrop-filter:",
                "scroll-behavior:",
            ] {
                for (index, _) in text.match_indices(needle) {
                    let value = &text[index + needle.len()..];
                    assert!(benign(value), "{} ships motion ({needle})", path.display());
                }
            }
        }
    }

    #[test]
    fn no_screen_ships_a_hover_only_affordance() {
        // A 10-foot shell has no pointer, so an action that exists only while
        // the mouse rests on a control is unreachable from a couch. There are
        // no hover rules today; this keeps it that way.
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let hover_selector = concat!(":", "hover");
        let enter = concat!("onmouse", "enter");
        let over = concat!("onmouse", "over");
        for entry in std::fs::read_dir(&source).expect("read the app sources") {
            let path = entry.expect("a source entry").path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read a source file");
            for needle in [hover_selector, enter, over] {
                assert!(
                    !text.contains(needle),
                    "{} uses a pointer-only affordance: {needle}",
                    path.display()
                );
            }
        }
    }
}
