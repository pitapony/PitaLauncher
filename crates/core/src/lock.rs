//! The single-instance lock.
//!
//! PitaGames owns one database and writes Steam's config files; two copies of
//! the launcher scanning and writing at once is a bug, not a feature. The GUI
//! takes an exclusive advisory lock at startup, and a second launch reports the
//! running instance instead of scanning and writing again.
//!
//! The lock is `flock(2)`-based, reached through [`std::fs::File::try_lock`], so
//! the kernel releases it when the process exits or crashes — there is no stale
//! pid file to clean up and no lock left behind after a `SIGKILL`. It is
//! advisory: it protects only the code that takes it, which is why the shim and
//! the CLI deliberately do not take it (they must run while the GUI does).

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// An exclusive advisory lock held for as long as this value is alive.
///
/// Dropping it closes the file, which releases the lock. Keep it in scope for
/// the whole process; a lock that is dropped early protects nothing.
#[derive(Debug)]
pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Take the lock at `path`, creating the file (and its parent) if needed.
    ///
    /// Returns [`LockError::AlreadyRunning`] when another process holds it. The
    /// owner's pid, written by whoever holds the lock, is carried along for the
    /// message when it can be read.
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, LockError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|source| LockError::Io {
                action: "create the parent directory of",
                path: path.clone(),
                source,
            })?;
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LockError::Io {
                action: "open",
                path: path.clone(),
                source,
            })?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(LockError::AlreadyRunning {
                    path,
                    pid: read_pid(&mut file),
                });
            }
            Err(TryLockError::Error(source)) => {
                return Err(LockError::Io {
                    action: "lock",
                    path,
                    source,
                });
            }
        }

        // Record the owner so the next process can name it. Failing to write
        // this is not fatal: the lock is already held.
        let _ = write_pid(&mut file);

        Ok(Self { file, path })
    }

    /// The lock file this instance holds.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Closing the file releases the `flock`; unlocking explicitly documents
        // that and swallows the "not locked" case.
        let _ = self.file.unlock();
    }
}

/// Write the current pid into the (already locked) file.
fn write_pid(file: &mut File) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()
}

/// Read the pid a previous owner left behind, if any.
fn read_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    // The lock does not stop us reading the file; size it to avoid a huge read.
    file.take(64).read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// Single-instance failures.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process already holds the lock.
    #[error("another PitaGames instance is already running")]
    AlreadyRunning {
        path: PathBuf,
        /// The holder's pid, when it could be read.
        pid: Option<u32>,
    },

    #[error("failed to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_lock_is_acquired_and_reports_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pitagames.lock");

        let lock = InstanceLock::acquire(&path).unwrap();
        assert_eq!(lock.path(), path);
        assert!(path.is_file(), "the lock file is created");
    }

    #[test]
    fn a_second_acquire_reports_the_running_instance() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pitagames.lock");

        let _held = InstanceLock::acquire(&path).unwrap();
        let error = InstanceLock::acquire(&path).unwrap_err();

        match error {
            LockError::AlreadyRunning {
                path: reported,
                pid,
            } => {
                assert_eq!(reported, path);
                assert_eq!(pid, Some(std::process::id()));
            }
            other => panic!("expected AlreadyRunning, got {other}"),
        }
    }

    #[test]
    fn dropping_the_lock_releases_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pitagames.lock");

        {
            let _held = InstanceLock::acquire(&path).unwrap();
            assert!(InstanceLock::acquire(&path).is_err());
        }

        // The first lock is gone, so the file can be locked again.
        assert!(InstanceLock::acquire(&path).is_ok());
    }

    #[test]
    fn acquiring_creates_missing_parent_directories() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a/b/pitagames.lock");

        assert!(InstanceLock::acquire(&path).is_ok());
        assert!(path.is_file());
    }
}
