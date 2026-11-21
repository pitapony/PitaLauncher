//! Safe mode: the escape hatch that keeps a broken GPU from leaving a black
//! screen with no keyboard.
//!
//! WebKitGTK composites through DMABUF by default, which is the fast path on
//! the RX 9070 but can fail to initialize on a driver that does not support it.
//! When initialization fails the window never appears, and a window the user
//! cannot see is a window the user cannot close. Safe mode turns that path off
//! — `WEBKIT_DISABLE_DMABUF_RENDERER=1` — and drops animations and blur with
//! it, because those are what the compositor was doing in the first place.
//!
//! Three ways in, all of which the same run honours:
//!
//! * `--safe-mode` on the command line (this run only),
//! * `[ui] safe_mode = true` in `config.toml` (sticks across runs),
//! * `PITAGAMES_SAFE_MODE=1` in the environment (what the supervisor sets when
//!   it retries after a webview init failure).
//!
//! This module holds the environment names and the pure decision; the process
//! watching that uses them is [`crate::supervisor`].

use std::path::PathBuf;

/// Marks the process the supervisor spawned as the one that owns the webview.
///
/// The child skips the instance lock and the supervisor loop so the two roles
/// never nest.
pub const CHILD_ENV: &str = "PITAGAMES_GUI_CHILD";

/// Forces safe mode for the child the supervisor spawns.
pub const SAFE_MODE_ENV: &str = "PITAGAMES_SAFE_MODE";

/// The file the GUI writes once its window is up, so the supervisor can tell a
/// webview init failure from a normal exit.
pub const READY_FILE_ENV: &str = "PITAGAMES_READY_FILE";

/// A test hook: pretend the webview failed to initialize. The CI smoke run
/// forces this to exercise the safe-mode retry without a broken GPU. It only
/// fires when safe mode is *off*, so the retried (safe) run still succeeds.
pub const FAKE_FAILURE_ENV: &str = "PITAGAMES_FAKE_WEBVIEW_FAILURE";

/// The WebKitGTK switch safe mode sets. Disables the DMABUF compositing path.
pub const DMABUF_ENV: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";

/// Whether a string reads as a boolean switch (like the shell's own env flags).
pub fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Whether safe mode is on, from the flag, the config and the environment.
///
/// Pure so it can be tested without touching the process environment; the
/// environment argument is the raw value of [`SAFE_MODE_ENV`].
pub fn enabled_from(flag: bool, configured: bool, environment: Option<&str>) -> bool {
    flag || configured || environment.is_some_and(is_truthy)
}

/// Whether safe mode is on for this process.
pub fn enabled(flag: bool, configured: bool) -> bool {
    enabled_from(
        flag,
        configured,
        std::env::var(SAFE_MODE_ENV).ok().as_deref(),
    )
}

/// Whether this process is the supervisor's GUI child.
pub fn is_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// The ready file the supervisor asked for, if any.
pub fn ready_path() -> Option<PathBuf> {
    std::env::var_os(READY_FILE_ENV).map(PathBuf::from)
}

/// Whether the webview-failure test hook is set.
pub fn fake_webview_failure() -> bool {
    std::env::var_os(FAKE_FAILURE_ENV).is_some()
}

/// Set the WebKit switch that disables the DMABUF renderer.
///
/// Must run before the webview is created. The name and value are fixed ASCII,
/// so this cannot panic.
pub fn apply_webkit_env() {
    // SAFETY: called from `run_gui`, before `LaunchBuilder` starts the GTK event
    // loop, and before any thread that reads the environment is spawned. This is
    // the one place the launcher mutates its own environment.
    unsafe { std::env::set_var(DMABUF_ENV, "1") };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_of_the_three_switches_turns_safe_mode_on() {
        assert!(!enabled_from(false, false, None));
        assert!(enabled_from(true, false, None), "--safe-mode");
        assert!(enabled_from(false, true, None), "config");
        assert!(enabled_from(false, false, Some("1")), "environment");
        assert!(enabled_from(false, false, Some("true")));
        assert!(enabled_from(false, false, Some("Yes")));
        assert!(enabled_from(false, false, Some("on")));
    }

    #[test]
    fn a_falsy_environment_value_does_not_turn_it_on() {
        for value in ["0", "false", "no", "off", "", "  "] {
            assert!(!enabled_from(false, false, Some(value)), "{value:?}");
        }
    }

    #[test]
    fn the_webkit_env_name_is_the_documented_switch() {
        assert_eq!(DMABUF_ENV, "WEBKIT_DISABLE_DMABUF_RENDERER");
        assert_eq!(SAFE_MODE_ENV, "PITAGAMES_SAFE_MODE");
        assert_eq!(CHILD_ENV, "PITAGAMES_GUI_CHILD");
    }

    #[test]
    fn truthy_matches_the_other_env_flags() {
        assert!(is_truthy(" ON "));
        assert!(!is_truthy("nope"));
    }
}
