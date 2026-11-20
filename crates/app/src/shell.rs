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

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::monitor::MonitorHandle;
use dioxus::desktop::tao::window::{Fullscreen, WindowBuilder};
use dioxus::prelude::*;
use pitagames_core::ui::{
    AppConfig, DESKTOP_HEIGHT, DESKTOP_MIN_HEIGHT, DESKTOP_MIN_WIDTH, DESKTOP_WIDTH, DisplayConfig,
    Shell,
};

/// The base type size of each shell, before the UI scale is applied.
pub const DESKTOP_BASE_PX: f32 = 16.0;
pub const CONSOLE_BASE_PX: f32 = 20.0;

/// The shell the command line asked for, plus anything it did not understand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellArgs {
    /// `--console` or `--desktop`; `None` means "use the preference".
    pub shell: Option<Shell>,
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
    /// The display numbers both shells read.
    pub display: DisplayConfig,
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
    /// The desktop shell's frame: decorated, 1280×800, resizable.
    pub fn desktop() -> Self {
        Self {
            title: "PitaGames".to_string(),
            fullscreen: false,
            decorations: true,
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
/// A named monitor cannot be resolved before the window exists (monitor handles
/// come from the event loop), so startup opens on the primary monitor; the
/// preference is honoured by the first runtime toggle.
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
    if config.fullscreen {
        builder = builder.with_fullscreen(Some(Fullscreen::Borderless(None)));
    }
    if config.maximized {
        builder = builder.with_maximized(true);
    }
    builder
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
/// `padding` would survive the switch back to the desktop shell.
pub fn root_style(shell: Shell, display: &DisplayConfig) -> String {
    let padding = if shell.is_console() {
        display.safe_area_css()
    } else {
        "0".to_string()
    };
    let background = if shell.is_console() {
        "#0b0b10"
    } else {
        "#14141c"
    };
    format!(
        "--ui-scale:{};box-sizing:border-box;display:flex;flex-direction:column;height:100vh;overflow:hidden;padding:{padding};background:{background};color:#e8e8ee;font-family:sans-serif;font-size:{}px;",
        display.ui_scale,
        base_font_px(shell, display)
    )
}

/// The app header's inline style for a shell.
///
/// The header's type scales with `--ui-scale`, so the 10-foot shell reads from a
/// couch without the cards' fixed type sizes being revisited yet.
pub fn header_style(shell: Shell, display: &DisplayConfig) -> String {
    let base = if shell.is_console() {
        CONSOLE_BASE_PX
    } else {
        DESKTOP_BASE_PX
    };
    let title = base * 1.375 * display.ui_scale;
    let subtitle = base * 0.75 * display.ui_scale;
    format!(
        "display:flex;align-items:baseline;gap:16px;padding:20px 32px 12px;--title-size:{title}px;font-size:{subtitle}px;"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_desktop_frame_is_decorated_and_windowed() {
        let config = window_config(Shell::Desktop, &DisplayConfig::default());
        assert!(!config.fullscreen);
        assert!(config.decorations);
        assert!(!config.maximized);
        assert_eq!(config.width, 1_280);
        assert_eq!(config.height, 800);
        assert_eq!(config.min_width, 900);
        assert_eq!(config.min_height, 560);
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
    fn the_header_carries_the_scaled_title_size() {
        let display = DisplayConfig::default();
        let header = header_style(Shell::Console, &display);
        assert!(header.contains("--title-size:27.5px;"));
    }
}
