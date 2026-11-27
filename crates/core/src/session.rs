//! The console session: its login entry and its systemd user unit.
//!
//! The v1.1 console session is two generated files that ship with the package
//! and that a checkout regenerates for itself:
//!
//! * a **wayland-sessions entry** (`/usr/share/wayland-sessions/pitagames.desktop`
//!   when packaged; `$XDG_DATA_HOME/wayland-sessions/pitagames.desktop` for a
//!   checkout) whose `Exec` starts `gamescope` around the launcher — so a game
//!   rendered by the launcher lands in the *same* compositor, with no nested
//!   gamescope;
//! * a **systemd user unit** (`pitagames-session.service`) running
//!   `pitagames --console --session` with `Restart=on-failure`, so a launcher
//!   crash is restarted inside the running session.
//!
//! The entry is generated from [`DisplayConfig`]: the session resolution (only
//! when configured), the chosen monitor, HDR and VRR become the gamescope
//! arguments. With no configured resolution the output's preferred mode is used,
//! never a hardcoded one.
//!
//! Like [`crate::desktop`], the rendered text is a contract: the checked-in
//! `packaging/pitagames-session.desktop` and `packaging/pitagames-session.service`
//! are the default rendering byte for byte, and tests assert it.

use std::path::Path;

use crate::desktop::{self, DesktopEntryError, InstallOutcome};
use crate::ui::DisplayConfig;

/// The launcher binary a packaged session runs (a checkout uses its own path).
pub const LAUNCHER_PROGRAM: &str = "pitagames";
/// The compositor the console session runs.
pub const GAMESCOPE_PROGRAM: &str = "gamescope";
/// The flag that forces the console shell.
pub const CONSOLE_FLAG: &str = "--console";
/// The flag that marks a run as the console session.
pub const SESSION_FLAG: &str = "--session";
/// The environment variable that suppresses a per-game gamescope wrapper.
pub const SESSION_ENV: &str = "PITAGAMES_SESSION";
/// The systemd user unit's file name (and unit name).
pub const UNIT_NAME: &str = "pitagames-session.service";
/// The wayland-sessions entry's file name.
pub const ENTRY_FILE_NAME: &str = "pitagames.desktop";

/// The launcher's argv as a packaged session runs it.
pub fn launcher_argv() -> Vec<String> {
    vec![
        LAUNCHER_PROGRAM.to_string(),
        CONSOLE_FLAG.to_string(),
        SESSION_FLAG.to_string(),
    ]
}

/// The gamescope arguments (no program name) that wrap the launcher.
///
/// The display config's session numbers become flags: a configured resolution
/// becomes `-w`/`-h` (`-r` when a refresh is set), HDR `--hdr-enabled`, VRR
/// `--adaptive-sync`, and the chosen monitor `--prefer-output`. Nothing is
/// emitted for a value that was not configured, so the default session never
/// pins a hardcoded resolution.
pub fn gamescope_arguments(display: &DisplayConfig) -> Vec<String> {
    let mut args = vec![
        "--fullscreen".to_string(),
        "--expose-wayland".to_string(),
        "--force-windows-fullscreen".to_string(),
    ];

    if let Some(mode) = display.session.resolution {
        args.push("-w".to_string());
        args.push(mode.width.to_string());
        args.push("-h".to_string());
        args.push(mode.height.to_string());
        if let Some(refresh) = mode.refresh_hz {
            args.push("-r".to_string());
            args.push(refresh.to_string());
        }
    }

    if display.session.vrr {
        args.push("--adaptive-sync".to_string());
    }
    if display.session.hdr {
        args.push("--hdr-enabled".to_string());
    }

    if let Some(monitor) = display
        .monitor
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        args.push("--prefer-output".to_string());
        args.push(monitor.to_string());
    }

    args
}

/// The full `Exec` argv of the console session entry.
///
/// `gamescope` runs the launcher's systemd unit, so the launcher — and every
/// game it starts — is a client of this one compositor: no nested gamescope.
pub fn session_exec(display: &DisplayConfig) -> Vec<String> {
    let mut argv = vec![GAMESCOPE_PROGRAM.to_string()];
    argv.extend(gamescope_arguments(display));
    argv.push("--".to_string());
    argv.extend([
        "systemctl".to_string(),
        "--user".to_string(),
        "start".to_string(),
        "--wait".to_string(),
        UNIT_NAME.to_string(),
    ]);
    argv
}

/// Whether a packaged session entry exists in one of the XDG data directories
/// (`/usr/share/wayland-sessions`, …).
///
/// A user-level entry shadows a packaged one, so the launcher only generates its
/// own when the package did not install one.
pub fn packaged_entry(data_dirs: &[std::path::PathBuf]) -> bool {
    data_dirs
        .iter()
        .any(|dir| dir.join("wayland-sessions").join(ENTRY_FILE_NAME).is_file())
}

/// Whether a packaged unit exists in one of the systemd user unit directories.
pub fn packaged_unit(unit_dirs: &[std::path::PathBuf]) -> bool {
    unit_dirs.iter().any(|dir| dir.join(UNIT_NAME).is_file())
}

/// The generated wayland-sessions entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// `Name`, shown in the display manager's session list.
    pub name: String,
    /// `Comment`, the session's one-line description.
    pub comment: String,
    /// `Exec`: the argv the display manager runs.
    pub exec: Vec<String>,
}

impl SessionEntry {
    /// The launcher's own session entry, generated from the display config.
    pub fn default_for(display: &DisplayConfig) -> Self {
        Self {
            name: "PitaGames (console)".to_string(),
            comment: "Boot straight into the PitaGames console shell under gamescope".to_string(),
            exec: session_exec(display),
        }
    }

    /// Render the file contents (always newline-terminated).
    pub fn render(&self) -> Result<String, SessionError> {
        single_line("Name", &self.name)?;
        single_line("Comment", &self.comment)?;
        if self.exec.is_empty() {
            return Err(SessionError::Empty { field: "Exec" });
        }

        let mut out = String::new();
        out.push_str("[Desktop Entry]\nType=Application\nVersion=");
        out.push_str(desktop::DESKTOP_SPEC_VERSION);
        out.push('\n');
        out.push_str("Name=");
        out.push_str(&desktop::escape_string(&self.name));
        out.push('\n');
        out.push_str("Comment=");
        out.push_str(&desktop::escape_string(&self.comment));
        out.push('\n');
        out.push_str("Exec=");
        out.push_str(&desktop::render_exec(&self.exec)?);
        out.push('\n');
        Ok(out)
    }

    /// Write the entry to `path` atomically.
    pub fn install(&self, path: impl AsRef<Path>) -> Result<InstallOutcome, SessionError> {
        install_text(path.as_ref(), &self.render()?)
    }

    /// Remove the entry. `Ok(false)` means it was already gone.
    pub fn remove(path: impl AsRef<Path>) -> Result<bool, SessionError> {
        remove_file(path.as_ref())
    }
}

/// The generated systemd user unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdUnit {
    /// `Description`, shown by `systemctl status`.
    pub description: String,
    /// `ExecStart`: the launcher argv the unit runs.
    pub exec: Vec<String>,
}

impl SystemdUnit {
    /// The launcher's own unit, running `exec` under systemd.
    pub fn default_for(exec: Vec<String>) -> Self {
        Self {
            description: "PitaGames console session".to_string(),
            exec,
        }
    }

    /// Render the file contents (always newline-terminated).
    pub fn render(&self) -> Result<String, SessionError> {
        single_line("Description", &self.description)?;
        if self.exec.is_empty() {
            return Err(SessionError::Empty { field: "ExecStart" });
        }
        if self.exec[0].trim().is_empty() {
            return Err(SessionError::Empty {
                field: "ExecStart (program)",
            });
        }
        for argument in &self.exec {
            if argument.contains(['\n', '\r', '\0']) {
                return Err(SessionError::Multiline { field: "ExecStart" });
            }
        }

        let mut out = String::new();
        out.push_str("[Unit]\nDescription=");
        out.push_str(&self.description);
        out.push('\n');
        out.push_str("After=graphical-session.target\nPartOf=graphical-session.target\n\n");
        out.push_str("[Service]\nType=simple\nExecStart=");
        out.push_str(&render_systemd_exec(&self.exec));
        out.push('\n');
        out.push_str("Environment=");
        out.push_str(SESSION_ENV);
        out.push_str("=1\n");
        out.push_str("Restart=on-failure\nRestartSec=2\n\n");
        out.push_str("[Install]\nWantedBy=graphical-session.target\n");
        Ok(out)
    }

    /// Write the unit to `path` atomically.
    pub fn install(&self, path: impl AsRef<Path>) -> Result<InstallOutcome, SessionError> {
        install_text(path.as_ref(), &self.render()?)
    }

    /// Remove the unit. `Ok(false)` means it was already gone.
    pub fn remove(path: impl AsRef<Path>) -> Result<bool, SessionError> {
        remove_file(path.as_ref())
    }
}

/// Refuse a blank or multi-line field.
fn single_line(field: &'static str, value: &str) -> Result<(), SessionError> {
    if value.trim().is_empty() {
        return Err(SessionError::Empty { field });
    }
    if value.contains(['\n', '\r', '\0']) {
        return Err(SessionError::Multiline { field });
    }
    Ok(())
}

/// Render a systemd `ExecStart` value.
fn render_systemd_exec(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| systemd_quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote one `ExecStart` argument the way systemd's command parser expects.
///
/// An argument with no whitespace or shell-ish characters is written bare;
/// anything else is double-quoted with `\` and `"` backslash-escaped.
fn systemd_quote(argument: &str) -> String {
    const RESERVED: [char; 5] = [' ', '\t', '"', '\'', '\\'];
    if !argument.is_empty() && !argument.contains(RESERVED) {
        return argument.to_string();
    }
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    for character in argument.chars() {
        if matches!(character, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

/// Write `contents` to `path` atomically, unless it is already there.
fn install_text(path: &Path, contents: &str) -> Result<InstallOutcome, SessionError> {
    match std::fs::read(path) {
        Ok(existing) if existing == contents.as_bytes() => return Ok(InstallOutcome::Unchanged),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(SessionError::Io {
                action: "read",
                path: path.to_path_buf(),
                source,
            });
        }
    }
    desktop::atomic_write(path, contents.as_bytes())?;
    Ok(InstallOutcome::Installed)
}

/// Remove a file, distinguishing "already gone" from an I/O failure.
fn remove_file(path: &Path) -> Result<bool, SessionError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(SessionError::Io {
            action: "remove",
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Failures rendering or writing the session files.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A required field was blank.
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    /// A field that must stay on one line contains a line break.
    #[error("{field} must be a single line")]
    Multiline { field: &'static str },
    /// A desktop-entry render error.
    #[error(transparent)]
    Desktop(#[from] DesktopEntryError),
    /// A filesystem failure.
    #[error("failed to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{DisplayConfig, SessionDisplay, SessionResolution};

    fn pinned(width: u32, height: u32, refresh: Option<u32>) -> DisplayConfig {
        DisplayConfig {
            session: SessionDisplay {
                resolution: Some(SessionResolution {
                    width,
                    height,
                    refresh_hz: refresh,
                }),
                hdr: false,
                vrr: false,
            },
            ..DisplayConfig::default()
        }
    }

    fn packaged_entry_text() -> String {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/pitagames-session.desktop"
        ))
        .to_string()
    }

    fn packaged_unit_text() -> String {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/pitagames-session.service"
        ))
        .to_string()
    }

    #[test]
    fn the_default_session_has_no_hardcoded_resolution() {
        let argv = session_exec(&DisplayConfig::default());
        assert_eq!(argv[0], GAMESCOPE_PROGRAM);
        // The plan's gamescope flags, so the session takes over the output and
        // exposes a Wayland socket to the launcher it starts.
        for flag in [
            "--fullscreen",
            "--expose-wayland",
            "--force-windows-fullscreen",
        ] {
            assert!(
                argv.iter().any(|arg| arg == flag),
                "missing {flag}: {argv:?}"
            );
        }
        // Nothing pins a mode or a feature that was not configured.
        for absent in [
            "-w",
            "-h",
            "-r",
            "--adaptive-sync",
            "--hdr-enabled",
            "--prefer-output",
        ] {
            assert!(!argv.iter().any(|arg| arg == absent), "unexpected {absent}");
        }
    }

    #[test]
    fn the_session_wraps_the_unit_and_not_a_nested_launcher() {
        let argv = session_exec(&DisplayConfig::default());
        // The launcher runs as the systemd unit, inside gamescope, so a game
        // renders in the same compositor with no nested gamescope.
        let separator = argv.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            &argv[separator + 1..],
            ["systemctl", "--user", "start", "--wait", UNIT_NAME]
        );
        assert!(
            !argv.iter().any(|arg| arg == LAUNCHER_PROGRAM),
            "the entry must not start a second compositor around the launcher: {argv:?}"
        );
    }

    #[test]
    fn a_configured_resolution_drives_the_gamescope_flags() {
        let args = gamescope_arguments(&pinned(2560, 1440, Some(120)));
        assert!(
            args.windows(2).any(|pair| pair == ["-w", "2560"]),
            "{args:?}"
        );
        assert!(
            args.windows(2).any(|pair| pair == ["-h", "1440"]),
            "{args:?}"
        );
        assert!(
            args.windows(2).any(|pair| pair == ["-r", "120"]),
            "{args:?}"
        );

        // A mode with no refresh omits `-r`.
        let no_refresh = gamescope_arguments(&pinned(1920, 1080, None));
        assert!(!no_refresh.iter().any(|arg| arg == "-r"), "{no_refresh:?}");
    }

    #[test]
    fn hdr_and_vrr_map_to_their_flags() {
        let display = DisplayConfig {
            session: SessionDisplay {
                resolution: None,
                hdr: true,
                vrr: true,
            },
            ..DisplayConfig::default()
        };
        let args = gamescope_arguments(&display);
        assert!(args.iter().any(|arg| arg == "--hdr-enabled"), "{args:?}");
        assert!(args.iter().any(|arg| arg == "--adaptive-sync"), "{args:?}");
    }

    #[test]
    fn the_monitor_selects_the_preferred_output() {
        let display = DisplayConfig {
            monitor: Some("HDMI-A-1".to_string()),
            ..DisplayConfig::default()
        };
        let args = gamescope_arguments(&display);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--prefer-output", "HDMI-A-1"]),
            "{args:?}"
        );
    }

    #[test]
    fn the_entry_renders_a_wayland_session() {
        let rendered = SessionEntry::default_for(&DisplayConfig::default())
            .render()
            .unwrap();
        assert!(rendered.starts_with("[Desktop Entry]\nType=Application\n"));
        assert!(
            rendered.contains("Name=PitaGames (console)\n"),
            "{rendered}"
        );
        assert!(rendered.contains("Exec=gamescope "), "{rendered}");
        assert!(
            rendered.contains("systemctl --user start --wait pitagames-session.service"),
            "{rendered}"
        );
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn the_unit_runs_the_console_session_and_restarts() {
        let rendered = SystemdUnit::default_for(launcher_argv()).render().unwrap();
        assert!(rendered.contains("[Unit]\n"), "{rendered}");
        assert!(rendered.contains("[Service]\n"), "{rendered}");
        assert!(
            rendered.contains("ExecStart=pitagames --console --session\n"),
            "{rendered}"
        );
        assert!(rendered.contains("Restart=on-failure\n"), "{rendered}");
        assert!(
            rendered.contains("Environment=PITAGAMES_SESSION=1\n"),
            "{rendered}"
        );
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn a_launcher_path_with_spaces_is_quoted() {
        let unit = SystemdUnit::default_for(vec![
            "/home/two words/pitagames".to_string(),
            CONSOLE_FLAG.to_string(),
            SESSION_FLAG.to_string(),
        ]);
        let rendered = unit.render().unwrap();
        assert!(
            rendered.contains("ExecStart=\"/home/two words/pitagames\" --console --session\n"),
            "{rendered}"
        );
    }

    #[test]
    fn an_empty_launcher_is_refused() {
        let unit = SystemdUnit::default_for(Vec::new());
        assert!(matches!(unit.render(), Err(SessionError::Empty { .. })));
    }

    #[test]
    fn the_packaged_session_files_match_the_renderer() {
        // The PKGBUILD installs the checked-in copies; a checkout regenerates
        // the same bytes. They can never drift.
        let entry = SessionEntry::default_for(&DisplayConfig::default())
            .render()
            .unwrap();
        assert_eq!(entry, packaged_entry_text());
        let unit = SystemdUnit::default_for(launcher_argv()).render().unwrap();
        assert_eq!(unit, packaged_unit_text());
    }

    #[test]
    fn installing_writes_once_and_then_reports_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wayland-sessions").join(ENTRY_FILE_NAME);
        let entry = SessionEntry::default_for(&DisplayConfig::default());

        assert_eq!(entry.install(&path).unwrap(), InstallOutcome::Installed);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            entry.render().unwrap()
        );
        assert_eq!(entry.install(&path).unwrap(), InstallOutcome::Unchanged);
        assert!(SessionEntry::remove(&path).unwrap());
        assert!(!path.exists());
    }

    /// The TV checklist is the human gate for the session and the EAC entry.
    ///
    /// The docs counterpart of the capability/nr docs-agreement tests: the
    /// checklist must keep covering every acceptance topic of the console
    /// session (resolution, HDR, VRR, overscan, focus return) and the EAC-entry
    /// interaction, and it must keep the "not tested on a TV" marker until a
    /// community tester passes it (ADR-0004).
    #[test]
    fn the_tv_checklist_covers_the_session_and_the_eac_entry() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("docs/console-mode.md");
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} is unreadable: {error}", path.display()));

        // The marker stays until a community tester runs the checklist (ADR-0004).
        assert!(
            doc.contains("not tested on a TV"),
            "docs/console-mode.md must keep the \"not tested on a TV\" marker"
        );

        // The checklist is everything from its heading to the end of the file.
        let checklist = doc
            .split_once("## TV checklist")
            .expect("docs/console-mode.md must carry a `## TV checklist` section")
            .1
            .to_ascii_lowercase();

        for (topic, needle) in [
            ("resolution switching", "resolution"),
            ("HDR", "hdr"),
            ("VRR / adaptive-sync", "vrr"),
            ("overscan / safe area", "overscan"),
            ("focus return", "focus"),
            ("the v1.1 login-screen session", "console session"),
            ("the EAC-entry interaction", "eac entry"),
            ("the DKMS caveat", "dkms"),
        ] {
            assert!(
                checklist.contains(needle),
                "the TV checklist must cover {topic} (`{needle}`)"
            );
        }
    }

    #[test]
    fn a_packaged_session_file_is_found_in_any_directory() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        let share = dir.path().join("share");
        std::fs::create_dir_all(share.join("wayland-sessions")).unwrap();
        std::fs::create_dir_all(share.join("systemd/user")).unwrap();

        assert!(!packaged_entry(std::slice::from_ref(&empty)));
        assert!(!packaged_unit(std::slice::from_ref(&empty)));

        std::fs::write(share.join("wayland-sessions").join(ENTRY_FILE_NAME), "x").unwrap();
        std::fs::write(share.join("systemd/user").join(UNIT_NAME), "x").unwrap();
        assert!(packaged_entry(std::slice::from_ref(&share)));
        assert!(packaged_unit(&[share.join("systemd/user")]));
    }
}
