//! The launcher's side of the performance harness.
//!
//! The budgets, the measurement format and the pure recorder live in
//! [`pitagames_core::perf`]; this module is the thin seam that turns the render
//! path into a measurement file. `main` calls [`install`] at process start, the
//! library grid calls [`mark_first_paint`] the first time it paints a loaded
//! library, and the grid's scroll path calls [`mark_frame`] on every step.
//!
//! Everything is gated on `PITAGAMES_PERF_FILE`: without it [`install`] does
//! nothing, so a normal run pays a single atomic load per call and never writes
//! a file. The harness (`scripts/perf-smoke.sh`) sets the variable, seeds a
//! 500-game library and drives a scroll; resident memory is measured by the
//! harness, because it is the whole process group (the launcher, the webview
//! and WebKit's helpers), not something the launcher can see.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use pitagames_core::perf::PerfRecorder;

/// Where the harness asks the launcher to write its measurement.
pub const PERF_FILE_ENV: &str = "PITAGAMES_PERF_FILE";

/// The one recorder, installed at process start and read from the render path.
///
/// A `Mutex` rather than a thread-local because the scroll path can run on the
/// async executor while a later paint runs on the event loop; the lock is only
/// ever taken while the harness is driving the app.
static RECORDER: OnceLock<Mutex<PerfRecorder>> = OnceLock::new();

/// The measurement file, when the harness asked for one.
fn perf_path() -> Option<PathBuf> {
    std::env::var_os(PERF_FILE_ENV).map(PathBuf::from)
}

/// Start recording, anchored at the process start.
///
/// Called once from `main` before the window is built. Without
/// `PITAGAMES_PERF_FILE` this does nothing and every other call no-ops.
pub fn install() {
    if perf_path().is_none() {
        return;
    }
    let _ = RECORDER.set(Mutex::new(PerfRecorder::new(Instant::now())));
}

/// Record the first painted library and write the measurement.
///
/// The grid calls this every render; only the first one records, so a scroll
/// cannot move the cold start.
pub fn mark_first_paint(games: usize) {
    let Some(recorder) = RECORDER.get() else {
        return;
    };
    let Ok(mut recorder) = recorder.lock() else {
        return;
    };
    recorder.set_games(games);
    if recorder.mark_first_paint(Instant::now()).is_some() {
        write(&recorder);
    }
}

/// Record one scroll step; the interval since the previous step is a frame.
pub fn mark_frame() {
    let Some(recorder) = RECORDER.get() else {
        return;
    };
    let Ok(mut recorder) = recorder.lock() else {
        return;
    };
    let now = Instant::now();
    recorder.mark_frame(now);
    // The file is throttled so a scroll burst does not write per frame; the
    // harness reads the last write after it stops scrolling.
    if recorder.should_flush(now) {
        write(&recorder);
    }
}

/// Write the measurement atomically, so a reader never sees half a file.
fn write(recorder: &PerfRecorder) {
    let Some(path) = perf_path() else {
        return;
    };
    let Ok(json) = recorder.measurement().to_json() else {
        return;
    };
    let temp = path.with_extension("json.tmp");
    if std::fs::write(&temp, json).is_ok() {
        let _ = std::fs::rename(&temp, &path);
    }
}
