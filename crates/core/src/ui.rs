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
    /// A windowed, decorated shell for the desktop.
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
