//! The application-menu entry, generated at startup.
//!
//! A packaged install ships `/usr/share/applications/pitagames.desktop` (see
//! `packaging/pitagames.desktop`); a checkout does not, so the launcher writes
//! the same entry into `~/.local/share/applications` on every start. That keeps
//! the `Exec` line pointing at the binary that is actually running — a rebuilt
//! or moved checkout still launches from the menu.
//!
//! Three cases skip the write:
//!
//! * a packaged entry exists in one of the XDG data directories (a user entry
//!   would shadow it),
//! * `PITAGAMES_NO_DESKTOP_ENTRY` is set (packagers and tests that own the
//!   applications directory),
//! * there is no data directory (`HOME` unset).
//!
//! The entry is a convenience: every failure is a warning on stderr, never a
//! reason to keep the window from opening.

use std::env;

use pitagames_core::desktop::{self, DesktopEntry, InstallOutcome};

/// Set this to any non-empty value to stop the launcher writing a user entry.
pub const DISABLE_ENV: &str = "PITAGAMES_NO_DESKTOP_ENTRY";

/// Write (or refresh) the user's `.desktop` entry, when the launcher owns it.
pub fn ensure() {
    if env::var_os(DISABLE_ENV).is_some_and(|value| !value.is_empty()) {
        return;
    }

    let Some(path) = pitagames_core::paths::desktop_entry_file() else {
        eprintln!("[pitagames] no data directory; skipping the desktop entry");
        return;
    };

    if desktop::packaged_entry(&pitagames_core::paths::data_dirs()) {
        // The package's entry is the one to launch; a user-level copy would win
        // over it and could point at a stale path.
        return;
    }

    let exec = match env::current_exe() {
        Ok(exe) => vec![exe.to_string_lossy().into_owned()],
        Err(error) => {
            eprintln!("[pitagames] could not locate this binary for the desktop entry: {error}");
            return;
        }
    };

    match DesktopEntry::default_for(exec).install(&path) {
        Ok(InstallOutcome::Installed) => {
            eprintln!("[pitagames] wrote {}", path.display());
        }
        Ok(InstallOutcome::Unchanged) => {}
        Err(error) => {
            eprintln!(
                "[pitagames] could not write the desktop entry {}: {error}",
                path.display()
            );
        }
    }
}
