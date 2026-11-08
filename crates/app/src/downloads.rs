//! Steam installs as the launcher sees them: a board to render plus the
//! threads that poll `appmanifest` files.
//!
//! Framework-free on purpose. The Dioxus layer owns a [`Downloads`] board and
//! forwards [`DownloadEvent`]s into it; everything here can be tested without a
//! window, and the tracker threads only need a callback, not a runtime.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use pitagames_stores::{DEFAULT_POLL_INTERVAL, InstallMonitor, InstallPhase, InstallSample};

/// Give up following an install after six hours.
///
/// Steam keeps a queued install for as long as the user leaves it, so this is a
/// safety net for the tracker thread, not a user-facing deadline.
pub const TRACK_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// Where one tracked install stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    /// The request was handed to Steam, but the manifest is not written yet.
    Waiting,
    /// The manifest exists and Steam is working on the files.
    Running(InstallPhase),
    /// Steam reports the app as installed and idle.
    Installed,
    /// We stopped following: Steam never finished within [`TRACK_TIMEOUT`].
    TimedOut,
}

impl DownloadState {
    /// No further events will arrive for this entry.
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Installed | Self::TimedOut)
    }

    /// Short status word for the downloads row.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting for Steam",
            Self::Running(phase) => phase.as_str(),
            Self::Installed => "installed",
            Self::TimedOut => "timed out",
        }
    }
}

/// One row of the downloads view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub app_id: String,
    pub title: String,
    pub state: DownloadState,
    /// Bytes fetched so far, when Steam records counters.
    pub downloaded: Option<u64>,
    /// Total bytes for this install, when Steam records counters.
    pub total: Option<u64>,
}

impl Download {
    fn new(app_id: &str, title: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            title: title.to_string(),
            state: DownloadState::Waiting,
            downloaded: None,
            total: None,
        }
    }

    /// Best-effort percentage; `None` when Steam has not written a total.
    pub fn percent(&self) -> Option<u8> {
        match (self.downloaded, self.total) {
            (Some(downloaded), Some(total)) => crate::format::percent(downloaded, total),
            _ => None,
        }
    }

    fn apply(&mut self, sample: &InstallSample) {
        match sample {
            InstallSample::WaitingForManifest => {
                self.state = DownloadState::Waiting;
            }
            InstallSample::InProgress(progress) => {
                self.state = DownloadState::Running(progress.phase);
                self.downloaded = progress.progress.map(|bytes| bytes.downloaded);
                self.total = progress.progress.map(|bytes| bytes.total);
            }
            InstallSample::Installed { .. } => {
                self.state = DownloadState::Installed;
                if let Some(total) = self.total {
                    self.downloaded = Some(total);
                }
            }
            InstallSample::TimedOut => {
                self.state = DownloadState::TimedOut;
            }
        }
    }
}

/// One poll result, addressed to the row it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadEvent {
    pub app_id: String,
    pub sample: InstallSample,
}

/// The tracked installs, in the order they were added.
#[derive(Debug, Default, Clone)]
pub struct Downloads {
    rows: Vec<Download>,
}

impl Downloads {
    /// Track an install, keeping any existing row for the same appid.
    ///
    /// Returns `true` when a tracker should be started: a fresh row, or a row
    /// that previously finished (a retry). Returns `false` while a tracker for
    /// this appid is already running, so double clicks do not double-track.
    pub fn start(&mut self, app_id: &str, title: &str) -> bool {
        if let Some(row) = self.rows.iter_mut().find(|row| row.app_id == app_id) {
            if !row.state.is_finished() {
                return false;
            }
            row.state = DownloadState::Waiting;
            row.downloaded = None;
            row.total = None;
            row.title = title.to_string();
            return true;
        }

        self.rows.push(Download::new(app_id, title));
        true
    }

    /// Fold one poll result in. Unknown appids are ignored: the row may have
    /// been dismissed while the tracker thread was still winding down.
    pub fn apply(&mut self, event: &DownloadEvent) {
        if let Some(row) = self.rows.iter_mut().find(|row| row.app_id == event.app_id) {
            row.apply(&event.sample);
        }
    }

    pub fn get(&self, app_id: &str) -> Option<&Download> {
        self.rows.iter().find(|row| row.app_id == app_id)
    }

    /// All rows, in the order they were added.
    pub fn rows(&self) -> &[Download] {
        &self.rows
    }

    /// Rows Steam is still working on, oldest first.
    pub fn active(&self) -> impl Iterator<Item = &Download> {
        self.rows.iter().filter(|row| !row.state.is_finished())
    }

    /// Rows that will not change again, oldest first.
    pub fn finished(&self) -> impl Iterator<Item = &Download> {
        self.rows.iter().filter(|row| row.state.is_finished())
    }

    pub fn active_count(&self) -> usize {
        self.active().count()
    }

    /// Drop a finished row. Active rows are kept: dismissing them would lie
    /// about a download Steam is still running.
    pub fn dismiss(&mut self, app_id: &str) -> bool {
        let Some(index) = self.rows.iter().position(|row| row.app_id == app_id) else {
            return false;
        };
        if !self.rows[index].state.is_finished() {
            return false;
        }
        self.rows.remove(index);
        true
    }

    pub fn dismiss_finished(&mut self) -> usize {
        let before = self.rows.len();
        self.rows.retain(|row| !row.state.is_finished());
        before - self.rows.len()
    }
}

/// Follow one appid on a background thread, reporting every poll to `emit`.
///
/// `emit` returns `false` when the UI is gone; the thread then stops instead of
/// polling for hours. `libraries` are the library folders of the last scan, so
/// no VDF is re-read here.
pub fn track<F>(app_id: &str, libraries: &[PathBuf], emit: F)
where
    F: FnMut(DownloadEvent) -> bool + Send + 'static,
{
    let Ok(numeric_id) = app_id.parse::<u32>() else {
        // Non-numeric ids belong to non-Steam stores; nothing to watch.
        return;
    };

    let libraries = libraries.to_vec();
    let app_id = app_id.to_string();
    let thread_name = format!("pitagames-download-{app_id}");

    // A tracker that cannot be named is still worth running; ignore the error.
    let _ = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let monitor = InstallMonitor::new(numeric_id, libraries, TRACK_TIMEOUT);
            let started = Instant::now();
            let mut emit = emit;

            loop {
                let sample = monitor.sample(started.elapsed());
                let final_sample = sample.is_final();
                if !emit(DownloadEvent {
                    app_id: app_id.clone(),
                    sample,
                }) || final_sample
                {
                    return;
                }
                std::thread::sleep(DEFAULT_POLL_INTERVAL);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_stores::InstallProgress;

    fn event(app_id: &str, sample: InstallSample) -> DownloadEvent {
        DownloadEvent {
            app_id: app_id.to_string(),
            sample,
        }
    }

    fn downloading(downloaded: u64, total: u64) -> InstallSample {
        InstallSample::InProgress(InstallProgress {
            phase: InstallPhase::Downloading,
            progress: Some(pitagames_stores::DownloadProgress { downloaded, total }),
        })
    }

    #[test]
    fn a_second_start_while_running_is_ignored() {
        let mut board = Downloads::default();
        assert!(board.start("620", "Portal 2"));
        assert!(!board.start("620", "Portal 2"));

        board.apply(&event(
            "620",
            InstallSample::Installed {
                manifest: PathBuf::from("/steam/appmanifest_620.acf"),
            },
        ));
        assert!(board.start("620", "Portal 2"), "a finished row can retry");
        assert_eq!(
            board.get("620").unwrap().state,
            DownloadState::Waiting,
            "retry clears the previous result"
        );
    }

    #[test]
    fn events_update_state_and_progress() {
        let mut board = Downloads::default();
        board.start("620", "Portal 2");

        board.apply(&event("620", InstallSample::WaitingForManifest));
        assert_eq!(board.get("620").unwrap().state, DownloadState::Waiting);

        board.apply(&event("620", downloading(1_000, 4_000)));
        let row = board.get("620").unwrap();
        assert_eq!(row.state, DownloadState::Running(InstallPhase::Downloading));
        assert_eq!(row.percent(), Some(25));
        assert_eq!(board.active_count(), 1);

        board.apply(&event("620", InstallSample::TimedOut));
        let row = board.get("620").unwrap();
        assert_eq!(row.state, DownloadState::TimedOut);
        assert!(row.state.is_finished());
        assert_eq!(board.active_count(), 0);
    }

    #[test]
    fn unknown_appids_and_dismissals_are_ignored() {
        let mut board = Downloads::default();
        board.start("620", "Portal 2");
        board.apply(&event("570", InstallSample::TimedOut));
        assert_eq!(board.rows().len(), 1);

        assert!(!board.dismiss("620"), "an active row cannot be dismissed");
        assert!(!board.dismiss("999"));
        board.apply(&event("620", InstallSample::TimedOut));
        assert!(board.dismiss("620"));
        assert!(board.rows().is_empty());
    }

    #[test]
    fn dismiss_finished_keeps_active_rows() {
        let mut board = Downloads::default();
        board.start("620", "Portal 2");
        board.start("570", "Dota 2");
        board.apply(&event("620", InstallSample::TimedOut));

        assert_eq!(board.dismiss_finished(), 1);
        assert_eq!(board.rows().len(), 1);
        assert_eq!(board.get("570").unwrap().state, DownloadState::Waiting);
    }

    #[test]
    fn tracker_reports_a_real_manifest_then_stops() {
        use std::fs;
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let steamapps = dir.path().join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        fs::write(
            steamapps.join("appmanifest_620.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"620\"\n\t\"name\"\t\t\"Portal 2\"\n\t\"StateFlags\"\t\"4\"\n}\n",
        )
        .unwrap();

        let (tx, rx) = mpsc::channel();
        track("620", &[dir.path().to_path_buf()], move |event| {
            let done = event.sample.is_final();
            tx.send((event.sample, done)).unwrap();
            true
        });

        let (sample, done) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the tracker should report immediately");
        assert!(matches!(sample, InstallSample::Installed { .. }));
        assert!(done);
    }

    #[test]
    fn tracker_ignores_non_numeric_ids() {
        let (tx, rx) = std::sync::mpsc::channel::<DownloadEvent>();
        track("gog:1234", &[], move |event| tx.send(event).is_ok());
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }
}
