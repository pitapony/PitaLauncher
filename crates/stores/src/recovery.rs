//! Reaping the install CLIs a crashed launcher left behind.
//!
//! A launcher crash never kills a running **game** — [`crate::launch`] starts a
//! game in its own session (`setsid`), so the game survives the launcher. A
//! store CLI mid-install is different: it is a child of the launcher, so when
//! the launcher dies the CLI is orphaned and keeps holding the store's
//! `~/.config` session lock. The queue persists the running job's process-group
//! id (see [`crate::queue::Interrupted`]), and this module decides what to do
//! with it at the next boot.
//!
//! **Killing is identity-checked.** A pid is not a durable handle: after a
//! crash, a reboot, or simply enough time, the id can be recycled by an
//! unrelated process. So a recorded pid is signalled only when its live command
//! line still names the store's own CLI (`legendary`, `nile`, `gogdl`). A pid
//! that is gone, or that now belongs to something else, is left exactly where
//! it is — a launcher must never kill a stranger.
//!
//! The killer itself is injected: `stores` has no dependency on `platform`, so
//! the caller supplies the escalation (the GUI passes
//! `platform::launch::terminate`, the same `SIGTERM`→`SIGKILL` a Stop button
//! uses). This keeps the decision logic pure and testable without spawning
//! processes.

use std::path::Path;

use pitagames_core::Store;

use crate::installer::program_for;
use crate::queue::Interrupted;

/// What became of one recorded pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanOutcome {
    /// The pid still named the store's CLI; it was signalled.
    Signalled { pid: u32, program: String },
    /// No process with that pid exists: it already exited.
    Gone { pid: u32 },
    /// The pid exists but is another program: left untouched.
    Foreign { pid: u32, command: String },
}

/// A recorded pid for an install that was `Running` when the launcher died.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub store: Store,
    pub pid: u32,
}

impl Orphan {
    /// The orphans named by a reconciled queue: one per `Running` job that had
    /// recorded its pid.
    #[must_use]
    pub fn from_interrupted(interrupted: &[Interrupted]) -> Vec<Self> {
        interrupted
            .iter()
            .filter_map(|entry| {
                entry.pid.map(|pid| Self {
                    store: entry.id.store,
                    pid,
                })
            })
            .collect()
    }
}

/// Reap every orphan, in order, through an injected process table and killer.
///
/// The process table answers "what is this pid now?" and the killer does the
/// signalling; both are injected so a test can drive the whole decision without
/// a live process.
pub fn reap_all(
    orphans: &[Orphan],
    cmdline: impl Fn(u32) -> Option<Vec<String>>,
    mut kill: impl FnMut(u32),
) -> Vec<OrphanOutcome> {
    orphans
        .iter()
        .map(|orphan| reap(orphan, &cmdline, &mut kill))
        .collect()
}

/// Reap one orphan: signal the pid only when it is still the store's CLI.
pub fn reap(
    orphan: &Orphan,
    cmdline: impl Fn(u32) -> Option<Vec<String>>,
    kill: impl FnOnce(u32),
) -> OrphanOutcome {
    let expected = program_for(orphan.store);
    let pid = orphan.pid;
    let Some(argv) = cmdline(pid) else {
        return OrphanOutcome::Gone { pid };
    };
    // A store with no installer (`program_for` is empty) is never reaped: the
    // caller only ever reconciles install jobs, but a defensive miss must not
    // kill an arbitrary pid.
    if expected.is_empty() {
        return OrphanOutcome::Foreign {
            pid,
            command: argv.first().cloned().unwrap_or_default(),
        };
    }
    match matching_program(&argv, expected) {
        Some(program) => {
            kill(pid);
            OrphanOutcome::Signalled { pid, program }
        }
        None => OrphanOutcome::Foreign {
            pid,
            command: argv.first().cloned().unwrap_or_default(),
        },
    }
}

/// The argument that names `expected`, if any: the CLI's own path.
///
/// `legendary` and `nile` are Python scripts, so the argv is
/// `["python3", "/usr/bin/legendary", …]` and the match is on the script's file
/// name rather than on argv[0].
fn matching_program(argv: &[String], expected: &str) -> Option<String> {
    argv.iter().find_map(|arg| {
        let name = Path::new(arg).file_name()?.to_str()?;
        (name == expected).then(|| name.to_string())
    })
}

/// The command line of `pid`, read from `/proc` on Linux.
///
/// `None` when no such process exists (or on a host without `/proc`, where the
/// answer is unavailable); the caller then leaves the pid alone rather than
/// guessing. An empty `/proc` read means a zombie, which is also "gone".
#[must_use]
pub fn system_cmdline(pid: u32) -> Option<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        if raw.is_empty() {
            return None;
        }
        Some(
            raw.split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect(),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orphan(store: Store, pid: u32) -> Orphan {
        Orphan { store, pid }
    }

    #[test]
    fn a_pid_that_still_names_the_store_cli_is_signalled() {
        let mut killed = Vec::new();
        let outcome = reap(
            &orphan(Store::Epic, 4242),
            |_| Some(vec!["python3".into(), "/usr/bin/legendary".into()]),
            |pid| killed.push(pid),
        );
        assert_eq!(
            outcome,
            OrphanOutcome::Signalled {
                pid: 4242,
                program: "legendary".into(),
            }
        );
        assert_eq!(killed, vec![4242]);
    }

    #[test]
    fn a_recycled_pid_belonging_to_another_program_is_left_alone() {
        let mut killed = Vec::new();
        let outcome = reap(
            &orphan(Store::Gog, 7),
            |_| Some(vec!["/usr/bin/firefox".into()]),
            |pid| killed.push(pid),
        );
        assert_eq!(
            outcome,
            OrphanOutcome::Foreign {
                pid: 7,
                command: "/usr/bin/firefox".into(),
            }
        );
        assert!(killed.is_empty(), "a stranger's pid is never signalled");
    }

    #[test]
    fn a_pid_that_is_gone_is_not_killed() {
        let mut killed = Vec::new();
        let outcome = reap(&orphan(Store::Amazon, 99), |_| None, |pid| killed.push(pid));
        assert_eq!(outcome, OrphanOutcome::Gone { pid: 99 });
        assert!(killed.is_empty());
    }

    #[test]
    fn a_store_with_no_cli_program_is_never_signalled() {
        let mut killed = Vec::new();
        let outcome = reap(
            &orphan(Store::Local, 5),
            |_| Some(vec!["shortcut".into()]),
            |pid| killed.push(pid),
        );
        assert_eq!(
            outcome,
            OrphanOutcome::Foreign {
                pid: 5,
                command: "shortcut".into(),
            }
        );
        assert!(killed.is_empty());
    }

    #[test]
    fn only_pids_recorded_by_running_jobs_become_orphans() {
        let interrupted = vec![
            Interrupted {
                id: pitagames_core::GameId::new(Store::Epic, "Gizmo"),
                pid: Some(10),
            },
            Interrupted {
                id: pitagames_core::GameId::new(Store::Gog, "1207658930"),
                pid: None,
            },
        ];
        assert_eq!(
            Orphan::from_interrupted(&interrupted),
            vec![Orphan {
                store: Store::Epic,
                pid: 10,
            }]
        );
    }

    #[test]
    fn reap_all_judges_each_orphan_and_kills_only_the_matching_one() {
        let orphans = vec![orphan(Store::Epic, 1), orphan(Store::Gog, 2)];
        let mut killed = Vec::new();
        let outcomes = reap_all(
            &orphans,
            |pid| match pid {
                1 => Some(vec!["/usr/bin/legendary".into()]),
                2 => Some(vec!["/usr/bin/bash".into()]),
                _ => None,
            },
            |pid| killed.push(pid),
        );
        assert_eq!(killed, vec![1]);
        assert!(matches!(
            outcomes[0],
            OrphanOutcome::Signalled { pid: 1, .. }
        ));
        assert!(matches!(outcomes[1], OrphanOutcome::Foreign { pid: 2, .. }));
    }
}
