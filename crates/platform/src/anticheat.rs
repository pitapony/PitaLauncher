//! Anti-cheat detection and the injection gate.
//!
//! PitaGames injects DLLs into a game's process for OptiScaler, HelixSR and
//! DLSSNR. An anti-cheat runtime that treats a modified game directory as a
//! cheat is the single most dangerous thing the launcher can trip over, so the
//! plan makes it a **hard gate**: a game known to use Easy Anti-Cheat or
//! BattlEye can only be injected after the user confirms the risk, and never
//! automatically.
//!
//! Two independent inputs feed the decision, and this module keeps them
//! separate:
//!
//! - **Curation** — the compat DB's `anticheat` field (`platform::compat`). It
//!   is right even when the game is not installed here.
//! - **Detection** — [`detect`] walks an installed game directory for the
//!   marker files and directories EAC and BattlEye ship ([`MARKERS`]).
//!
//! [`effective`] merges them: a concrete on-disk marker always wins, otherwise
//! curation is trusted, because a missing marker does not prove the absence of
//! anti-cheat (the runtime may live in the prefix or behind a launcher). The
//! merged value then feeds [`gate`], which the graphics UI consults before it
//! lets injection turn on.
//!
//! Like `platform::proton` and `platform::compat`, this is framework-free and
//! takes paths rather than reading XDG state, so it is testable against fixture
//! trees. It only *reads*: nothing here injects, and nothing here is enabled
//! automatically.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use pitagames_core::AntiCheat;

/// How deep the walk descends below the install directory.
///
/// Anti-cheat files sit at the game root (`BattlEye/`) or one or two levels
/// down (Unreal's `Binaries/Win64/EasyAntiCheat/`), so three levels is plenty
/// and keeps the walk away from deep asset trees.
pub const MAX_DEPTH: usize = 3;

/// The most directory entries a single walk visits before it gives up.
///
/// A game directory can hold hundreds of thousands of asset files. The cap
/// bounds the cost on the detail page; when it trips, the scan is marked
/// [`AnticheatScan::truncated`] so the result is not mistaken for complete.
pub const MAX_ENTRIES: usize = 50_000;

/// One file or directory name that names an anti-cheat runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    /// The runtime this marker belongs to.
    pub kind: AntiCheat,
    /// The name to match, compared case-insensitively.
    pub name: &'static str,
    /// `true` when the marker is a directory, `false` for a file.
    pub directory: bool,
}

/// The marker table, matched against every name under an install directory.
///
/// Windows games ship these with canonical casing, but the directory may have
/// been created by Proton/Wine, so matching lowercases the on-disk name.
pub const MARKERS: &[Marker] = &[
    // Easy Anti-Cheat, standalone and the Epic Online Services build.
    Marker {
        kind: AntiCheat::Eac,
        name: "easyanticheat",
        directory: true,
    },
    Marker {
        kind: AntiCheat::Eac,
        name: "easyanticheat_eos",
        directory: true,
    },
    Marker {
        kind: AntiCheat::Eac,
        name: "easyanticheat.exe",
        directory: false,
    },
    Marker {
        kind: AntiCheat::Eac,
        name: "easyanticheat_x64.dll",
        directory: false,
    },
    Marker {
        kind: AntiCheat::Eac,
        name: "easyanticheat_x86.dll",
        directory: false,
    },
    Marker {
        kind: AntiCheat::Eac,
        name: "easyanticheat_eos_x64.dll",
        directory: false,
    },
    Marker {
        kind: AntiCheat::Eac,
        name: "easyanticheat_eos_x86.dll",
        directory: false,
    },
    // EAC's "protected game" launcher (ELDEN RING and friends).
    Marker {
        kind: AntiCheat::Eac,
        name: "start_protected_game.exe",
        directory: false,
    },
    // BattlEye: the service directory, its service binaries and the client DLL.
    Marker {
        kind: AntiCheat::BattlEye,
        name: "battleye",
        directory: true,
    },
    Marker {
        kind: AntiCheat::BattlEye,
        name: "beservice.exe",
        directory: false,
    },
    Marker {
        kind: AntiCheat::BattlEye,
        name: "beservice_x64.exe",
        directory: false,
    },
    Marker {
        kind: AntiCheat::BattlEye,
        name: "beclient.dll",
        directory: false,
    },
    Marker {
        kind: AntiCheat::BattlEye,
        name: "beclient_x64.dll",
        directory: false,
    },
];

/// Bounds for one walk; [`detect`] uses the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    pub max_depth: usize,
    pub max_entries: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_depth: MAX_DEPTH,
            max_entries: MAX_ENTRIES,
        }
    }
}

/// One matched marker, with the path that carried it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnticheatFinding {
    pub kind: AntiCheat,
    /// The marker's canonical name, from [`MARKERS`].
    pub marker: &'static str,
    /// The absolute file or directory that matched.
    pub path: PathBuf,
}

/// The result of walking an install directory for anti-cheat markers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct AnticheatScan {
    /// The runtime of the first finding (ordered by path), or [`AntiCheat::Unknown`].
    pub kind: AntiCheat,
    /// Every marker that matched, ordered by path.
    pub findings: Vec<AnticheatFinding>,
    /// Whether the directory existed and was walked. `false` means "not
    /// installed here", which is different from "walked and found nothing".
    pub scanned: bool,
    /// Whether the walk stopped at [`ScanLimits::max_entries`] before finishing.
    pub truncated: bool,
}

impl AnticheatScan {
    /// Whether a concrete, injectable anti-cheat runtime was found.
    pub fn protected(&self) -> bool {
        self.kind.is_protected()
    }
}

/// Walk `install_dir` for the anti-cheat markers in [`MARKERS`].
///
/// A directory that does not exist (or is not a directory) yields
/// `scanned = false`; a read or permission error on a subdirectory is skipped
/// rather than failing the scan, so one unreadable folder never hides the rest.
#[must_use]
pub fn detect(install_dir: &Path) -> AnticheatScan {
    detect_limited(install_dir, ScanLimits::default())
}

/// [`detect`] with explicit walk bounds, for tests and callers that know better.
#[must_use]
pub fn detect_limited(install_dir: &Path, limits: ScanLimits) -> AnticheatScan {
    let mut scan = AnticheatScan::default();
    if !install_dir.is_dir() {
        return scan;
    }
    scan.scanned = true;

    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((install_dir.to_path_buf(), 0));
    let mut visited = 0usize;

    'walk: while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };

        // Sort before matching so the entry cap and the finding order are
        // deterministic regardless of the filesystem's directory order.
        let mut items: Vec<(String, PathBuf, bool)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let directory = path.is_dir();
            items.push((name, path, directory));
        }
        items.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

        let mut children: Vec<PathBuf> = Vec::new();
        for (name, path, directory) in items {
            visited += 1;
            if visited > limits.max_entries {
                scan.truncated = true;
                break 'walk;
            }

            if let Some(marker) = MARKERS
                .iter()
                .find(|marker| marker.name == name && marker.directory == directory)
            {
                scan.findings.push(AnticheatFinding {
                    kind: marker.kind,
                    marker: marker.name,
                    path: path.clone(),
                });
                // A matched directory holds only the anti-cheat runtime; no
                // need to descend into it for more evidence.
                continue;
            }

            if directory && depth < limits.max_depth {
                children.push(path);
            }
        }

        for child in children {
            queue.push_back((child, depth + 1));
        }
    }

    scan.findings.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.marker.cmp(right.marker))
    });
    if let Some(first) = scan.findings.first() {
        scan.kind = first.kind;
    }

    scan
}

/// Where an [`EffectiveAnticheat`] value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnticheatSource {
    /// A marker file was found in the installed game directory.
    Detected,
    /// The compat DB's curated `anticheat` field.
    Curated,
    /// Neither: nothing was detected and no entry curated a value.
    Undetermined,
}

/// The anti-cheat value the launcher acts on, with its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EffectiveAnticheat {
    pub kind: AntiCheat,
    pub source: AnticheatSource,
}

/// Merge curated knowledge with an on-disk scan.
///
/// A concrete `Eac`/`BattlEye` marker wins outright: a stale compat entry must
/// never downgrade real evidence. Otherwise the curated value is used, even
/// when the scan ran and found nothing, because anti-cheat frequently lives
/// outside the game folder (in the Proton prefix, behind a launcher, or in a
/// separate service install).
#[must_use]
pub fn effective(curated: AntiCheat, scan: &AnticheatScan) -> EffectiveAnticheat {
    if scan.kind.is_protected() {
        return EffectiveAnticheat {
            kind: scan.kind,
            source: AnticheatSource::Detected,
        };
    }

    if curated == AntiCheat::Unknown {
        EffectiveAnticheat {
            kind: curated,
            source: AnticheatSource::Undetermined,
        }
    } else {
        EffectiveAnticheat {
            kind: curated,
            source: AnticheatSource::Curated,
        }
    }
}

/// What the injection gate decided for one profile and game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateDecision {
    /// The profile asks for no injection; the gate did not engage.
    NotRequested,
    /// Injection may proceed.
    Allowed,
    /// Injection would run on an anti-cheat-protected game and needs an
    /// explicit acknowledgement first.
    ConfirmationRequired { anticheat: AntiCheat },
}

impl GateDecision {
    /// Whether injection may run without further input.
    pub const fn allows(self) -> bool {
        matches!(self, Self::NotRequested | Self::Allowed)
    }

    /// The anti-cheat that forced a confirmation, if any.
    pub const fn confirmation(self) -> Option<AntiCheat> {
        match self {
            Self::ConfirmationRequired { anticheat } => Some(anticheat),
            _ => None,
        }
    }

    /// A one-line reason to show the user, or `None` when injection may run.
    #[must_use]
    pub fn message(self) -> Option<String> {
        match self {
            Self::ConfirmationRequired { anticheat } => Some(format!(
                "{} can ban accounts for modified game files. Injecting here needs an \
                 explicit confirmation, and online play is never recommended.",
                anticheat.label()
            )),
            Self::NotRequested | Self::Allowed => None,
        }
    }
}

/// The anti-cheat injection gate.
///
/// Injection is refused (as [`GateDecision::ConfirmationRequired`]) only when it
/// is actually requested *and* a concrete `Eac`/`BattlEye` value is known and
/// the user has not acknowledged it. `Unknown` never hard-gates: it would block
/// every game whose anti-cheat we simply have not catalogued, and the plan
/// reserves the hard gate for the two runtimes it names.
#[must_use]
pub fn gate(anticheat: AntiCheat, requested: bool, acknowledged: bool) -> GateDecision {
    if !requested {
        return GateDecision::NotRequested;
    }
    if anticheat.is_protected() && !acknowledged {
        return GateDecision::ConfirmationRequired { anticheat };
    }
    GateDecision::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn a_clean_directory_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Game.exe"));
        touch(&dir.path().join("data/pak0.pak"));

        let scan = detect(dir.path());
        assert!(scan.scanned);
        assert!(!scan.truncated);
        assert_eq!(scan.kind, AntiCheat::Unknown);
        assert!(scan.findings.is_empty());
        assert!(!scan.protected());
    }

    #[test]
    fn a_missing_directory_is_not_scanned() {
        let scan = detect(Path::new("/nonexistent/pitagames/game"));
        assert!(!scan.scanned);
        assert_eq!(scan.kind, AntiCheat::Unknown);
        assert!(scan.findings.is_empty());
    }

    #[test]
    fn eac_and_battleye_directories_are_found_at_the_root() {
        let eac = tempfile::tempdir().unwrap();
        touch(&eac.path().join("EasyAntiCheat/EasyAntiCheat.exe"));
        let scan = detect(eac.path());
        assert_eq!(scan.kind, AntiCheat::Eac);
        assert!(scan.protected());
        assert_eq!(scan.findings.len(), 1);
        assert_eq!(scan.findings[0].marker, "easyanticheat");

        let be = tempfile::tempdir().unwrap();
        touch(&be.path().join("BattlEye/BEService.exe"));
        let scan = detect(be.path());
        assert_eq!(scan.kind, AntiCheat::BattlEye);
        assert_eq!(scan.findings[0].marker, "battleye");
    }

    #[test]
    fn markers_are_found_in_nested_unreal_binaries() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Binaries/Win64/EasyAntiCheat_X64.dll"));

        let scan = detect(dir.path());
        assert_eq!(scan.kind, AntiCheat::Eac);
        assert_eq!(scan.findings[0].marker, "easyanticheat_x64.dll");
        assert!(
            scan.findings[0]
                .path
                .ends_with("Binaries/Win64/EasyAntiCheat_X64.dll")
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("easyanticheat/easyanticheat.exe"));
        // The directory marker matches first (case-insensitively) and the walk
        // does not descend into it.
        let scan = detect(dir.path());
        assert_eq!(scan.kind, AntiCheat::Eac);
    }

    #[test]
    fn the_entry_cap_truncates_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        // The marker is created last (sorted after), so a small cap stops
        // before reaching it.
        touch(&dir.path().join("a.txt"));
        touch(&dir.path().join("b.txt"));
        fs::create_dir_all(dir.path().join("BattlEye")).unwrap();

        let limited = detect_limited(
            dir.path(),
            ScanLimits {
                max_depth: MAX_DEPTH,
                max_entries: 2,
            },
        );
        assert!(limited.truncated);
        assert!(limited.findings.is_empty());

        // With the default cap the walk completes and finds the directory.
        let full = detect(dir.path());
        assert!(!full.truncated);
        assert_eq!(full.kind, AntiCheat::BattlEye);
    }

    #[test]
    fn the_depth_limit_bounds_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Binaries/Win64/EasyAntiCheat_x64.dll"));

        let shallow = detect_limited(
            dir.path(),
            ScanLimits {
                max_depth: 1,
                max_entries: MAX_ENTRIES,
            },
        );
        assert!(shallow.findings.is_empty());

        let deep = detect_limited(
            dir.path(),
            ScanLimits {
                max_depth: 2,
                max_entries: MAX_ENTRIES,
            },
        );
        assert_eq!(deep.kind, AntiCheat::Eac);
    }

    #[test]
    fn detection_beats_a_stale_compat_entry() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("EasyAntiCheat/EasyAntiCheat.exe"));
        let scan = detect(dir.path());

        // The compat DB says none; the marker says otherwise.
        let merged = effective(AntiCheat::None, &scan);
        assert_eq!(merged.kind, AntiCheat::Eac);
        assert_eq!(merged.source, AnticheatSource::Detected);
    }

    #[test]
    fn curation_is_trusted_when_nothing_is_detected() {
        // Not installed here: the scan buys us nothing.
        let absent = detect(Path::new("/nonexistent/pitagames/game"));
        let merged = effective(AntiCheat::Eac, &absent);
        assert_eq!(merged.kind, AntiCheat::Eac);
        assert_eq!(merged.source, AnticheatSource::Curated);

        // Scanned and clean, but the DB knows the game ships EAC elsewhere.
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Game.exe"));
        let clean = detect(dir.path());
        let merged = effective(AntiCheat::BattlEye, &clean);
        assert_eq!(merged.kind, AntiCheat::BattlEye);
        assert_eq!(merged.source, AnticheatSource::Curated);
    }

    #[test]
    fn an_uncurated_uninstalled_game_is_undetermined() {
        let absent = detect(Path::new("/nonexistent/pitagames/game"));
        let merged = effective(AntiCheat::Unknown, &absent);
        assert_eq!(merged.kind, AntiCheat::Unknown);
        assert_eq!(merged.source, AnticheatSource::Undetermined);
    }

    #[test]
    fn the_gate_only_engages_when_injection_is_requested() {
        // No injection asked for: nothing to gate, whatever the anti-cheat.
        for anticheat in [AntiCheat::Eac, AntiCheat::BattlEye, AntiCheat::Unknown] {
            assert_eq!(
                gate(anticheat, false, false),
                GateDecision::NotRequested,
                "{anticheat:?}"
            );
        }
    }

    #[test]
    fn a_protected_game_needs_confirmation_before_injection() {
        for anticheat in [AntiCheat::Eac, AntiCheat::BattlEye] {
            let decision = gate(anticheat, true, false);
            assert_eq!(
                decision,
                GateDecision::ConfirmationRequired { anticheat },
                "{anticheat:?}"
            );
            assert!(!decision.allows());
            assert_eq!(decision.confirmation(), Some(anticheat));
            assert!(decision.message().unwrap().contains(anticheat.label()));

            // Once acknowledged, injection may run.
            let acknowledged = gate(anticheat, true, true);
            assert_eq!(acknowledged, GateDecision::Allowed);
            assert!(acknowledged.allows());
            assert!(acknowledged.message().is_none());
        }
    }

    #[test]
    fn unknown_or_absent_anti_cheat_does_not_hard_gate() {
        for anticheat in [AntiCheat::Unknown, AntiCheat::None] {
            let decision = gate(anticheat, true, false);
            assert_eq!(decision, GateDecision::Allowed, "{anticheat:?}");
            assert!(decision.allows());
        }
    }
}
