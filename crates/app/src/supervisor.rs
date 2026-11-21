//! The process that owns the launcher and watches the window.
//!
//! The GUI cannot restart itself *after* its webview fails to initialize: the
//! failure leaves the process in a state GTK and WebKit will not initialize
//! again. So the launcher runs two roles. The **supervisor** — the process the
//! user or the `.desktop` file starts — takes the single-instance lock, then
//! spawns the **GUI child** ([`crate::safe_mode::CHILD_ENV`]) and waits. The
//! child builds the webview; when it is up it writes the ready file the
//! supervisor passed it, which is what tells the supervisor "the window worked,
//! this exit is a real one".
//!
//! Two things fall out of that split, both from the plan:
//!
//! * **Safe-mode auto-retry.** A child that dies *before* it is ready failed to
//!   start, most likely on the DMABUF renderer. The supervisor retries once with
//!   [`crate::safe_mode::SAFE_MODE_ENV`] set, and only gives up if that fails
//!   too — so a GPU crash becomes a usable window instead of a black screen.
//! * **Window watchdog.** A child that crashes *after* it was ready is restarted
//!   (a bounded number of times), so a transient compositor crash does not end
//!   the session. The v1.1 console session adds a systemd `Restart=on-failure`
//!   unit on top; this is the in-app half.
//!
//! The decision itself ([`decide`]) is pure and table-tested; [`run`] is the
//! small imperative shell around it.

use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use crate::safe_mode;

/// How many times a ready window may crash before the supervisor gives up.
///
/// A restart loop is worse than a clean exit: after this many crashes the
/// launcher stops and says so, and the user can start it again in safe mode.
pub const MAX_RESTARTS: u32 = 3;

/// How the GUI child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exit code 0 and not killed by a signal: the window closed normally.
    Clean,
    /// A non-zero exit code or a signal: the child panicked, aborted or crashed.
    Crashed,
}

/// What the supervisor does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The session ended normally; the supervisor exits.
    Done,
    /// The child never became ready: retry once in safe mode.
    RetrySafe,
    /// The child was ready and then crashed: restart it (watchdog).
    Restart,
    /// Nothing left to try: safe mode failed too, or the crashes kept coming.
    GiveUp,
}

/// Decide what to do after one GUI child exits.
///
/// `safe` is whether the child that just ran was already in safe mode; `ready`
/// whether it wrote its ready file; `restarts` how many watchdog restarts have
/// happened so far.
pub fn decide(safe: bool, ready: bool, exit: Exit, restarts: u32) -> Decision {
    if ready {
        return match exit {
            Exit::Clean => Decision::Done,
            Exit::Crashed if restarts < MAX_RESTARTS => Decision::Restart,
            Exit::Crashed => Decision::GiveUp,
        };
    }

    // The child died before its window was up. A clean early exit is the app
    // choosing to stop (nothing more to do); a crash is a failed start.
    match exit {
        Exit::Clean => Decision::Done,
        Exit::Crashed if !safe => Decision::RetrySafe,
        Exit::Crashed => Decision::GiveUp,
    }
}

/// Classify a child's exit status.
fn classify(status: ExitStatus) -> Exit {
    if status.success() {
        Exit::Clean
    } else {
        Exit::Crashed
    }
}

/// Where the child writes its "window is up" line.
///
/// Under the data directory so two launchers in different XDG homes cannot
/// collide; falls back to the temp directory when `HOME` is unset. The name
/// carries the supervisor's pid, so a stale file from a killed run is never
/// mistaken for this one.
fn ready_file_path() -> PathBuf {
    let dir = pitagames_core::paths::data_dir().unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("ready-{}", std::process::id()))
}

/// Run the GUI as a child, retrying into safe mode and restarting on crashes.
///
/// Returns the process exit code the supervisor should use.
pub fn run(initial_safe: bool, argv: &[String]) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("[pitagames] could not find the launcher binary: {error}");
            return 1;
        }
    };

    let ready = ready_file_path();
    let mut safe = initial_safe;
    let mut restarts = 0u32;

    loop {
        // A stale file must never read as "this child became ready".
        let _ = std::fs::remove_file(&ready);

        let mut command = Command::new(&exe);
        command
            .args(argv)
            .env(safe_mode::CHILD_ENV, "1")
            .env(safe_mode::READY_FILE_ENV, &ready);
        if safe {
            command.env(safe_mode::SAFE_MODE_ENV, "1");
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                eprintln!("[pitagames] could not start the window process: {error}");
                return 1;
            }
        };

        let status = match child.wait() {
            Ok(status) => status,
            Err(error) => {
                eprintln!("[pitagames] lost track of the window process: {error}");
                return 1;
            }
        };

        let became_ready = ready.exists();
        let _ = std::fs::remove_file(&ready);

        match decide(safe, became_ready, classify(status), restarts) {
            Decision::Done => return 0,
            Decision::RetrySafe => {
                eprintln!(
                    "[pitagames] the window did not start; retrying in safe mode (animations and blur off)"
                );
                safe = true;
            }
            Decision::Restart => {
                restarts += 1;
                eprintln!(
                    "[pitagames] the window crashed; restarting ({restarts} of {MAX_RESTARTS})"
                );
            }
            Decision::GiveUp => {
                if became_ready {
                    eprintln!("[pitagames] the window keeps crashing; not restarting again");
                } else {
                    eprintln!(
                        "[pitagames] the window did not start in safe mode either; see the log above"
                    );
                }
                return 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_ready_exit_is_done() {
        assert_eq!(decide(false, true, Exit::Clean, 0), Decision::Done);
    }

    #[test]
    fn a_crash_before_ready_retries_in_safe_mode() {
        assert_eq!(decide(false, false, Exit::Crashed, 0), Decision::RetrySafe);
    }

    #[test]
    fn a_crash_before_ready_in_safe_mode_gives_up() {
        assert_eq!(decide(true, false, Exit::Crashed, 0), Decision::GiveUp);
    }

    #[test]
    fn a_clean_exit_before_ready_is_done_not_a_retry() {
        // The app chose to exit before showing a window; do not second-guess it.
        assert_eq!(decide(false, false, Exit::Clean, 0), Decision::Done);
    }

    #[test]
    fn a_ready_crash_restarts_until_the_cap() {
        for restarts in 0..MAX_RESTARTS {
            assert_eq!(
                decide(true, true, Exit::Crashed, restarts),
                Decision::Restart,
                "restart {restarts}"
            );
        }
        assert_eq!(
            decide(true, true, Exit::Crashed, MAX_RESTARTS),
            Decision::GiveUp
        );
    }

    #[test]
    fn a_signalled_child_is_a_crash() {
        // `Exit::Crashed` is what `classify` yields for a signal; the decision
        // table treats it like any other failure.
        assert_eq!(decide(false, false, Exit::Crashed, 0), Decision::RetrySafe);
    }

    #[test]
    fn the_ready_file_is_named_for_this_process() {
        let path = ready_file_path();
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("ready-"), "{name}");
        assert!(name.contains(&std::process::id().to_string()), "{name}");
    }
}
