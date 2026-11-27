//! The two shells and the display preferences that pick one.
//!
//! PitaGames ships one product with two shells: a windowed desktop shell and a
//! 10-foot console shell. The shell is a *runtime* choice — the same window is
//! reconfigured when the player toggles — while the preference that picks it at
//! startup is persisted in `<config>/pitagames/config.toml`.
//!
//! This module owns the closed set of values and the file. It is framework-free
//! on purpose: the app renders the CSS and drives the window, but the numbers
//! (safe area, UI scale, the chosen shell) come from here so a headless test can
//! check them without a webview.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The default TV overscan margin for the console shell, in percent of the screen.
pub const DEFAULT_SAFE_AREA_PCT: f32 = 2.5;
/// How long the pointer may sit still before the shell hides it.
pub const DEFAULT_HIDE_CURSOR_MS: u64 = 3_000;
/// The accepted range of the UI scale; a value outside it is clamped.
pub const MIN_UI_SCALE: f32 = 0.75;
pub const MAX_UI_SCALE: f32 = 3.0;
/// The largest safe-area margin accepted, in percent of the screen.
pub const MAX_SAFE_AREA_PCT: f32 = 12.0;
/// The longest cursor-hide delay accepted, in milliseconds.
pub const MAX_HIDE_CURSOR_MS: u64 = 60_000;

/// The default desktop window size.
pub const DESKTOP_WIDTH: f64 = 1_280.0;
pub const DESKTOP_HEIGHT: f64 = 800.0;
/// The smallest size the desktop window may be dragged to.
pub const DESKTOP_MIN_WIDTH: f64 = 900.0;
pub const DESKTOP_MIN_HEIGHT: f64 = 560.0;

/// Which shell the window is in.
///
/// The shell is runtime state: toggling it reconfigures the same window (the
/// decoration and fullscreen flags) rather than launching a second process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    /// A windowed, frameless shell for the desktop.
    #[default]
    Desktop,
    /// A 10-foot, undecorated shell for a TV or a controller.
    Console,
}

impl Shell {
    /// The stable wire name, used in `config.toml` and on the command line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Console => "console",
        }
    }

    /// The name shown to the player.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Desktop => "Desktop",
            Self::Console => "Console",
        }
    }

    /// Whether this is the console shell.
    pub const fn is_console(self) -> bool {
        matches!(self, Self::Console)
    }

    /// The other shell.
    pub const fn toggled(self) -> Self {
        match self {
            Self::Desktop => Self::Console,
            Self::Console => Self::Desktop,
        }
    }
}

impl fmt::Display for Shell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Shell {
    type Err = UnknownShell;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "desktop" | "windowed" | "window" => Ok(Self::Desktop),
            "console" | "tv" | "fullscreen" => Ok(Self::Console),
            _ => Err(UnknownShell(value.to_string())),
        }
    }
}

/// A shell name that is not one of the two.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown shell {0:?}; expected \"desktop\" or \"console\"")]
pub struct UnknownShell(pub String);

/// The persisted preference that decides which shell opens at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartMode {
    /// Always open the windowed shell.
    Desktop,
    /// Always open the console shell.
    Console,
    /// Open the shell the player last used.
    #[default]
    Remember,
}

impl StartMode {
    /// The stable wire name, used in `config.toml`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Console => "console",
            Self::Remember => "remember",
        }
    }

    /// The name shown to the player.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Desktop => "Always desktop",
            Self::Console => "Always console",
            Self::Remember => "Remember last",
        }
    }
}

impl fmt::Display for StartMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for StartMode {
    type Err = UnknownStartMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "desktop" | "windowed" => Ok(Self::Desktop),
            "console" | "tv" => Ok(Self::Console),
            "remember" | "last" => Ok(Self::Remember),
            _ => Err(UnknownStartMode(value.to_string())),
        }
    }
}

/// A start mode that is not one of the three.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown start mode {0:?}; expected \"desktop\", \"console\" or \"remember\"")]
pub struct UnknownStartMode(pub String);

/// A display mode the console session pins gamescope's output to.
///
/// Typed rather than a `"1920x1080"` string so a malformed value is a config
/// error, not a gamescope argument. The session file only emits `-w`/`-h`/`-r`
/// when this is set; with no mode the output's preferred mode is used, so the
/// shipped session never hardcodes a resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResolution {
    /// The output width in pixels.
    pub width: u32,
    /// The output height in pixels.
    pub height: u32,
    /// The refresh rate in Hz; `None` leaves the mode's default.
    pub refresh_hz: Option<u32>,
}

/// The console session's own display preferences.
///
/// The wayland-sessions entry runs `gamescope`, so these numbers become its
/// arguments (see [`crate::session`]). They are separate from the shell's
/// [`DisplayConfig`] numbers because they configure the *compositor*, not the
/// launcher's window.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionDisplay {
    /// The mode to pin the output to; `None` uses the output's preferred mode.
    pub resolution: Option<SessionResolution>,
    /// Whether to ask gamescope for HDR (`--hdr-enabled`).
    pub hdr: bool,
    /// Whether to ask gamescope for adaptive sync / VRR (`--adaptive-sync`).
    pub vrr: bool,
}

/// How the shells lay themselves out.
///
/// The numbers are CSS values (a percentage, a multiplier, a duration), so a
/// shell can consume them without knowing where they came from. Every field has
/// a default, so a hand-written `config.toml` may name only what it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// Whether the console shell goes fullscreen. A false value leaves it a
    /// maximised, undecorated window, which is easier to debug on a desktop.
    pub console_fullscreen: bool,
    /// The monitor connector to open on, e.g. `HDMI-A-1`. `None` means the
    /// primary monitor.
    pub monitor: Option<String>,
    /// A multiplier on the shell's base font size. `1.0` is the design size.
    pub ui_scale: f32,
    /// The overscan margin kept clear on each edge, in percent of the screen.
    pub safe_area_pct: f32,
    /// How long the pointer may sit still before the shell hides it, in
    /// milliseconds. `0` disables hiding.
    pub hide_cursor_ms: u64,
    /// Whether to drop animations and blur (the accessibility switch).
    pub reduce_animations: bool,
    /// The console session's output: the gamescope session file is generated
    /// from these numbers, so a couch user's TV mode is configured once.
    pub session: SessionDisplay,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            console_fullscreen: true,
            monitor: None,
            ui_scale: 1.0,
            safe_area_pct: DEFAULT_SAFE_AREA_PCT,
            hide_cursor_ms: DEFAULT_HIDE_CURSOR_MS,
            reduce_animations: false,
            session: SessionDisplay::default(),
        }
    }
}

impl DisplayConfig {
    /// The safe-area margin as a CSS length, e.g. `2.5vw`.
    pub fn safe_area_css(&self) -> String {
        format!("{}vw", self.safe_area_pct)
    }

    /// The UI scale as a CSS multiplier, e.g. `1.25`.
    pub fn ui_scale_css(&self) -> String {
        format!("{}", self.ui_scale)
    }

    /// Whether the pointer should be hidden after a pause.
    pub fn hides_cursor(&self) -> bool {
        self.hide_cursor_ms > 0
    }

    /// The index of the attached monitor this config names, if any.
    ///
    /// See [`select_monitor`]: `None` is the primary, whether the config names
    /// no monitor or names one that is not attached.
    pub fn select_monitor(&self, monitors: &[MonitorInfo]) -> Option<usize> {
        select_monitor(self.monitor.as_deref(), monitors)
    }

    /// Where a `width × height` window should open.
    ///
    /// See [`placement`].
    pub fn placement(&self, width: u32, height: u32, monitors: &[MonitorInfo]) -> Placement {
        placement(self.monitor.as_deref(), width, height, monitors)
    }
}

/// A monitor the window may open on, in the shape the placement logic needs.
///
/// The geometry is in physical pixels, which is what the window layer reports;
/// the startup placement compares a logical window size against it, so a HiDPI
/// scale factor other than 1 is not accounted for. That is part of the deferred
/// 4K/HiDPI pass (see `docs/m2.5-shells.md`), not a bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// The connector name, e.g. `HDMI-A-1`; `None` when the platform has none.
    pub name: Option<String>,
    /// The monitor's top-left in physical pixels.
    pub x: i32,
    pub y: i32,
    /// The monitor's size in physical pixels.
    pub width: u32,
    pub height: u32,
}

impl MonitorInfo {
    /// Build a monitor from the values the window layer reports.
    pub fn new(name: Option<String>, x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            name,
            x,
            y,
            width,
            height,
        }
    }

    /// The top-left a `width × height` window needs to sit centred here.
    ///
    /// A window larger than the monitor is pinned to the monitor's origin
    /// rather than pushed off its top-left edge.
    pub fn center(&self, width: u32, height: u32) -> (i32, i32) {
        let dx = (self.width.saturating_sub(width) / 2) as i32;
        let dy = (self.height.saturating_sub(height) / 2) as i32;
        (self.x + dx, self.y + dy)
    }
}

/// Where a window should open, resolved against the attached monitors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Leave the window where the window manager puts it: the primary monitor.
    Default,
    /// Move the window's top-left to this physical position so a window is
    /// centred on the chosen monitor.
    Center { x: i32, y: i32 },
}

/// The index of the attached monitor a configured name selects, if any.
///
/// `None` means "the platform default" (the primary): either no name is
/// configured, or the name is not attached. Either way the window is left where
/// the window manager puts it, which is the primary monitor.
pub fn select_monitor(name: Option<&str>, monitors: &[MonitorInfo]) -> Option<usize> {
    let name = name?;
    monitors
        .iter()
        .position(|monitor| monitor.name.as_deref() == Some(name))
}

/// Where a `width × height` window should open.
///
/// The configured monitor wins when it is attached; otherwise (the default, or
/// a name that is not attached) the window opens on the primary, which needs no
/// move.
pub fn placement(
    name: Option<&str>,
    width: u32,
    height: u32,
    monitors: &[MonitorInfo],
) -> Placement {
    match select_monitor(name, monitors) {
        Some(index) => {
            let (x, y) = monitors[index].center(width, height);
            Placement::Center { x, y }
        }
        None => Placement::Default,
    }
}

/// The `[ui]` section of `config.toml`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Which shell opens at startup.
    pub start_mode: StartMode,
    /// The shell the player last used, so `Remember` can restore it.
    pub last_shell: Shell,
    /// Whether to start in safe mode: the DMABUF renderer off, animations and
    /// blur off. `--safe-mode` forces it for one run; this makes it stick, so a
    /// machine that cannot composite can still be rescued by hand-editing the
    /// file (the Settings toggle is a later slice).
    pub safe_mode: bool,
    /// The display numbers both shells read.
    pub display: DisplayConfig,
}

impl UiConfig {
    /// Which shell to open with.
    ///
    /// A command-line choice wins over the persisted preference; otherwise the
    /// start mode decides, and `Remember` replays the last shell.
    pub fn resolve_start(&self, requested: Option<Shell>) -> Shell {
        requested.unwrap_or(match self.start_mode {
            StartMode::Desktop => Shell::Desktop,
            StartMode::Console => Shell::Console,
            StartMode::Remember => self.last_shell,
        })
    }

    /// Record that the player switched shells, so `Remember` opens there next
    /// time. The start mode itself is left alone: an explicit "always console"
    /// stays explicit.
    pub fn remember(&mut self, shell: Shell) {
        self.last_shell = shell;
    }
}

/// The `[compat]` section of `config.toml`.
///
/// The compatibility database itself is data (`platform::compat` reads the
/// shipped `compat/*.toml` and the user overrides); this is only the launcher's
/// preference about it. [`sync`](Self::sync) is the Settings screen's
/// **compat-DB sync** toggle: when on, the launcher keeps a synced copy of the
/// shipped database under `$XDG_DATA_HOME/pitagames/compat` (the copy
/// `platform::compat` already reads beside the package's). It defaults to off —
/// a machine that only reads the packaged database writes nothing.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompatConfig {
    /// Whether to keep (and read) a synced copy of the shipped compat database
    /// under the data directory.
    pub sync: bool,
}

/// The launcher's own `config.toml`.
///
/// One file for the launcher's preferences. It is separate from `input.toml`
/// because the input maps are keyed by device GUID and owned by the
/// framework-free input crate; this file is the window's preferences.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// The shells and the display numbers.
    pub ui: UiConfig,
    /// The compatibility database preference (the sync toggle).
    pub compat: CompatConfig,
}

impl AppConfig {
    /// Read the config, falling back to the defaults on any problem.
    ///
    /// A missing file is normal (the defaults are the shipped behaviour). A
    /// malformed or unreadable file is reported as a warning and the defaults
    /// are used: a typo in a config file must never stop the launcher from
    /// starting. Out-of-range numbers are clamped, and each clamp is a warning.
    pub fn load(path: &Path) -> (Self, Vec<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), Vec::new());
            }
            Err(error) => {
                return (
                    Self::default(),
                    vec![format!("could not read {}: {error}", path.display())],
                );
            }
        };
        match toml::from_str::<Self>(&text) {
            Ok(config) => config.sanitized(),
            Err(error) => (
                Self::default(),
                vec![format!("could not parse {}: {error}", path.display())],
            ),
        }
    }

    /// Read the config from the default XDG path, if there is one.
    pub fn load_default() -> (Self, Vec<String>) {
        match crate::paths::config_file() {
            Some(path) => Self::load(&path),
            None => (Self::default(), Vec::new()),
        }
    }

    /// Write the config atomically: a `.toml.tmp` sibling, then a rename.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self).map_err(|source| ConfigError::Serialize {
            path: path.to_path_buf(),
            source,
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Write the config to the default XDG path.
    pub fn save_default(&self) -> Result<(), ConfigError> {
        let path = crate::paths::config_file().ok_or(ConfigError::NoConfigDir)?;
        self.save(&path)
    }

    /// Clamp the numbers into their accepted ranges, collecting a warning per
    /// change. A config file is hand-edited, so a typo must be reported and
    /// survived rather than obeyed.
    pub fn sanitized(mut self) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let display = &mut self.ui.display;

        if !display.ui_scale.is_finite() {
            warnings.push(format!(
                "ui_scale {} is not a number; using 1.0",
                display.ui_scale
            ));
            display.ui_scale = 1.0;
        }
        if display.ui_scale < MIN_UI_SCALE || display.ui_scale > MAX_UI_SCALE {
            warnings.push(format!(
                "ui_scale {} is outside {MIN_UI_SCALE}..={MAX_UI_SCALE}; clamped",
                display.ui_scale
            ));
            display.ui_scale = display.ui_scale.clamp(MIN_UI_SCALE, MAX_UI_SCALE);
        }

        if !display.safe_area_pct.is_finite() {
            warnings.push(format!(
                "safe_area_pct {} is not a number; using {DEFAULT_SAFE_AREA_PCT}",
                display.safe_area_pct
            ));
            display.safe_area_pct = DEFAULT_SAFE_AREA_PCT;
        }
        if display.safe_area_pct < 0.0 || display.safe_area_pct > MAX_SAFE_AREA_PCT {
            warnings.push(format!(
                "safe_area_pct {} is outside 0..={MAX_SAFE_AREA_PCT}; clamped",
                display.safe_area_pct
            ));
            display.safe_area_pct = display.safe_area_pct.clamp(0.0, MAX_SAFE_AREA_PCT);
        }

        if display.hide_cursor_ms > MAX_HIDE_CURSOR_MS {
            warnings.push(format!(
                "hide_cursor_ms {} is longer than {MAX_HIDE_CURSOR_MS}; clamped",
                display.hide_cursor_ms
            ));
            display.hide_cursor_ms = MAX_HIDE_CURSOR_MS;
        }

        // A mode the compositor cannot use is dropped rather than passed to
        // gamescope: a zero dimension is not a resolution, and a refresh of zero
        // is not a rate.
        if let Some(mode) = display.session.resolution {
            if mode.width == 0 || mode.height == 0 {
                warnings.push(format!(
                    "session resolution {}x{} is not a mode; using the output default",
                    mode.width, mode.height
                ));
                display.session.resolution = None;
            } else if mode.refresh_hz == Some(0) {
                warnings.push(
                    "session resolution refresh 0 is not a rate; using the mode default"
                        .to_string(),
                );
                display.session.resolution = Some(SessionResolution {
                    refresh_hz: None,
                    ..mode
                });
            }
        }

        if display
            .monitor
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            display.monitor = None;
        }

        (self, warnings)
    }
}

/// Something went wrong writing the config.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("config file {path}: {source}")]
    Serialize {
        path: PathBuf,
        source: toml::ser::Error,
    },
    #[error("no config directory (HOME is unset)")]
    NoConfigDir,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_windowed_shell() {
        let config = AppConfig::default();
        assert_eq!(config.ui.start_mode, StartMode::Remember);
        assert_eq!(config.ui.last_shell, Shell::Desktop);
        // Safe mode is off unless asked for.
        assert!(!config.ui.safe_mode);
        // Nothing has been switched yet, so Remember opens the desktop shell.
        assert_eq!(config.ui.resolve_start(None), Shell::Desktop);
        assert!(config.ui.display.console_fullscreen);
        assert_eq!(config.ui.display.ui_scale, 1.0);
        assert_eq!(config.ui.display.safe_area_pct, DEFAULT_SAFE_AREA_PCT);
        assert!(config.ui.display.hides_cursor());
    }

    #[test]
    fn a_command_line_choice_wins_over_the_preference() {
        let mut config = UiConfig {
            start_mode: StartMode::Console,
            ..UiConfig::default()
        };
        assert_eq!(config.resolve_start(None), Shell::Console);
        assert_eq!(config.resolve_start(Some(Shell::Desktop)), Shell::Desktop);

        config.start_mode = StartMode::Remember;
        config.remember(Shell::Console);
        assert_eq!(config.resolve_start(None), Shell::Console);
        assert_eq!(config.resolve_start(Some(Shell::Desktop)), Shell::Desktop);
    }

    #[test]
    fn remember_keeps_the_start_mode_explicit() {
        let mut config = UiConfig {
            start_mode: StartMode::Desktop,
            ..UiConfig::default()
        };
        config.remember(Shell::Console);
        // An explicit "always desktop" is not overridden by the last shell.
        assert_eq!(config.resolve_start(None), Shell::Desktop);
        assert_eq!(config.last_shell, Shell::Console);
    }

    #[test]
    fn the_compat_sync_preference_defaults_off_and_survives_the_file() {
        // The synced copy is opt-in: a machine that only reads the packaged
        // database writes nothing.
        let config = AppConfig::default();
        assert!(!config.compat.sync);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = AppConfig::default();
        config.compat.sync = true;
        config.save(&path).unwrap();
        let (loaded, warnings) = AppConfig::load(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(loaded.compat.sync);

        // A file that names no `[compat]` keeps the default.
        let partial = dir.path().join("partial.toml");
        std::fs::write(&partial, "[ui]\nstart_mode = \"console\"\n").unwrap();
        let (loaded, _) = AppConfig::load(&partial);
        assert!(!loaded.compat.sync);
    }

    #[test]
    fn a_missing_file_is_the_defaults_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (config, warnings) = AppConfig::load(&path);
        assert_eq!(config, AppConfig::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_broken_file_falls_back_to_the_defaults_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not toml [[[").unwrap();
        let (config, warnings) = AppConfig::load(&path);
        assert_eq!(config, AppConfig::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("could not parse"));
    }

    #[test]
    fn a_partial_file_keeps_the_other_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[ui]\nstart_mode = \"console\"\n\n[ui.display]\nui_scale = 1.5\n",
        )
        .unwrap();
        let (config, warnings) = AppConfig::load(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.ui.start_mode, StartMode::Console);
        assert_eq!(config.ui.display.ui_scale, 1.5);
        // The unnamed fields keep their defaults.
        assert!(config.ui.display.console_fullscreen);
        assert_eq!(config.ui.display.safe_area_pct, DEFAULT_SAFE_AREA_PCT);
    }

    #[test]
    fn out_of_range_numbers_are_clamped_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[ui.display]\nui_scale = 9.0\nsafe_area_pct = 40.0\nhide_cursor_ms = 999999\n",
        )
        .unwrap();
        let (config, warnings) = AppConfig::load(&path);
        assert_eq!(config.ui.display.ui_scale, MAX_UI_SCALE);
        assert_eq!(config.ui.display.safe_area_pct, MAX_SAFE_AREA_PCT);
        assert_eq!(config.ui.display.hide_cursor_ms, MAX_HIDE_CURSOR_MS);
        assert_eq!(warnings.len(), 3);
    }

    #[test]
    fn a_not_a_number_scale_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ui.display]\nui_scale = nan\n").unwrap();
        let (config, warnings) = AppConfig::load(&path);
        assert_eq!(config.ui.display.ui_scale, 1.0);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn a_blank_monitor_name_means_the_primary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ui.display]\nmonitor = \"  \"\n").unwrap();
        let (config, _) = AppConfig::load(&path);
        assert_eq!(config.ui.display.monitor, None);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let mut config = AppConfig::default();
        config.ui.start_mode = StartMode::Console;
        config.ui.remember(Shell::Console);
        config.ui.display.ui_scale = 1.25;
        config.ui.display.safe_area_pct = 4.0;
        config.ui.display.monitor = Some("HDMI-A-1".to_string());
        config.ui.display.reduce_animations = true;
        config.ui.safe_mode = true;
        config.compat.sync = true;
        config.save(&path).unwrap();

        let (loaded, warnings) = AppConfig::load(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(loaded, config);
    }

    #[test]
    fn the_shell_names_round_trip() {
        for shell in [Shell::Desktop, Shell::Console] {
            assert_eq!(Shell::from_str(shell.as_str()).unwrap(), shell);
            assert_eq!(shell.toggled().toggled(), shell);
        }
        assert!(Shell::from_str("television").is_err());
        for mode in [StartMode::Desktop, StartMode::Console, StartMode::Remember] {
            assert_eq!(StartMode::from_str(mode.as_str()).unwrap(), mode);
        }
        assert!(StartMode::from_str("sometimes").is_err());
    }

    #[test]
    fn a_configured_monitor_is_selected_by_name() {
        let monitors = [
            MonitorInfo::new(Some("eDP-1".to_string()), 0, 0, 1920, 1080),
            MonitorInfo::new(Some("HDMI-A-1".to_string()), 1920, 0, 2560, 1440),
        ];
        let named = DisplayConfig {
            monitor: Some("HDMI-A-1".to_string()),
            ..DisplayConfig::default()
        };
        assert_eq!(named.select_monitor(&monitors), Some(1));

        // No name means the primary: the platform default, never index 0 by
        // assumption.
        assert_eq!(DisplayConfig::default().select_monitor(&monitors), None);

        // A name that is not attached falls back to the primary too.
        let missing = DisplayConfig {
            monitor: Some("DP-9".to_string()),
            ..DisplayConfig::default()
        };
        assert_eq!(missing.select_monitor(&monitors), None);
    }

    #[test]
    fn placement_centers_the_window_on_the_chosen_monitor() {
        let monitors = [
            MonitorInfo::new(Some("eDP-1".to_string()), 0, 0, 1920, 1080),
            MonitorInfo::new(Some("HDMI-A-1".to_string()), 1920, 0, 2560, 1440),
        ];
        let named = DisplayConfig {
            monitor: Some("HDMI-A-1".to_string()),
            ..DisplayConfig::default()
        };
        // 1280×800 centred on a 2560×1440 monitor at x=1920.
        assert_eq!(
            named.placement(1280, 800, &monitors),
            Placement::Center {
                x: 1920 + (2560 - 1280) / 2,
                y: (1440 - 800) / 2,
            }
        );
        // The primary needs no move: the window manager owns it.
        assert_eq!(
            DisplayConfig::default().placement(1280, 800, &monitors),
            Placement::Default
        );
    }

    #[test]
    fn a_window_larger_than_the_monitor_is_pinned_to_its_origin() {
        let monitor = MonitorInfo::new(Some("small".to_string()), 100, 50, 800, 600);
        assert_eq!(monitor.center(1280, 800), (100, 50));
        // An exact fit is the origin too.
        assert_eq!(monitor.center(800, 600), (100, 50));
    }

    #[test]
    fn the_default_session_pins_nothing() {
        // The shipped console session must never force a resolution: with no
        // session section, gamescope runs the output's preferred mode.
        let display = DisplayConfig::default();
        assert_eq!(display.session.resolution, None);
        assert!(!display.session.hdr);
        assert!(!display.session.vrr);
    }

    #[test]
    fn the_session_section_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[ui.display.session]\nhdr = true\nvrr = true\nresolution = { width = 2560, height = 1440, refresh_hz = 120 }\n",
        )
        .unwrap();
        let (config, warnings) = AppConfig::load(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            config.ui.display.session.resolution,
            Some(SessionResolution {
                width: 2560,
                height: 1440,
                refresh_hz: Some(120),
            })
        );
        assert!(config.ui.display.session.hdr);
        assert!(config.ui.display.session.vrr);

        // Saving writes the section back, so a generated session file survives
        // a reload.
        let saved = dir.path().join("again.toml");
        config.save(&saved).unwrap();
        let (reloaded, warnings) = AppConfig::load(&saved);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(reloaded, config);
    }

    #[test]
    fn a_zero_session_resolution_is_dropped_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[ui.display.session]\nresolution = { width = 0, height = 1440, refresh_hz = 0 }\n",
        )
        .unwrap();
        let (config, warnings) = AppConfig::load(&path);
        // A dimension of zero is not a mode; the whole pin is dropped instead of
        // asking gamescope for an impossible resolution.
        assert_eq!(config.ui.display.session.resolution, None);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("resolution"), "{warnings:?}");
    }

    #[test]
    fn the_css_helpers_format_the_numbers() {
        let display = DisplayConfig {
            safe_area_pct: 2.5,
            ui_scale: 1.25,
            ..DisplayConfig::default()
        };
        assert_eq!(display.safe_area_css(), "2.5vw");
        assert_eq!(display.ui_scale_css(), "1.25");
        assert!(display.hides_cursor());
        let disabled = DisplayConfig {
            hide_cursor_ms: 0,
            ..DisplayConfig::default()
        };
        assert!(!disabled.hides_cursor());
    }
}
