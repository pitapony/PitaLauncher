//! Watching Steam libraries for changes.
//!
//! The GUI keeps the library grid fresh without polling every manifest: each
//! `<library>/steamapps` directory is watched (non-recursively), manifest and
//! `libraryfolders.vdf` changes are classified, and bursts are coalesced by a
//! [`Debouncer`] before reaching the UI. Re-parsing a manifest is the caller's
//! job; this module only reports which files changed.
//!
//! Steam writes several files in quick succession during an install or update,
//! hence the default two-second quiet period rather than one event per write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// The plan's debounce window: 2 s of quiet after the last change.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(2);

/// Whether a watched file was written or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOp {
    Written,
    Removed,
}

/// One relevant change inside a `steamapps` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryChange {
    /// An `appmanifest_<appid>.acf` was created, modified or removed.
    Manifest {
        app_id: u32,
        path: PathBuf,
        op: ChangeOp,
    },
    /// `libraryfolders.vdf` changed: libraries may have been added or removed.
    LibraryFolders { path: PathBuf, op: ChangeOp },
}

impl LibraryChange {
    pub fn path(&self) -> &Path {
        match self {
            Self::Manifest { path, .. } | Self::LibraryFolders { path, .. } => path,
        }
    }

    pub fn op(&self) -> ChangeOp {
        match self {
            Self::Manifest { op, .. } | Self::LibraryFolders { op, .. } => *op,
        }
    }
}

/// A running watcher. Dropping it stops the native watch and joins the worker.
///
/// Consumers loop on [`LibraryWatcher::recv`] (or `recv_timeout` in a UI event
/// loop) and re-parse the reported manifests.
pub struct LibraryWatcher {
    // `Option` so `Drop` can release the native watcher (and its callback)
    // before joining the worker; otherwise the worker would never disconnect.
    watcher: Option<RecommendedWatcher>,
    batches: Receiver<Vec<LibraryChange>>,
    worker: Option<JoinHandle<()>>,
}

impl LibraryWatcher {
    /// Watch each library root's `steamapps` directory.
    ///
    /// Roots without a `steamapps` directory are skipped; if none exist this
    /// returns [`WatchError::NothingToWatch`], which callers may treat as
    /// "Steam is not installed here" rather than a failure.
    pub fn new<I, P>(libraries: I, debounce: Duration) -> Result<Self, WatchError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let (raw_tx, raw_rx) = mpsc::channel();
        let (batches, batch_rx) = mpsc::channel();

        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<notify::Event>| {
                // Access events are ignored: our own re-parse would retrigger
                // them and the UI would never go quiet.
                let Ok(event) = result else { return };
                let Some(op) = change_op(&event.kind) else {
                    return;
                };
                for path in event.paths {
                    if let Some(change) = classify(&path, op) {
                        if raw_tx.send(change).is_err() {
                            break;
                        }
                    }
                }
            },
            notify::Config::default(),
        )
        .map_err(|source| WatchError::Watcher { source })?;

        let mut watched = 0usize;
        for library in libraries {
            let steamapps = library.as_ref().join("steamapps");
            if !steamapps.is_dir() {
                continue;
            }
            watcher
                .watch(&steamapps, RecursiveMode::NonRecursive)
                .map_err(|source| WatchError::Watch {
                    path: steamapps.clone(),
                    source,
                })?;
            watched += 1;
        }

        if watched == 0 {
            return Err(WatchError::NothingToWatch);
        }

        let worker = thread::Builder::new()
            .name("pitagames-steam-watch".into())
            .spawn(move || {
                let mut debouncer = Debouncer::new(debounce);
                loop {
                    match raw_rx.recv_timeout(debouncer.next_timeout(Instant::now())) {
                        Ok(change) => debouncer.push(change, Instant::now()),
                        Err(RecvTimeoutError::Timeout) => {
                            if let Some(batch) = debouncer.ready(Instant::now()) {
                                if batches.send(batch).is_err() {
                                    break;
                                }
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            // Flush a final partial batch so a shutdown does not
                            // silently drop a just-written manifest.
                            if let Some(batch) = debouncer.flush() {
                                let _ = batches.send(batch);
                            }
                            break;
                        }
                    }
                }
            })
            .map_err(|source| WatchError::Spawn { source })?;

        Ok(Self {
            watcher: Some(watcher),
            batches: batch_rx,
            worker: Some(worker),
        })
    }

    /// Wait for the next debounced batch.
    pub fn recv(&self) -> Option<Vec<LibraryChange>> {
        self.batches.recv().ok()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<LibraryChange>, RecvTimeoutError> {
        self.batches.recv_timeout(timeout)
    }

    pub fn try_recv(&self) -> Result<Vec<LibraryChange>, TryRecvError> {
        self.batches.try_recv()
    }
}

impl Drop for LibraryWatcher {
    fn drop(&mut self) {
        // Drop the native watcher first: that releases the callback and its
        // sender, which disconnects the worker and unblocks its join.
        self.watcher.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Coalesce a burst of changes into one batch after a quiet period.
///
/// Kept separate from the notify plumbing so the timing rules are testable
/// without touching the filesystem.
#[derive(Debug)]
pub struct Debouncer {
    delay: Duration,
    pending: BTreeMap<PathBuf, LibraryChange>,
    last_change: Option<Instant>,
}

impl Debouncer {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            pending: BTreeMap::new(),
            last_change: None,
        }
    }

    /// Record a change. A repeated path replaces the earlier change, so
    /// create-then-remove inside one window reports the final state only.
    pub fn push(&mut self, change: LibraryChange, now: Instant) {
        self.last_change = Some(now);
        self.pending.insert(change.path().to_path_buf(), change);
    }

    /// Return a batch once `delay` has elapsed since the last change.
    pub fn ready(&mut self, now: Instant) -> Option<Vec<LibraryChange>> {
        let last = self.last_change?;
        if now.saturating_duration_since(last) >= self.delay {
            Some(self.take())
        } else {
            None
        }
    }

    /// Return everything pending regardless of the quiet period.
    pub fn flush(&mut self) -> Option<Vec<LibraryChange>> {
        (!self.pending.is_empty()).then(|| self.take())
    }

    /// How long a caller should wait before calling [`Debouncer::ready`].
    pub fn next_timeout(&self, now: Instant) -> Duration {
        match self.last_change {
            Some(last) => self
                .delay
                .saturating_sub(now.saturating_duration_since(last)),
            None => self.delay,
        }
    }

    fn take(&mut self) -> Vec<LibraryChange> {
        self.last_change = None;
        std::mem::take(&mut self.pending).into_values().collect()
    }
}

fn change_op(kind: &EventKind) -> Option<ChangeOp> {
    match kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any => Some(ChangeOp::Written),
        EventKind::Remove(_) => Some(ChangeOp::Removed),
        // Access is ignored on purpose; Other carries no reliable meaning.
        _ => None,
    }
}

/// Classify a path that notify reported inside a watched `steamapps` dir.
fn classify(path: &Path, op: ChangeOp) -> Option<LibraryChange> {
    let name = path.file_name()?.to_str()?;

    if name.eq_ignore_ascii_case("libraryfolders.vdf") {
        return Some(LibraryChange::LibraryFolders {
            path: path.to_path_buf(),
            op,
        });
    }

    let app_id = name
        .strip_prefix("appmanifest_")?
        .strip_suffix(".acf")?
        .parse::<u32>()
        .ok()?;

    Some(LibraryChange::Manifest {
        app_id,
        path: path.to_path_buf(),
        op,
    })
}

/// Why a watcher could not be started.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("could not start the filesystem watcher: {source}")]
    Watcher {
        #[source]
        source: notify::Error,
    },

    #[error("could not watch {path}: {source}")]
    Watch {
        path: PathBuf,
        #[source]
        source: notify::Error,
    },

    #[error("could not start the watcher thread: {source}")]
    Spawn {
        #[source]
        source: std::io::Error,
    },

    #[error("no Steam library has a steamapps directory to watch")]
    NothingToWatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn manifest(app_id: u32) -> LibraryChange {
        LibraryChange::Manifest {
            app_id,
            path: PathBuf::from(format!("/steam/steamapps/appmanifest_{app_id}.acf")),
            op: ChangeOp::Written,
        }
    }

    #[test]
    fn classifies_manifests_and_library_folders() {
        let path = Path::new("/steam/steamapps/appmanifest_620.acf");
        let change = classify(path, ChangeOp::Removed).unwrap();
        assert_eq!(
            change,
            LibraryChange::Manifest {
                app_id: 620,
                path: path.to_path_buf(),
                op: ChangeOp::Removed,
            }
        );
        assert_eq!(change.path(), path);

        assert_eq!(
            classify(
                Path::new("/steam/steamapps/libraryfolders.vdf"),
                ChangeOp::Written
            )
            .unwrap(),
            LibraryChange::LibraryFolders {
                path: PathBuf::from("/steam/steamapps/libraryfolders.vdf"),
                op: ChangeOp::Written,
            }
        );
    }

    #[test]
    fn ignores_unrelated_and_malformed_paths() {
        for path in [
            "/steam/steamapps/common/Portal 2/portal2.exe",
            "/steam/steamapps/appmanifest_.acf",
            "/steam/steamapps/appmanifest_620.ACF",
            "/steam/steamapps/appmanifest_620.acf.tmp",
            "/steam/steamapps/libraryfolders.vdf.bak",
            "/steam/steamapps/appmanifest_abc.acf",
        ] {
            assert!(
                classify(Path::new(path), ChangeOp::Written).is_none(),
                "{path}"
            );
        }
    }

    #[test]
    fn debouncer_coalesces_paths_until_the_window_is_quiet() {
        let base = Instant::now();
        let mut debouncer = Debouncer::new(Duration::from_secs(2));

        debouncer.push(manifest(620), base);
        debouncer.push(manifest(440), base + Duration::from_millis(500));
        // Reparsing the same manifest reports its final state once.
        debouncer.push(manifest(620), base + Duration::from_millis(900));

        assert!(
            debouncer
                .ready(base + Duration::from_millis(1_000))
                .is_none()
        );
        // The window restarts at the newest change (base + 900 ms).
        assert!(
            debouncer
                .ready(base + Duration::from_millis(2_800))
                .is_none()
        );

        let batch = debouncer
            .ready(base + Duration::from_millis(3_000))
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], manifest(440));
        assert_eq!(batch[1], manifest(620));

        assert!(
            debouncer
                .ready(base + Duration::from_millis(10_000))
                .is_none()
        );
        assert_eq!(
            debouncer.next_timeout(base + Duration::from_millis(10_000)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn debouncer_flushes_on_shutdown() {
        let mut debouncer = Debouncer::new(Duration::from_secs(2));
        assert!(debouncer.flush().is_none());

        debouncer.push(manifest(620), Instant::now());
        assert_eq!(debouncer.flush().unwrap(), vec![manifest(620)]);
        assert!(debouncer.flush().is_none());
    }

    #[test]
    fn watcher_reports_a_debounced_manifest_write() {
        let dir = tempfile::tempdir().unwrap();
        let steamapps = dir.path().join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();

        let watcher = LibraryWatcher::new([dir.path()], Duration::from_millis(100)).unwrap();
        fs::write(steamapps.join("appmanifest_620.acf"), "\"AppState\" {}\n").unwrap();

        let batch = watcher
            .recv_timeout(Duration::from_secs(10))
            .expect("manifest write should be reported");
        assert!(
            batch.iter().any(|change| matches!(
                change,
                LibraryChange::Manifest {
                    app_id: 620,
                    op: ChangeOp::Written,
                    ..
                }
            )),
            "unexpected batch: {batch:?}"
        );
    }

    #[test]
    fn watcher_skips_libraries_without_steamapps() {
        let dir = tempfile::tempdir().unwrap();
        let error = match LibraryWatcher::new([dir.path()], DEFAULT_DEBOUNCE) {
            Ok(_) => panic!("a library without steamapps must not start a watcher"),
            Err(error) => error,
        };
        assert!(matches!(error, WatchError::NothingToWatch));
    }
}
