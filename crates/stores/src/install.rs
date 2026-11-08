//! Steam install hand-off and appmanifest polling.
//!
//! The launcher never downloads Steam content itself. It asks the running
//! Steam client to install an app (`steam://install/<appid>`, as a
//! [`LaunchSpec`](pitagames_core::LaunchSpec)) and then watches the
//! `appmanifest_<appid>.acf` file until Steam reports the app as installed.
//!
//! Polling rather than watching is deliberate: the install may start before
//! the manifest exists, and the manifest is rewritten in place during updates,
//! so a 500 ms poll (the default) is both simpler and more reliable than
//! trying to keep a filesystem watch alive across library changes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::acf::{AppManifest, DownloadProgress, InstallPhase};
use crate::steam;

/// How often the Downloads view/CLI should re-read the manifest.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// One observation of an in-flight install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSample {
    /// Steam has not written the manifest yet; the request may still be queued.
    WaitingForManifest,
    /// The manifest exists but the app is not ready to launch.
    InProgress(InstallProgress),
    /// Steam reports an installed, idle app.
    Installed { manifest: PathBuf },
    /// The timeout elapsed before the install completed.
    TimedOut,
}

impl InstallSample {
    /// Whether polling can stop.
    pub const fn is_final(&self) -> bool {
        matches!(self, Self::Installed { .. } | Self::TimedOut)
    }
}

/// The phase and best-effort download progress of an in-flight install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    pub phase: InstallPhase,
    pub progress: Option<DownloadProgress>,
}

/// Polls Steam's manifests for one app.
#[derive(Debug, Clone)]
pub struct InstallMonitor {
    app_id: u32,
    libraries: Vec<PathBuf>,
    timeout: Duration,
    started: Instant,
}

impl InstallMonitor {
    /// Build a monitor over already-resolved library roots.
    pub fn new(
        app_id: u32,
        libraries: impl IntoIterator<Item = PathBuf>,
        timeout: Duration,
    ) -> Self {
        Self {
            app_id,
            libraries: libraries.into_iter().collect(),
            timeout,
            started: Instant::now(),
        }
    }

    /// Build a monitor by resolving each Steam root's library folders.
    ///
    /// This is the normal constructor for the GUI and CLI: pass
    /// [`pitagames_core::paths::steam_roots`](pitagames_core::paths::steam_roots)
    /// and each root's `libraryfolders.vdf` is expanded here.
    pub fn with_roots<I, P>(app_id: u32, roots: I, timeout: Duration) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut libraries: Vec<PathBuf> = Vec::new();
        for root in roots {
            for library in steam::discover_libraries(root.as_ref()).libraries {
                if !libraries.contains(&library) {
                    libraries.push(library);
                }
            }
        }
        Self::new(app_id, libraries, timeout)
    }

    pub fn app_id(&self) -> u32 {
        self.app_id
    }

    pub fn libraries(&self) -> &[PathBuf] {
        &self.libraries
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// One observation. `elapsed` is measured from the moment the install was
    /// requested; tests pass it explicitly, [`InstallMonitor::wait`] uses the
    /// monitor's clock.
    pub fn sample(&self, elapsed: Duration) -> InstallSample {
        if let Some((manifest_path, manifest)) = self.find_manifest() {
            if manifest.is_complete() {
                return InstallSample::Installed {
                    manifest: manifest_path,
                };
            }

            if elapsed >= self.timeout {
                return InstallSample::TimedOut;
            }

            return InstallSample::InProgress(InstallProgress {
                phase: manifest.phase(),
                progress: manifest.download_progress(),
            });
        }

        if elapsed >= self.timeout {
            InstallSample::TimedOut
        } else {
            InstallSample::WaitingForManifest
        }
    }

    /// Block until the app is installed, the timeout elapses, or `on_sample`
    /// asks to stop early.
    ///
    /// The callback runs once per poll, including the final sample. This is the
    /// shape the Downloads view needs: it can spawn a thread and forward each
    /// sample to the UI without making `stores` async.
    pub fn wait<F>(&self, interval: Duration, mut on_sample: F) -> InstallSample
    where
        F: FnMut(&InstallSample),
    {
        loop {
            let sample = self.sample(self.started.elapsed());
            on_sample(&sample);
            if sample.is_final() {
                return sample;
            }
            std::thread::sleep(interval);
        }
    }

    /// Prefer a complete manifest; otherwise return the first readable one.
    ///
    /// The same appid can linger in a library the game was moved out of, so a
    /// complete copy must win over an in-progress stale one.
    fn find_manifest(&self) -> Option<(PathBuf, AppManifest)> {
        let mut in_progress = None;
        for library in &self.libraries {
            let path = library
                .join("steamapps")
                .join(format!("appmanifest_{}.acf", self.app_id));
            let Ok(manifest) = AppManifest::parse_file(&path) else {
                continue;
            };

            if manifest.is_complete() {
                return Some((path, manifest));
            }
            if in_progress.is_none() {
                in_progress = Some((path, manifest));
            }
        }
        in_progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acf::flags;
    use std::fs;

    fn acf(app_id: u32, flags: u32, bytes: Option<(u64, u64)>) -> String {
        let counters = match bytes {
            Some((downloaded, total)) => format!(
                "\t\"BytesDownloaded\"\t\"{downloaded}\"\n\t\"BytesToDownload\"\t\"{total}\"\n"
            ),
            None => String::new(),
        };
        format!(
            r#""AppState"
{{
	"appid"		"{app_id}"
	"name"		"Test Game"
	"StateFlags"	"{flags}"
	"installdir"	"Test Game"
{counters}}}
"#
        )
    }

    fn library_with_manifest(app_id: u32, contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let steamapps = dir.path().join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        fs::write(
            steamapps.join(format!("appmanifest_{app_id}.acf")),
            contents,
        )
        .unwrap();
        dir
    }

    #[test]
    fn missing_manifest_waits_then_times_out() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("steamapps")).unwrap();
        let monitor = InstallMonitor::with_roots(620, [dir.path()], Duration::from_secs(10));

        assert_eq!(
            monitor.sample(Duration::ZERO),
            InstallSample::WaitingForManifest
        );
        assert_eq!(
            monitor.sample(Duration::from_secs(10)),
            InstallSample::TimedOut
        );
    }

    #[test]
    fn complete_manifest_is_terminal() {
        let dir = library_with_manifest(620, &acf(620, flags::FULLY_INSTALLED, None));
        let monitor = InstallMonitor::with_roots(620, [dir.path()], Duration::from_secs(10));

        let sample = monitor.sample(Duration::ZERO);
        let InstallSample::Installed { manifest } = sample else {
            panic!("expected Installed, got {sample:?}");
        };
        assert!(manifest.ends_with("steamapps/appmanifest_620.acf"));
    }

    #[test]
    fn update_required_counts_as_installed_for_polling() {
        let dir = library_with_manifest(620, &acf(620, flags::UPDATE_REQUIRED, None));
        let monitor = InstallMonitor::with_roots(620, [dir.path()], Duration::from_secs(10));

        assert!(matches!(
            monitor.sample(Duration::ZERO),
            InstallSample::Installed { .. }
        ));
    }

    #[test]
    fn in_flight_download_reports_phase_and_progress() {
        let dir = library_with_manifest(620, &acf(620, flags::DOWNLOADING, Some((1_000, 4_000))));
        let monitor = InstallMonitor::with_roots(620, [dir.path()], Duration::from_secs(10));

        let sample = monitor.sample(Duration::ZERO);
        let InstallSample::InProgress(progress) = sample else {
            panic!("expected InProgress, got {sample:?}");
        };
        assert_eq!(progress.phase, InstallPhase::Downloading);
        let bytes = progress.progress.unwrap();
        assert_eq!(bytes.downloaded, 1_000);
        assert_eq!(bytes.total, 4_000);
        assert_eq!(bytes.percent(), Some(25));
    }

    #[test]
    fn a_busy_update_is_not_terminal_even_with_the_installed_bit() {
        let bits = flags::FULLY_INSTALLED | flags::UPDATE_RUNNING;
        let dir = library_with_manifest(620, &acf(620, bits, None));
        let monitor = InstallMonitor::with_roots(620, [dir.path()], Duration::from_secs(10));

        let sample = monitor.sample(Duration::ZERO);
        assert!(
            matches!(
                sample,
                InstallSample::InProgress(InstallProgress {
                    phase: InstallPhase::Updating,
                    ..
                })
            ),
            "{sample:?}"
        );
    }

    #[test]
    fn finds_a_manifest_in_a_secondary_library() {
        let root = tempfile::tempdir().unwrap();
        let other = library_with_manifest(620, &acf(620, flags::FULLY_INSTALLED, None));

        let root_steamapps = root.path().join("steamapps");
        fs::create_dir_all(&root_steamapps).unwrap();
        fs::write(
            root_steamapps.join("libraryfolders.vdf"),
            format!(
                r#""libraryfolders"
{{
    "0"
    {{
        "path" "{}"
    }}
}}
"#,
                other.path().display()
            ),
        )
        .unwrap();

        let monitor = InstallMonitor::with_roots(620, [root.path()], Duration::from_secs(10));
        assert_eq!(monitor.libraries(), [other.path()]);
        assert!(matches!(
            monitor.sample(Duration::ZERO),
            InstallSample::Installed { .. }
        ));
    }

    #[test]
    fn prefers_a_complete_copy_over_a_stale_in_progress_one() {
        let stale = library_with_manifest(620, &acf(620, flags::DOWNLOADING, None));
        let complete = library_with_manifest(620, &acf(620, flags::FULLY_INSTALLED, None));
        let monitor = InstallMonitor::new(
            620,
            [stale.path().to_path_buf(), complete.path().to_path_buf()],
            Duration::from_secs(10),
        );

        let sample = monitor.sample(Duration::ZERO);
        let InstallSample::Installed { manifest } = sample else {
            panic!("expected Installed, got {sample:?}");
        };
        assert!(manifest.starts_with(complete.path()));
    }

    #[test]
    fn wait_polls_until_the_manifest_appears() {
        let dir = tempfile::tempdir().unwrap();
        let steamapps = dir.path().join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        let manifest_path = steamapps.join("appmanifest_620.acf");
        let contents = acf(620, flags::FULLY_INSTALLED, None);

        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            fs::write(manifest_path, contents).unwrap();
        });

        let monitor = InstallMonitor::with_roots(620, [dir.path()], Duration::from_secs(5));
        let mut samples = Vec::new();
        let last = monitor.wait(Duration::from_millis(10), |sample| {
            samples.push(sample.clone());
        });
        writer.join().unwrap();

        assert!(matches!(last, InstallSample::Installed { .. }));
        assert!(
            samples
                .iter()
                .any(|sample| matches!(sample, InstallSample::WaitingForManifest))
        );
        assert!(samples.len() >= 2);
    }
}
