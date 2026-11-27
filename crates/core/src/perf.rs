//! Performance budgets and the measurement format.
//!
//! The plan makes three numbers acceptance criteria for the launcher: a cold
//! start to first paint under **2 s** with a 500-game library, a **60 fps**
//! grid scroll, and a resident-memory ceiling with the WebKit webview of about
//! **300 MB**. Those numbers are hardware claims, so the harness that measures
//! them runs on the two target machines; the container measures the same three
//! numbers as a proxy and asserts the two it can.
//!
//! This module is the framework-free half: the budgets, the measurement format
//! the harness writes, and the pure verdict. The renderer (`crates/app`) and
//! the CLI both consume it, so the number the container prints and the number
//! the hardware records cannot drift.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Cold start to first paint with a full library, in milliseconds.
pub const COLD_START_BUDGET_MS: f64 = 2000.0;

/// One 60 Hz frame, in milliseconds.
pub const FRAME_TIME_BUDGET_MS: f64 = 1000.0 / 60.0;

/// Resident memory ceiling with the webview, in bytes (300 MiB).
pub const MEMORY_CEILING_BYTES: u64 = 300 * 1024 * 1024;

/// The library size the cold-start budget is measured against.
pub const LIBRARY_GAMES: usize = 500;

/// One metric a budget can enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Metric {
    /// Process start to the library's first painted grid.
    ColdStart,
    /// The 95th-percentile scroll frame interval.
    FrameTime,
    /// Peak resident memory of the launcher's process group.
    Memory,
}

impl Metric {
    /// Every metric, in the order a report prints them.
    pub const ALL: [Metric; 3] = [Metric::ColdStart, Metric::FrameTime, Metric::Memory];

    /// The stable name used on the command line and in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Metric::ColdStart => "cold-start",
            Metric::FrameTime => "frame-time",
            Metric::Memory => "memory",
        }
    }

    /// Parse a metric from its [`Metric::as_str`] name.
    pub fn parse(value: &str) -> Option<Metric> {
        Metric::ALL.into_iter().find(|m| m.as_str() == value)
    }

    /// The unit the metric is measured in, for a report.
    pub fn unit(self) -> &'static str {
        match self {
            Metric::ColdStart | Metric::FrameTime => "ms",
            Metric::Memory => "bytes",
        }
    }
}

/// A percentile summary of the sampled scroll frame intervals.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameStats {
    /// How many frame intervals were sampled.
    pub frames: usize,
    /// Median frame interval, in milliseconds.
    pub p50_ms: f64,
    /// 95th-percentile frame interval, in milliseconds.
    pub p95_ms: f64,
    /// Slowest frame interval, in milliseconds.
    pub max_ms: f64,
}

impl FrameStats {
    /// Summarise raw frame intervals. Non-finite samples are dropped; an empty
    /// list has no summary. The percentile is the linear-interpolation method
    /// (the same one `numpy.percentile` defaults to), so the value is stable.
    pub fn from_samples(samples: &[f64]) -> Option<FrameStats> {
        let mut sorted: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
        if sorted.is_empty() {
            return None;
        }
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite values are ordered"));
        let max_ms = *sorted.last().expect("non-empty");
        Some(FrameStats {
            frames: sorted.len(),
            p50_ms: percentile(&sorted, 50.0),
            p95_ms: percentile(&sorted, 95.0),
            max_ms,
        })
    }
}

/// The linear-interpolation percentile of an ascending list.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (p / 100.0) * (sorted.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        return sorted[lower];
    }
    let fraction = rank - lower as f64;
    sorted[lower] + (sorted[upper] - sorted[lower]) * fraction
}

/// One run of the harness: the numbers the script collected.
///
/// `frame_ms` carries the raw per-frame samples rather than a summary so the
/// percentile method lives in one place ([`FrameStats`]); the harness does not
/// have to know what "60 fps" means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    /// How many games the library held. The cold-start budget is only
    /// meaningful at [`LIBRARY_GAMES`].
    pub games: usize,
    /// Process start to the first painted grid, in milliseconds.
    #[serde(default)]
    pub cold_start_ms: Option<f64>,
    /// Raw scroll frame intervals, in milliseconds.
    #[serde(default)]
    pub frame_ms: Vec<f64>,
    /// Peak resident memory of the launcher's process group, in bytes.
    #[serde(default)]
    pub memory_bytes: Option<u64>,
}

impl Measurement {
    /// Parse the harness JSON.
    pub fn from_json(input: &str) -> Result<Measurement, PerfError> {
        Ok(serde_json::from_str(input)?)
    }

    /// Render the harness JSON.
    pub fn to_json(&self) -> Result<String, PerfError> {
        Ok(serde_json::to_string(self)?)
    }

    /// The scroll frame summary, when any samples were collected.
    pub fn frame_stats(&self) -> Option<FrameStats> {
        FrameStats::from_samples(&self.frame_ms)
    }

    /// The measured value of a metric, or `None` when it was not collected.
    pub fn value(&self, metric: Metric) -> Option<f64> {
        match metric {
            Metric::ColdStart => self.cold_start_ms,
            Metric::FrameTime => self.frame_stats().map(|stats| stats.p95_ms),
            Metric::Memory => self.memory_bytes.map(|bytes| bytes as f64),
        }
    }
}

/// A metric outside its budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Violation {
    /// Which budget was broken.
    pub metric: Metric,
    /// What the run measured.
    pub measured: f64,
    /// The budget it exceeded.
    pub limit: f64,
}

/// The result of checking a measurement against a budget.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every required metric was measured and inside its budget.
    Pass,
    /// At least one required metric was measured and outside its budget.
    Fail(Vec<Violation>),
    /// A required metric was not measured, so the run cannot be judged.
    Incomplete(Vec<Metric>),
}

impl Verdict {
    /// Whether the run passed.
    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

/// The numbers a run is checked against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// Cold-start ceiling, in milliseconds.
    pub cold_start_ms: f64,
    /// Frame-time ceiling (the 95th percentile), in milliseconds.
    pub frame_time_ms: f64,
    /// Resident-memory ceiling, in bytes.
    pub memory_bytes: u64,
    /// The library size the run should have used.
    pub games: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            cold_start_ms: COLD_START_BUDGET_MS,
            frame_time_ms: FRAME_TIME_BUDGET_MS,
            memory_bytes: MEMORY_CEILING_BYTES,
            games: LIBRARY_GAMES,
        }
    }
}

impl Budget {
    /// The ceiling for one metric.
    pub fn limit(&self, metric: Metric) -> f64 {
        match metric {
            Metric::ColdStart => self.cold_start_ms,
            Metric::FrameTime => self.frame_time_ms,
            Metric::Memory => self.memory_bytes as f64,
        }
    }

    /// Check the required metrics.
    ///
    /// A required metric that was measured and is over budget is a [`Violation`];
    /// one that was not measured makes the run [`Verdict::Incomplete`]. Metrics
    /// not in `required` are not checked, so the container can enforce the two
    /// budgets it can and merely record the third.
    pub fn check(&self, measurement: &Measurement, required: &[Metric]) -> Verdict {
        let mut violations = Vec::new();
        let mut missing = Vec::new();
        for metric in required {
            match measurement.value(*metric) {
                None => missing.push(*metric),
                Some(measured) => {
                    let limit = self.limit(*metric);
                    if measured > limit {
                        violations.push(Violation {
                            metric: *metric,
                            measured,
                            limit,
                        });
                    }
                }
            }
        }
        if !violations.is_empty() {
            Verdict::Fail(violations)
        } else if !missing.is_empty() {
            Verdict::Incomplete(missing)
        } else {
            Verdict::Pass
        }
    }
}

/// Why a measurement could not be parsed or rendered.
#[derive(Debug, Error)]
pub enum PerfError {
    /// The harness JSON was malformed.
    #[error("invalid performance measurement: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten frames, one millisecond apart — a worked example with known
    /// percentiles (linear interpolation: p50 = 5.5, p95 = 9.55, max = 10).
    fn ten_frames() -> Vec<f64> {
        (1..=10).map(|n| n as f64).collect()
    }

    #[test]
    fn the_hardware_budget_matches_the_plan() {
        let budget = Budget::default();
        assert_eq!(budget.cold_start_ms, 2000.0);
        assert_eq!(budget.frame_time_ms, 1000.0 / 60.0);
        assert_eq!(budget.memory_bytes, 300 * 1024 * 1024);
        assert_eq!(budget.games, 500);
    }

    #[test]
    fn a_metric_round_trips_through_its_name() {
        for metric in Metric::ALL {
            assert_eq!(Metric::parse(metric.as_str()), Some(metric));
        }
        assert_eq!(Metric::parse("nonsense"), None);
        assert_eq!(Metric::ColdStart.as_str(), "cold-start");
        assert_eq!(Metric::Memory.unit(), "bytes");
    }

    #[test]
    fn frame_stats_are_computed_with_linear_interpolation() {
        let stats = FrameStats::from_samples(&ten_frames()).unwrap();
        assert_eq!(stats.frames, 10);
        assert_eq!(stats.p50_ms, 5.5);
        assert!((stats.p95_ms - 9.55).abs() < 1e-9, "{}", stats.p95_ms);
        assert_eq!(stats.max_ms, 10.0);
    }

    #[test]
    fn frame_stats_do_not_depend_on_sample_order() {
        let mut shuffled = ten_frames();
        shuffled.reverse();
        assert_eq!(
            FrameStats::from_samples(&shuffled),
            FrameStats::from_samples(&ten_frames())
        );
    }

    #[test]
    fn frame_stats_drop_non_finite_samples() {
        let stats = FrameStats::from_samples(&[1.0, f64::NAN, 3.0, f64::INFINITY]).unwrap();
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.max_ms, 3.0);
    }

    #[test]
    fn an_empty_sample_list_has_no_stats() {
        assert!(FrameStats::from_samples(&[]).is_none());
        assert!(FrameStats::from_samples(&[f64::NAN]).is_none());
    }

    #[test]
    fn a_measurement_round_trips_through_json() {
        let measurement = Measurement {
            games: 500,
            cold_start_ms: Some(1234.5),
            frame_ms: ten_frames(),
            memory_bytes: Some(123_456_789),
        };
        let json = measurement.to_json().unwrap();
        assert_eq!(Measurement::from_json(&json).unwrap(), measurement);
    }

    #[test]
    fn a_measurement_tolerates_a_missing_metric() {
        let parsed = Measurement::from_json(r#"{ "games": 500 }"#).unwrap();
        assert_eq!(parsed.games, 500);
        assert_eq!(parsed.cold_start_ms, None);
        assert!(parsed.frame_ms.is_empty());
        assert_eq!(parsed.memory_bytes, None);
        assert!(parsed.frame_stats().is_none());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(Measurement::from_json("not json").is_err());
        assert!(Measurement::from_json(r#"{ "games": "five" }"#).is_err());
    }

    #[test]
    fn a_within_budget_run_passes() {
        let measurement = Measurement {
            games: 500,
            cold_start_ms: Some(1500.0),
            frame_ms: vec![8.0, 9.0, 10.0, 11.0],
            memory_bytes: Some(250 * 1024 * 1024),
        };
        assert_eq!(
            Budget::default().check(&measurement, &Metric::ALL),
            Verdict::Pass
        );
    }

    #[test]
    fn each_over_budget_metric_is_a_violation() {
        let measurement = Measurement {
            games: 500,
            cold_start_ms: Some(2500.0),
            frame_ms: vec![20.0, 20.0, 20.0, 20.0],
            memory_bytes: Some(400 * 1024 * 1024),
        };
        let verdict = Budget::default().check(&measurement, &Metric::ALL);
        let Verdict::Fail(violations) = verdict else {
            panic!("expected a failure");
        };
        let metrics: Vec<Metric> = violations.iter().map(|v| v.metric).collect();
        assert!(metrics.contains(&Metric::ColdStart));
        assert!(metrics.contains(&Metric::FrameTime));
        assert!(metrics.contains(&Metric::Memory));
    }

    #[test]
    fn frame_time_is_judged_at_the_95th_percentile() {
        // A steady 60 fps with a few long frames: p50 is fine, p95 is not.
        let mut samples = vec![16.0; 90];
        samples.extend(std::iter::repeat_n(40.0, 10));
        let measurement = Measurement {
            games: 500,
            cold_start_ms: None,
            frame_ms: samples,
            memory_bytes: None,
        };
        assert!(matches!(
            Budget::default().check(&measurement, &[Metric::FrameTime]),
            Verdict::Fail(_)
        ));
    }

    #[test]
    fn a_required_metric_that_was_not_measured_is_incomplete() {
        let measurement = Measurement {
            games: 500,
            cold_start_ms: None,
            frame_ms: vec![8.0, 9.0],
            memory_bytes: Some(100),
        };
        assert_eq!(
            Budget::default().check(&measurement, &Metric::ALL),
            Verdict::Incomplete(vec![Metric::ColdStart])
        );
    }

    #[test]
    fn an_unrequired_metric_is_not_checked() {
        // Over budget on every metric, but only memory is required and fine.
        let measurement = Measurement {
            games: 500,
            cold_start_ms: Some(9000.0),
            frame_ms: vec![500.0],
            memory_bytes: Some(200 * 1024 * 1024),
        };
        assert_eq!(
            Budget::default().check(&measurement, &[Metric::Memory]),
            Verdict::Pass
        );
    }

    #[test]
    fn a_failure_beats_a_missing_metric() {
        let measurement = Measurement {
            games: 500,
            cold_start_ms: Some(9000.0),
            frame_ms: Vec::new(),
            memory_bytes: None,
        };
        // Cold start fails; frame time and memory are missing.
        assert!(matches!(
            Budget::default().check(&measurement, &Metric::ALL),
            Verdict::Fail(_)
        ));
    }

    #[test]
    fn the_default_verdict_is_not_a_pass_until_checked() {
        assert!(!Verdict::Incomplete(Vec::new()).is_pass());
        assert!(Verdict::Pass.is_pass());
    }
}
