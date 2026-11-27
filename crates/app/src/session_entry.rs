//! The console session files, generated at startup.
//!
//! A packaged install ships `/usr/share/wayland-sessions/pitagames.desktop` and
//! `/usr/lib/systemd/user/pitagames-session.service` (see `packaging/`); a
//! checkout does not, so the launcher writes the same files into its own XDG
//! directories on every start, generated from the display config. That keeps the
//! session honest: change `[ui.display.session]` and the next launch regenerates
//! the entry, so the gamescope resolution is never a stale hardcoded value.
//!
//! The entry's `Exec` starts gamescope around the launcher's systemd unit; the
//! unit runs *this* binary with `--console --session` and `Restart=on-failure`,
//! so a crash is restarted inside the running session.
//!
//! Two cases skip the write, like the desktop entry:
//!
//! * a packaged file exists in the XDG data directories / systemd unit
//!   directories (a user copy would shadow it),
//! * `PITAGAMES_NO_SESSION_ENTRY` is set (packagers and tests that own the dirs).
//!
//! The files are a convenience: every failure is a warning on stderr, never a
//! reason to keep the window from opening.

use std::env;

use pitagames_core::desktop::InstallOutcome;
use pitagames_core::session::{self, SessionEntry, SystemdUnit};
use pitagames_core::ui::DisplayConfig;

/// Set this to any non-empty value to stop the launcher writing the session files.
pub const DISABLE_ENV: &str = "PITAGAMES_NO_SESSION_ENTRY";

/// Write (or refresh) the user's console session files, when the launcher owns
/// them.
pub fn ensure(display: &DisplayConfig) {
    if env::var_os(DISABLE_ENV).is_some_and(|value| !value.is_empty()) {
        return;
    }
    ensure_entry(display);
    ensure_unit();
}

/// Write the wayland-sessions entry, generated from the display config.
fn ensure_entry(display: &DisplayConfig) {
    let Some(path) = pitagames_core::paths::session_entry_file() else {
        eprintln!("[pitagames] no data directory; skipping the console session entry");
        return;
    };
    if session::packaged_entry(&pitagames_core::paths::data_dirs()) {
        return;
    }
    match SessionEntry::default_for(display).install(&path) {
        Ok(InstallOutcome::Installed) => {
            eprintln!("[pitagames] wrote {}", path.display());
        }
        Ok(InstallOutcome::Unchanged) => {}
        Err(error) => {
            eprintln!(
                "[pitagames] could not write the console session entry {}: {error}",
                path.display()
            );
        }
    }
}

/// Write the systemd user unit, pointing at the running binary.
fn ensure_unit() {
    let Some(path) = pitagames_core::paths::session_unit_file() else {
        eprintln!("[pitagames] no config directory; skipping the console session unit");
        return;
    };
    if session::packaged_unit(&pitagames_core::paths::systemd_user_unit_dirs()) {
        return;
    }
    let exec = match env::current_exe() {
        Ok(exe) => vec![
            exe.to_string_lossy().into_owned(),
            session::CONSOLE_FLAG.to_string(),
            session::SESSION_FLAG.to_string(),
        ],
        Err(error) => {
            eprintln!(
                "[pitagames] could not locate this binary for the console session unit: {error}"
            );
            return;
        }
    };
    match SystemdUnit::default_for(exec).install(&path) {
        Ok(InstallOutcome::Installed) => {
            eprintln!("[pitagames] wrote {}", path.display());
        }
        Ok(InstallOutcome::Unchanged) => {}
        Err(error) => {
            eprintln!(
                "[pitagames] could not write the console session unit {}: {error}",
                path.display()
            );
        }
    }
}
