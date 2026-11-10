//! Anti-cheat summary for the game detail page.
//!
//! The hard rules live in `platform::anticheat`; this module only turns its
//! detection and the compat DB's curated value into the short label, marker
//! list and warning the detail page shows. It is framework-free so it can be
//! unit-tested without a webview, like `grid.rs` and `runtime.rs`.

use std::path::Path;

use pitagames_core::AntiCheat;
use pitagames_platform::anticheat::{self, AnticheatSource};

/// What the detail page renders for one game's anti-cheat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnticheatView {
    /// Short value for the info card, e.g. `Easy Anti-Cheat (detected)`.
    pub label: String,
    /// Marker files found on disk, for the warning box.
    pub evidence: Vec<String>,
    /// The injection warning, when a concrete anti-cheat was found or curated.
    pub warning: Option<String>,
}

impl AnticheatView {
    /// The placeholder shown until the scan resolves.
    pub fn pending() -> Self {
        Self {
            label: "checking…".to_string(),
            evidence: Vec::new(),
            warning: None,
        }
    }
}

/// Merge `curated` with a marker scan of `install_dir` into the page's view.
///
/// The warning reflects the injection gate for a profile that asks for
/// injection: a game known to use EAC or BattlEye can only be injected after
/// the user confirms the risk, which the graphics screen enforces.
#[must_use]
pub fn summarize(install_dir: Option<&Path>, curated: AntiCheat) -> AnticheatView {
    let scan = install_dir.map(anticheat::detect).unwrap_or_default();
    let effective = anticheat::effective(curated, &scan);

    let label = match effective.kind {
        // "Not detected (not detected)" would be noise.
        AntiCheat::Unknown => AntiCheat::Unknown.label().to_string(),
        kind => format!("{} ({})", kind.label(), source_label(effective.source)),
    };

    let mut evidence: Vec<String> = scan
        .findings
        .iter()
        .map(|finding| format!("{}  ({})", finding.path.display(), finding.kind.as_str()))
        .collect();
    if scan.truncated {
        evidence.push("scan stopped early (large game directory)".to_string());
    }

    AnticheatView {
        label,
        evidence,
        warning: anticheat::gate(effective.kind, true, false).message(),
    }
}

fn source_label(source: AnticheatSource) -> &'static str {
    match source {
        AnticheatSource::Detected => "detected",
        AnticheatSource::Curated => "compat DB",
        AnticheatSource::Undetermined => "not detected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn a_curated_none_shows_no_warning() {
        let view = summarize(None, AntiCheat::None);
        assert_eq!(view.label, "None (compat DB)");
        assert!(view.warning.is_none());
        assert!(view.evidence.is_empty());
    }

    #[test]
    fn an_unknown_anti_cheat_is_reported_plainly() {
        let view = summarize(None, AntiCheat::Unknown);
        assert_eq!(view.label, "Not detected");
        assert!(view.warning.is_none());
    }

    #[test]
    fn a_detected_marker_wins_and_warns() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("EasyAntiCheat/EasyAntiCheat.exe"));

        // The compat DB says none, but the marker is proof.
        let view = summarize(Some(dir.path()), AntiCheat::None);
        assert_eq!(view.label, "Easy Anti-Cheat (detected)");
        assert_eq!(view.evidence.len(), 1);
        assert!(view.evidence[0].contains("EasyAntiCheat"));
        let warning = view.warning.expect("a protected game warns");
        assert!(warning.contains("Easy Anti-Cheat"), "{warning}");
    }

    #[test]
    fn a_curated_protected_game_warns_even_when_not_installed() {
        let view = summarize(None, AntiCheat::BattlEye);
        assert_eq!(view.label, "BattlEye (compat DB)");
        assert!(view.warning.unwrap().contains("BattlEye"));
    }
}
