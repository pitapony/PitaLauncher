//! Verify that an injected mod actually engaged, from its own log.
//!
//! After a launch the launcher answers "did it engage?" by reading the mod's own
//! log — `dlssnr-amd.log` for DLSSNR, `OptiScaler.log` for the upscaler — and
//! looking for a **success marker** (plan §6 item 8, ADR-0024). Nobody has to
//! open the in-game menu.
//!
//! The markers are **data**, not code: [`MarkerData::shipped`] parses
//! `data/verify-markers.toml`, so an upstream wording change is a one-line edit
//! to that file. This module is framework-free and path-explicit, so the whole
//! verdict is tested against fixture logs with no game, GPU or webview — and the
//! upscaler and neural-rendering subjects share one engine and one
//! [`panel`](panel), so the graphics screen reports them side by side.
//!
//! ```text
//!  MarkerData { checks: [ MarkerSet { check, log, engaged, failed } ] }
//!          │
//!          ▼
//!  panel(data, [Check::Nr], log_dir) ──► [ Row { check, log, engagement } ]
//!          │                                  │
//!          └── verify_file(set, log_dir/log) ─┘
//! ```

use std::path::Path;

use pitagames_core::profile::{FrameGen, Profile};
use serde::{Deserialize, Serialize};

use crate::logs;

/// The shipped engagement markers, embedded at build time.
///
/// It is data, not code: a marker string that upstream changes is an edit to
/// this file (and a fixture), never a code change.
pub const SHIPPED: &str = include_str!("../data/verify-markers.toml");

/// How many lines [`verify_file`] keeps from the bounded tail it reads.
///
/// Effectively "all of them" for the 256 KiB window [`logs::tail`] reads; the
/// bound is the byte window, not the count.
const READ_LINES: usize = 200_000;

// ---------------------------------------------------------------------------
// The subjects
// ---------------------------------------------------------------------------

/// The injected subject a verification row is about.
///
/// The upscaler and neural rendering share one panel; a [`MarkerSet`] says which
/// log and which markers belong to each, so the two are the same mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// The injected upscaler's own log (`OptiScaler.log`).
    Upscaler,
    /// DLSSNR neural rendering (`dlssnr-amd.log`).
    Nr,
}

impl Check {
    /// Every subject, in the order the panel reads them.
    pub const ALL: [Check; 2] = [Check::Upscaler, Check::Nr];

    /// The stable identifier used in JSON and the CLI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upscaler => "upscaler",
            Self::Nr => "nr",
        }
    }

    /// The human label the panel shows beside the verdict.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Upscaler => "Upscaler",
            Self::Nr => "Neural rendering",
        }
    }
}

// ---------------------------------------------------------------------------
// The markers
// ---------------------------------------------------------------------------

/// One log marker: a stable `id` and the substring that proves it.
///
/// The `id` (not the text) is what a verdict reports, so a wording change keeps
/// the verdict's identity while the `contains` string is edited in the data
/// file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// A short, stable name for the marker (`model-load`).
    pub id: String,
    /// The case-sensitive substring to look for on a log line.
    pub contains: String,
}

/// The log and markers for one [`Check`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerSet {
    /// The subject this set verifies.
    pub check: Check,
    /// The log file name, read inside the given directory.
    pub log: String,
    /// Markers that prove the mod engaged.
    #[serde(default)]
    pub engaged: Vec<Marker>,
    /// Markers that explain why it did not.
    #[serde(default)]
    pub failed: Vec<Marker>,
    /// An optional note the panel shows (`"a data-only marker set"`).
    #[serde(default)]
    pub note: Option<String>,
}

impl MarkerSet {
    /// The first engaged marker found on any line, with the line it matched.
    fn first_engaged<'a>(&'a self, text: &'a str) -> Option<(&'a Marker, &'a str)> {
        // An engaged marker anywhere wins over a later failure: the network
        // loaded at least once, and that is the question being asked.
        for marker in &self.engaged {
            if let Some(line) = text.lines().find(|line| line.contains(&marker.contains)) {
                return Some((marker, line));
            }
        }
        None
    }

    /// The first failed marker found on any line, with its id.
    fn first_failed(&self, text: &str) -> Option<String> {
        self.failed
            .iter()
            .find(|marker| text.lines().any(|line| line.contains(&marker.contains)))
            .map(|marker| marker.id.clone())
    }
}

// ---------------------------------------------------------------------------
// The marker file
// ---------------------------------------------------------------------------

/// The parsed marker file: one [`MarkerSet`] per subject.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerData {
    /// The marker sets, in file order.
    #[serde(default)]
    pub checks: Vec<MarkerSet>,
}

impl MarkerData {
    /// Parse a marker document.
    pub fn parse(text: &str) -> Result<Self, VerifyError> {
        let data: Self = toml::from_str(text)?;
        data.validate()?;
        Ok(data)
    }

    /// The shipped marker data.
    ///
    /// The embedded file is compiled data, so a malformed one is a build bug; a
    /// test asserts it parses and covers both subjects.
    pub fn shipped() -> Self {
        Self::parse(SHIPPED).expect("the shipped verify-markers file parses")
    }

    /// The marker set for a subject, if the file ships one.
    pub fn get(&self, check: Check) -> Option<&MarkerSet> {
        self.checks.iter().find(|set| set.check == check)
    }

    /// Refuse a duplicated subject or a set that could never engage.
    fn validate(&self) -> Result<(), VerifyError> {
        for (index, set) in self.checks.iter().enumerate() {
            if self.checks[..index]
                .iter()
                .any(|other| other.check == set.check)
            {
                return Err(VerifyError::DuplicateCheck { check: set.check });
            }
            if set.engaged.is_empty() {
                return Err(VerifyError::NoEngagedMarker { check: set.check });
            }
            if set.log.trim().is_empty() {
                return Err(VerifyError::MissingLog { check: set.check });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

/// Whether an injected mod engaged, read from its log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Engagement {
    /// A success marker was found; the marker id and its line are kept.
    Engaged {
        /// The `id` of the marker that matched.
        marker: String,
        /// The log line it matched, trimmed.
        line: String,
    },
    /// No success marker was found; a failed marker explains it when present.
    NotEngaged {
        /// The `id` of the failed marker that matched, if any.
        failed: Option<String>,
    },
    /// The log was not found: nothing has been verified yet.
    NoLog,
}

impl Engagement {
    /// Whether the mod engaged.
    pub const fn is_engaged(&self) -> bool {
        matches!(self, Self::Engaged { .. })
    }

    /// The one-line label the panel shows.
    pub fn label(&self) -> String {
        match self {
            Self::Engaged { marker, .. } => format!("engaged ({marker})"),
            Self::NotEngaged { failed: Some(id) } => format!("not engaged ({id})"),
            Self::NotEngaged { failed: None } => "not engaged".to_string(),
            Self::NoLog => "no log yet".to_string(),
        }
    }
}

/// Read one log's text for markers: the engaged verdict first, then a failure.
///
/// Pure over its input, so a fixture string is a complete test of the policy.
pub fn verify_text(set: &MarkerSet, text: &str) -> Engagement {
    if let Some((marker, line)) = set.first_engaged(text) {
        return Engagement::Engaged {
            marker: marker.id.clone(),
            line: line.trim().to_string(),
        };
    }
    Engagement::NotEngaged {
        failed: set.first_failed(text),
    }
}

/// Read `dir/log` (bounded from the end) and report the engagement.
///
/// A missing or unreadable log is [`Engagement::NoLog`], never an error: a game
/// that has not run since an install simply has nothing to verify yet.
pub fn verify_file(set: &MarkerSet, dir: &Path) -> Engagement {
    let path = dir.join(&set.log);
    match logs::tail(&path, READ_LINES, None) {
        Ok(tail) => verify_text(set, &tail.lines.join("\n")),
        Err(_) => Engagement::NoLog,
    }
}

// ---------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------

/// One row of the verification panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// The subject this row reports.
    pub check: Check,
    /// The log file the verdict was read from.
    pub log: String,
    /// The verdict.
    pub engagement: Engagement,
}

/// The verification rows for the requested subjects, in the order given.
///
/// A subject the marker file does not cover is skipped rather than guessed, so
/// the panel never invents a verdict.
pub fn panel(data: &MarkerData, checks: &[Check], dir: &Path) -> Vec<Row> {
    checks
        .iter()
        .filter_map(|&check| {
            data.get(check).map(|set| Row {
                check,
                log: set.log.clone(),
                engagement: verify_file(set, dir),
            })
        })
        .collect()
}

/// The subjects worth reporting for a profile, in panel order.
///
/// Neural rendering and an injected upscaler are mutually exclusive (the single
/// injection slot, ADR-0023), so a profile reports exactly one — and a profile
/// that injects nothing reports none.
pub fn checks_for(profile: &Profile) -> Vec<Check> {
    if profile.neural_rendering.is_enabled() {
        vec![Check::Nr]
    } else if profile.frame_gen != FrameGen::Off || profile.upscaler.uses_injection() {
        vec![Check::Upscaler]
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why the marker file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("the verification marker file could not be read: {source}")]
    Toml {
        #[from]
        source: toml::de::Error,
    },

    #[error("the `{check:?}` verification subject is listed twice")]
    DuplicateCheck { check: Check },

    #[error("the `{check:?}` verification subject has no engaged marker")]
    NoEngagedMarker { check: Check },

    #[error("the `{check:?}` verification subject names no log file")]
    MissingLog { check: Check },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::profile::{
        ModelSource, NeuralRendering, NrBackend, NrModel, NrProcessing, NrRoute, PkgArch, Upscaler,
    };
    use pitagames_core::{GameId, Store};
    use std::fs;

    fn marker(id: &str, contains: &str) -> Marker {
        Marker {
            id: id.to_string(),
            contains: contains.to_string(),
        }
    }

    /// A worked marker set, independent of the shipped strings.
    fn set() -> MarkerSet {
        MarkerSet {
            check: Check::Nr,
            log: "dlssnr-amd.log".to_string(),
            engaged: vec![
                marker("model-load", "NN model loaded"),
                marker("nr-init", "NR initialized"),
            ],
            failed: vec![marker("model-missing", "NN model missing")],
            note: None,
        }
    }

    // -- verify_text --------------------------------------------------------

    #[test]
    fn an_engaged_marker_is_read_from_a_fixture_log() {
        let text = "[NR] starting\n[NR] NN model loaded (cc_small_fp8)\n";
        assert_eq!(
            verify_text(&set(), text),
            Engagement::Engaged {
                marker: "model-load".to_string(),
                line: "[NR] NN model loaded (cc_small_fp8)".to_string(),
            }
        );
    }

    #[test]
    fn a_log_without_a_success_marker_is_not_engaged() {
        let text = "[NR] starting\n[NR] waiting for a swapchain\n";
        assert_eq!(
            verify_text(&set(), text),
            Engagement::NotEngaged { failed: None }
        );
    }

    #[test]
    fn a_failed_marker_names_why_it_did_not_engage() {
        let text = "[NR] starting\n[NR] NN model missing, disabling\n";
        assert_eq!(
            verify_text(&set(), text),
            Engagement::NotEngaged {
                failed: Some("model-missing".to_string()),
            }
        );
    }

    #[test]
    fn an_engaged_marker_anywhere_wins_over_a_later_failure() {
        // A game that engaged once and later failed still engaged; the question
        // is "did the NR path run", and it did.
        let text = "[NR] NN model loaded\n...\n[NR] NN model missing\n";
        assert!(verify_text(&set(), text).is_engaged());
    }

    // -- verify_file --------------------------------------------------------

    #[test]
    fn verify_file_reads_the_named_log_from_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("dlssnr-amd.log"), "[NR] NR initialized\n").unwrap();
        let verdict = verify_file(&set(), dir.path());
        assert_eq!(
            verdict,
            Engagement::Engaged {
                marker: "nr-init".to_string(),
                line: "[NR] NR initialized".to_string(),
            }
        );
    }

    #[test]
    fn a_missing_log_is_no_log_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(verify_file(&set(), dir.path()), Engagement::NoLog);

        fs::write(dir.path().join("dlssnr-amd.log"), "").unwrap();
        assert_eq!(
            verify_file(&set(), dir.path()),
            Engagement::NotEngaged { failed: None },
            "an empty log is present but has no marker"
        );
    }

    // -- the marker file ----------------------------------------------------

    #[test]
    fn the_shipped_marker_file_covers_both_subjects() {
        let data = MarkerData::shipped();
        let nr = data.get(Check::Nr).expect("an NR marker set");
        assert_eq!(nr.log, "dlssnr-amd.log");
        assert!(!nr.engaged.is_empty());
        let upscaler = data.get(Check::Upscaler).expect("an upscaler marker set");
        assert_eq!(upscaler.log, "OptiScaler.log");
    }

    #[test]
    fn a_duplicated_subject_is_refused_at_load() {
        let text = r#"
            [[checks]]
            check = "nr"
            log = "a.log"
            engaged = [{ id = "x", contains = "x" }]

            [[checks]]
            check = "nr"
            log = "b.log"
            engaged = [{ id = "y", contains = "y" }]
        "#;
        assert!(matches!(
            MarkerData::parse(text),
            Err(VerifyError::DuplicateCheck { check: Check::Nr })
        ));
    }

    #[test]
    fn a_subject_with_no_engaged_marker_is_refused() {
        let text = r#"
            [[checks]]
            check = "nr"
            log = "a.log"
            failed = [{ id = "x", contains = "x" }]
        "#;
        assert!(matches!(
            MarkerData::parse(text),
            Err(VerifyError::NoEngagedMarker { check: Check::Nr })
        ));
    }

    // -- the panel ----------------------------------------------------------

    #[test]
    fn the_panel_reports_only_the_requested_subjects_it_has_data_for() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("dlssnr-amd.log"),
            "[NR] neural rendering initialized\n",
        )
        .unwrap();
        let data = MarkerData::shipped();

        let rows = panel(&data, &[Check::Nr], dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].check, Check::Nr);
        assert!(rows[0].engagement.is_engaged());

        // The upscaler log is absent, so its row is `NoLog`, not an error.
        let rows = panel(&data, &[Check::Upscaler, Check::Nr], dir.path());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].engagement, Engagement::NoLog);
        assert_eq!(rows[0].check, Check::Upscaler);
    }

    #[test]
    fn checks_for_picks_the_profile_single_injection_slot() {
        let game = GameId::new(Store::Steam, "1091500");
        let mut profile = Profile::new(game.clone(), "Default");
        assert!(checks_for(&profile).is_empty(), "nothing injected");

        profile.upscaler = Upscaler::DlssToFsr4;
        assert_eq!(checks_for(&profile), vec![Check::Upscaler]);

        profile.neural_rendering = NeuralRendering::DlssNrAmd {
            backend: NrBackend::Rdna4Upstream,
            version: "0.0.9".into(),
            route: NrRoute::Optiscaler,
            model: NrModel::CcSmallFp8,
            model_scale: 100,
            processing: NrProcessing::RenoDx,
            pkg_arch: PkgArch::X86_64,
            captured_env: Default::default(),
            model_source: ModelSource::CachedInPackage,
        };
        assert_eq!(checks_for(&profile), vec![Check::Nr]);
    }

    #[test]
    fn a_verdict_round_trips_through_json_for_the_cli() {
        let verdict = Engagement::Engaged {
            marker: "model-load".to_string(),
            line: "[NR] NN model loaded".to_string(),
        };
        let json = serde_json::to_string(&verdict).unwrap();
        assert!(json.contains(r#""status":"engaged""#), "{json}");
        let parsed: Engagement = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, verdict);
    }
}
