//! Controlled writes to Steam's `localconfig.vdf`.
//!
//! This is the highest-risk code path in the launcher: the file is Steam's, and
//! a bad write breaks per-game settings or the whole client's user config. The
//! rules from the plan are enforced here, not documented and hoped for:
//!
//! 1. refuse while Steam is running (its own write on exit would win);
//! 2. change only the requested app's `LaunchOptions` key
//!    ([`crate::localconfig`] splices the bytes);
//! 3. keep a timestamped backup of the untruncated original;
//! 4. replace atomically: temp file in the same directory, `fsync`, `rename`;
//! 5. record what was written so a restore can either copy the original bytes
//!    back (byte-identical) or splice only this key when Steam changed other
//!    things in between.
//!
//! The restore record is a small JSON file next to the backup. The GUI will
//! mirror the same fields into SQLite when it owns the apply flow; keeping the
//! record next to the backup means `pitagames-cli launch-options --restore`
//! works without a database.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::localconfig;

/// What the Steam client is doing, as far as this machine can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteamProcessState {
    /// No live `steam` process and no surviving pid file.
    Stopped,
    /// A live `steam` process. Writing now would be overwritten on exit.
    Running { pid: u32 },
    /// The platform has no `/proc`; the caller must force the write.
    Unknown,
}

impl SteamProcessState {
    pub fn describe(self) -> String {
        match self {
            Self::Stopped => "not running".to_string(),
            Self::Running { pid } => format!("running (pid {pid})"),
            Self::Unknown => "unknown (cannot inspect processes here)".to_string(),
        }
    }
}

/// Where to keep backups and how to treat a running Steam client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOptions {
    /// Usually `<XDG_DATA_HOME>/pitagames/backups/steam`.
    pub backup_dir: PathBuf,
    /// Only for tests and an explicit user override (`--force`).
    pub allow_running: bool,
}

impl WriteOptions {
    pub fn new(backup_dir: impl Into<PathBuf>) -> Self {
        Self {
            backup_dir: backup_dir.into(),
            allow_running: false,
        }
    }
}

/// What [`apply_launch_options`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReport {
    pub localconfig: PathBuf,
    pub user_id: String,
    pub app_id: String,
    /// The value before the write; `None` means the key was absent.
    pub previous: Option<String>,
    /// The value after the write; `None` means the key was removed.
    pub applied: Option<String>,
    pub changed: bool,
    /// `None` when nothing was written.
    pub backup: Option<PathBuf>,
}

/// What [`restore_launch_options`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub localconfig: PathBuf,
    pub user_id: String,
    pub app_id: String,
    /// The launch-options value that was put back.
    pub restored: Option<String>,
    /// `true` when the whole file was restored byte-for-byte from the backup.
    /// `false` means Steam changed other keys in the meantime, so only this
    /// app's `LaunchOptions` was spliced back and the rest was left alone.
    pub exact: bool,
    /// The original backup used for the restore.
    pub backup: PathBuf,
}

/// Detect a running Steam client for a Steam root.
///
/// Linux only: `<root>/steam.pid` plus a `/proc/<pid>/comm` scan. Other
/// platforms return [`SteamProcessState::Unknown`] instead of guessing, and the
/// writer refuses unless `allow_running` is set.
pub fn detect_steam_process(steam_root: &Path) -> SteamProcessState {
    #[cfg(target_os = "linux")]
    {
        detect_steam_process_in(steam_root, Path::new("/proc"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = steam_root;
        SteamProcessState::Unknown
    }
}

/// `detect_steam_process` against an explicit `/proc`-shaped directory.
///
/// Tests fabricate a `proc/<pid>/comm` tree; production always passes `/proc`.
pub fn detect_steam_process_in(steam_root: &Path, proc_root: &Path) -> SteamProcessState {
    if let Some(pid) = steam_pid_file(steam_root) {
        if proc_root.join(pid.to_string()).is_dir() {
            return SteamProcessState::Running { pid };
        }
    }

    let Ok(entries) = fs::read_dir(proc_root) else {
        return SteamProcessState::Stopped;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let comm = fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        // Deliberately exact: `steamwebhelper` alone can outlive the client.
        if comm.trim() == "steam" {
            return SteamProcessState::Running { pid };
        }
    }

    SteamProcessState::Stopped
}

fn steam_pid_file(steam_root: &Path) -> Option<u32> {
    let mut candidates = vec![steam_root.join("steam.pid")];
    // ~/.steam/steam is a symlink whose pid file lives one level up.
    if steam_root.file_name().is_some_and(|name| name == "steam") {
        if let Some(parent) = steam_root.parent() {
            candidates.push(parent.join("steam.pid"));
        }
    }

    candidates.into_iter().find_map(|path| {
        let text = fs::read_to_string(path).ok()?;
        let pid = text.trim().parse::<u32>().ok()?;
        (pid > 0).then_some(pid)
    })
}

fn check_steam_closed(state: SteamProcessState, allow_running: bool) -> Result<(), Error> {
    match state {
        SteamProcessState::Stopped => Ok(()),
        SteamProcessState::Running { pid } if !allow_running => Err(Error::SteamRunning { pid }),
        SteamProcessState::Running { .. } => Ok(()),
        SteamProcessState::Unknown if !allow_running => Err(Error::SteamStateUnknown),
        SteamProcessState::Unknown => Ok(()),
    }
}

/// Read one app's launch options, or `None` when the key/section is absent.
pub fn read_launch_options(localconfig: &Path, app_id: &str) -> Result<Option<String>, Error> {
    let text = read_text(localconfig)?;
    Ok(localconfig::read(&text, app_id)?)
}

/// Compute the edit without touching anything (the CLI's `--dry-run`).
pub fn plan_launch_options(
    localconfig: &Path,
    app_id: &str,
    value: Option<&str>,
) -> Result<localconfig::Edit, Error> {
    let text = read_text(localconfig)?;
    Ok(localconfig::edit(&text, app_id, value)?)
}

/// Set (`Some`) or remove (`None`) one app's launch options.
///
/// Refuses while Steam is running unless `options.allow_running` is set. The
/// first write for an app keeps the original file and value; repeated writes
/// keep that same restore point.
pub fn apply_launch_options(
    localconfig: &Path,
    app_id: &str,
    value: Option<&str>,
    steam: SteamProcessState,
    options: &WriteOptions,
) -> Result<WriteReport, Error> {
    check_steam_closed(steam, options.allow_running)?;

    let original = read_bytes(localconfig)?;
    let original_text = String::from_utf8(original.clone()).map_err(|_| Error::NotUtf8 {
        path: localconfig.to_path_buf(),
    })?;
    let edit = localconfig::edit(&original_text, app_id, value)?;
    let user_id = user_id_for(localconfig);
    let applied = value.map(str::to_string);

    if !edit.changed {
        return Ok(WriteReport {
            localconfig: localconfig.to_path_buf(),
            user_id,
            app_id: app_id.to_string(),
            previous: edit.previous,
            applied,
            changed: false,
            backup: None,
        });
    }

    fs::create_dir_all(&options.backup_dir).map_err(|source| Error::Io {
        path: options.backup_dir.clone(),
        source,
    })?;

    let journal_path = journal_path(&options.backup_dir, &user_id, app_id);
    let existing = match load_journal(&journal_path)? {
        Some(journal)
            if journal.app_id == app_id
                && journal.user_id == user_id
                && journal.localconfig == localconfig =>
        {
            Some(journal)
        }
        Some(journal) => {
            // Same user id and app under a different Steam root: do not
            // overwrite the other file's restore point.
            return Err(Error::RecordMismatch {
                app_id: app_id.to_string(),
                recorded: journal.localconfig,
                requested: localconfig.to_path_buf(),
            });
        }
        None => None,
    };

    // A restore point whose backup disappeared cannot be trusted: writing now
    // would leave a change with no way back.
    if let Some(journal) = &existing {
        let backup = fs::read(&journal.backup).map_err(|_| Error::BackupModified {
            app_id: app_id.to_string(),
            path: journal.backup.clone(),
        })?;
        if sha256_hex(&backup) != journal.original_sha256 {
            return Err(Error::BackupModified {
                app_id: app_id.to_string(),
                path: journal.backup.clone(),
            });
        }
    }

    // Keep the first restore point: a second apply must not make the already
    // shimmed line the "original" launch options.
    let (previous, backup, original_sha256, created_unix) = match existing {
        Some(journal) => (
            journal.previous,
            journal.backup,
            journal.original_sha256,
            journal.created_unix,
        ),
        None => {
            let backup = unique_backup_path(&options.backup_dir, &user_id, app_id, "");
            write_new_file(&backup, &original)?;
            (
                edit.previous.clone(),
                backup,
                sha256_hex(&original),
                OffsetDateTime::now_utc().unix_timestamp(),
            )
        }
    };

    let journal = Journal {
        app_id: app_id.to_string(),
        user_id: user_id.clone(),
        localconfig: localconfig.to_path_buf(),
        backup: backup.clone(),
        previous: previous.clone(),
        applied: applied.clone(),
        original_sha256,
        written_sha256: sha256_hex(edit.text.as_bytes()),
        created_unix,
    };

    // The record goes first: if the process dies before the replace, a later
    // restore sees that the file does not match and simply puts the same value
    // back. A backup without a record is much worse than a record without a
    // write.
    write_journal(&journal_path, &journal)?;
    if let Err(error) = atomic_write(localconfig, edit.text.as_bytes()) {
        let _ = fs::remove_file(&journal_path);
        return Err(error);
    }

    Ok(WriteReport {
        localconfig: localconfig.to_path_buf(),
        user_id,
        app_id: app_id.to_string(),
        previous,
        applied,
        changed: true,
        backup: Some(backup),
    })
}

/// Undo the last [`apply_launch_options`] for an app.
///
/// When the file is byte-for-byte what we wrote, the original file is copied
/// back and the result is byte-identical to the pre-write state. When Steam has
/// changed something else since (playtime, another app), only this app's
/// `LaunchOptions` is put back, so Steam's changes are never clobbered.
pub fn restore_launch_options(
    localconfig: &Path,
    app_id: &str,
    steam: SteamProcessState,
    options: &WriteOptions,
) -> Result<RestoreReport, Error> {
    check_steam_closed(steam, options.allow_running)?;

    let user_id = user_id_for(localconfig);
    let journal_path = journal_path(&options.backup_dir, &user_id, app_id);
    let journal = load_journal(&journal_path)?.ok_or_else(|| Error::NoRecord {
        app_id: app_id.to_string(),
        path: journal_path.clone(),
    })?;

    if journal.localconfig != localconfig {
        return Err(Error::RecordMismatch {
            app_id: app_id.to_string(),
            recorded: journal.localconfig,
            requested: localconfig.to_path_buf(),
        });
    }

    let current = read_bytes(localconfig)?;

    if sha256_hex(&current) == journal.written_sha256 {
        let backup_bytes = read_bytes(&journal.backup).map_err(|_| Error::BackupModified {
            app_id: app_id.to_string(),
            path: journal.backup.clone(),
        })?;
        if sha256_hex(&backup_bytes) != journal.original_sha256 {
            return Err(Error::BackupModified {
                app_id: app_id.to_string(),
                path: journal.backup.clone(),
            });
        }

        atomic_write(localconfig, &backup_bytes)?;
        remove_journal(&journal_path);
        return Ok(RestoreReport {
            localconfig: localconfig.to_path_buf(),
            user_id,
            app_id: app_id.to_string(),
            restored: journal.previous,
            exact: true,
            backup: journal.backup,
        });
    }

    let current_text = String::from_utf8(current.clone()).map_err(|_| Error::NotUtf8 {
        path: localconfig.to_path_buf(),
    })?;
    let edit = localconfig::edit(&current_text, app_id, journal.previous.as_deref())?;
    if edit.changed {
        // This write is itself undoable: keep what the file looked like before
        // the merge.
        let pre_restore = unique_backup_path(&options.backup_dir, &user_id, app_id, "-pre-restore");
        write_new_file(&pre_restore, &current)?;
        atomic_write(localconfig, edit.text.as_bytes())?;
    }
    remove_journal(&journal_path);

    Ok(RestoreReport {
        localconfig: localconfig.to_path_buf(),
        user_id,
        app_id: app_id.to_string(),
        restored: journal.previous,
        exact: false,
        backup: journal.backup,
    })
}

/// One restore point per app and Steam user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Journal {
    app_id: String,
    user_id: String,
    localconfig: PathBuf,
    backup: PathBuf,
    previous: Option<String>,
    applied: Option<String>,
    original_sha256: String,
    written_sha256: String,
    created_unix: i64,
}

fn user_id_for(localconfig: &Path) -> String {
    let user_id = localconfig
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("default");

    user_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn journal_path(dir: &Path, user_id: &str, app_id: &str) -> PathBuf {
    dir.join(format!("localconfig-{user_id}-{app_id}.json"))
}

fn unique_backup_path(dir: &Path, user_id: &str, app_id: &str, suffix: &str) -> PathBuf {
    let stamp = OffsetDateTime::now_utc().unix_timestamp_nanos();
    dir.join(format!(
        "localconfig-{user_id}-{app_id}-{stamp}-{}{suffix}.vdf",
        process::id()
    ))
}

fn load_journal(path: &Path) -> Result<Option<Journal>, Error> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn write_journal(path: &Path, journal: &Journal) -> Result<(), Error> {
    let bytes = serde_json::to_vec_pretty(journal)?;
    atomic_write(path, &bytes)
}

fn remove_journal(path: &Path) {
    // A failed cleanup is harmless: the next restore sees a mismatching hash
    // and retries the value-level path.
    let _ = fs::remove_file(path);
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_text(path: &Path) -> Result<String, Error> {
    String::from_utf8(read_bytes(path)?).map_err(|_| Error::NotUtf8 {
        path: path.to_path_buf(),
    })
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();

    result.map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Write `path` through a sibling temp file so a crash can never leave a
/// half-written config behind.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("localconfig.vdf");
    let stamp = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let temp = parent.join(format!(".{name}.tmp-{}-{stamp}", process::id()));

    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;

        // Keep Steam's file mode (usually 0644) on the replacement.
        if let Ok(metadata) = fs::metadata(path) {
            fs::set_permissions(&temp, metadata.permissions())?;
        }

        fs::rename(&temp, path)?;

        // Make the rename durable. Directory fsync is not portable everywhere,
        // so a failure here must not fail the write that already succeeded.
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }

    result.map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Why a config write or restore was refused.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{path} is not valid UTF-8; Steam writes UTF-8 config files")]
    NotUtf8 { path: PathBuf },

    #[error(transparent)]
    Edit(#[from] localconfig::Error),

    #[error(
        "Steam is running (pid {pid}); quit Steam and retry \
         (its config would overwrite ours on exit)"
    )]
    SteamRunning { pid: u32 },

    #[error("cannot confirm that Steam is closed on this platform; pass --force to write anyway")]
    SteamStateUnknown,

    #[error("no PitaGames restore record for app {app_id} at {path}")]
    NoRecord { app_id: String, path: PathBuf },

    #[error("the restore record for app {app_id} belongs to {recorded}, not {requested}")]
    RecordMismatch {
        app_id: String,
        recorded: PathBuf,
        requested: PathBuf,
    },

    #[error(
        "the backup for app {app_id} at {path} is missing or changed; refusing to restore from it"
    )]
    BackupModified { app_id: String, path: PathBuf },

    #[error("could not read or write the restore record: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = include_str!("../../../fixtures/steam/localconfig_empty.vdf");
    const WITH_OPTIONS: &str = include_str!("../../../fixtures/steam/localconfig_with_options.vdf");
    const QUOTING: &str = include_str!("../../../fixtures/steam/localconfig_quoting.vdf");

    fn fixture(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join("Steam/userdata/123/config/localconfig.vdf");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn options(dir: &Path) -> WriteOptions {
        WriteOptions::new(dir.join("backups"))
    }

    fn backup_files(dir: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(dir.join("backups"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        files.sort();
        files
    }

    #[test]
    fn set_then_restore_is_byte_identical_for_all_fixtures() {
        for (name, fixture_text) in [
            ("empty", EMPTY),
            ("with options", WITH_OPTIONS),
            ("quoting", QUOTING),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = fixture(dir.path(), fixture_text);
            let original = fs::read(&path).unwrap();

            let report = apply_launch_options(
                &path,
                "620",
                Some("pitagames-shim 620 %command%"),
                SteamProcessState::Stopped,
                &options(dir.path()),
            )
            .unwrap_or_else(|error| panic!("{name}: {error}"));

            assert!(report.changed, "{name}");
            assert_eq!(report.user_id, "123", "{name}");
            let backup = report.backup.as_ref().unwrap();
            assert_eq!(fs::read(backup).unwrap(), original, "{name}");

            let restored = restore_launch_options(
                &path,
                "620",
                SteamProcessState::Stopped,
                &options(dir.path()),
            )
            .unwrap_or_else(|error| panic!("{name}: {error}"));

            assert!(restored.exact, "{name}");
            assert_eq!(fs::read(&path).unwrap(), original, "{name}");
            assert!(!journal_path(&dir.path().join("backups"), "123", "620").exists());
        }
    }

    #[test]
    fn restore_after_steam_changed_other_keys_keeps_steams_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), WITH_OPTIONS);

        apply_launch_options(
            &path,
            "620",
            Some("pitagames-shim 620 %command%"),
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();

        let changed = fs::read_to_string(&path)
            .unwrap()
            .replace("\"Playtime\"\t\t\"9000\"", "\"Playtime\"\t\t\"9999\"");
        fs::write(&path, &changed).unwrap();

        let restored = restore_launch_options(
            &path,
            "620",
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();

        assert!(!restored.exact);
        assert_eq!(restored.restored.as_deref(), Some("-novid %command%"));
        assert_eq!(
            read_launch_options(&path, "620").unwrap().as_deref(),
            Some("-novid %command%")
        );
        assert!(fs::read_to_string(&path).unwrap().contains("9999"));
    }

    #[test]
    fn a_running_steam_client_refuses_the_write_before_touching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), WITH_OPTIONS);
        let original = fs::read(&path).unwrap();

        let error = apply_launch_options(
            &path,
            "620",
            Some("anything"),
            SteamProcessState::Running { pid: 4242 },
            &options(dir.path()),
        )
        .unwrap_err();

        assert!(matches!(error, Error::SteamRunning { pid: 4242 }));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!dir.path().join("backups").exists());
    }

    #[test]
    fn force_overrides_a_running_or_unknown_steam_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), EMPTY);
        let mut write = options(dir.path());
        write.allow_running = true;

        apply_launch_options(
            &path,
            "620",
            Some("a"),
            SteamProcessState::Running { pid: 7 },
            &write,
        )
        .unwrap();
        apply_launch_options(&path, "620", Some("b"), SteamProcessState::Unknown, &write).unwrap();

        assert_eq!(
            read_launch_options(&path, "620").unwrap().as_deref(),
            Some("b")
        );
    }

    #[test]
    fn unknown_state_is_refused_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), EMPTY);

        let error = apply_launch_options(
            &path,
            "620",
            Some("x"),
            SteamProcessState::Unknown,
            &options(dir.path()),
        )
        .unwrap_err();
        assert!(matches!(error, Error::SteamStateUnknown));
    }

    #[test]
    fn repeated_writes_keep_the_original_restore_point() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), WITH_OPTIONS);
        let original = fs::read(&path).unwrap();

        for value in ["first %command%", "second %command%", "third %command%"] {
            apply_launch_options(
                &path,
                "620",
                Some(value),
                SteamProcessState::Stopped,
                &options(dir.path()),
            )
            .unwrap();
        }

        let journal_path = journal_path(&dir.path().join("backups"), "123", "620");
        let journal: Journal = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        assert_eq!(journal.previous.as_deref(), Some("-novid %command%"));

        let restored = restore_launch_options(
            &path,
            "620",
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();
        assert!(restored.exact);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn removing_the_key_then_restoring_returns_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), WITH_OPTIONS);
        let original = fs::read(&path).unwrap();

        let report = apply_launch_options(
            &path,
            "620",
            None,
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();
        assert!(report.changed);
        assert_eq!(report.previous.as_deref(), Some("-novid %command%"));
        assert_eq!(read_launch_options(&path, "620").unwrap(), None);

        restore_launch_options(
            &path,
            "620",
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn writing_the_same_value_does_not_create_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), WITH_OPTIONS);

        let report = apply_launch_options(
            &path,
            "620",
            Some("-novid %command%"),
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();

        assert!(!report.changed);
        assert!(report.backup.is_none());
        assert!(!dir.path().join("backups").exists());
    }

    #[test]
    fn restore_without_a_record_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), EMPTY);

        let error = restore_launch_options(
            &path,
            "620",
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap_err();
        assert!(matches!(error, Error::NoRecord { .. }), "{error}");
        assert!(error.to_string().contains("620"));
    }

    #[test]
    fn a_tampered_backup_is_never_restored() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), WITH_OPTIONS);

        let report = apply_launch_options(
            &path,
            "620",
            Some("shim %command%"),
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();
        fs::write(report.backup.unwrap(), "not the original").unwrap();

        let error = restore_launch_options(
            &path,
            "620",
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap_err();
        assert!(matches!(error, Error::BackupModified { .. }), "{error}");
    }

    #[test]
    fn atomic_write_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), WITH_OPTIONS);

        apply_launch_options(
            &path,
            "620",
            Some("shim %command%"),
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();

        let leftovers: Vec<PathBuf> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(".tmp-"))
            })
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn non_utf8_configs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), "");

        fs::write(&path, [0xff, 0xfe, b'{']).unwrap();
        let error = apply_launch_options(
            &path,
            "620",
            Some("x"),
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap_err();
        assert!(matches!(error, Error::NotUtf8 { .. }), "{error}");
        assert_eq!(fs::read(&path).unwrap(), [0xff, 0xfe, b'{']);
    }

    #[test]
    fn the_journal_is_written_beside_the_backup_and_names_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), EMPTY);
        apply_launch_options(
            &path,
            "620",
            Some("x"),
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();

        let files = backup_files(dir.path());
        assert!(
            files
                .iter()
                .any(|file| file.extension() == Some("json".as_ref()))
        );
        assert!(
            files
                .iter()
                .any(|file| file.extension() == Some("vdf".as_ref()))
        );
        assert!(files.iter().all(|file| {
            file.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("localconfig-123-620")
        }));
    }

    #[test]
    fn a_journal_from_another_steam_root_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let first = fixture(&dir.path().join("one"), WITH_OPTIONS);
        let second = dir
            .path()
            .join("two/Steam/userdata/123/config/localconfig.vdf");
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&second, EMPTY).unwrap();

        apply_launch_options(
            &first,
            "620",
            Some("first %command%"),
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap();

        let error = apply_launch_options(
            &second,
            "620",
            Some("second %command%"),
            SteamProcessState::Stopped,
            &options(dir.path()),
        )
        .unwrap_err();

        assert!(matches!(error, Error::RecordMismatch { .. }), "{error}");
        assert!(error.to_string().contains(&first.display().to_string()));
        assert_eq!(fs::read_to_string(&second).unwrap(), EMPTY);
    }

    #[test]
    fn a_corrupt_journal_blocks_a_write_instead_of_overwriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), EMPTY);
        let options = options(dir.path());
        fs::create_dir_all(&options.backup_dir).unwrap();
        fs::write(journal_path(&options.backup_dir, "123", "620"), "not json").unwrap();

        let error = apply_launch_options(
            &path,
            "620",
            Some("x"),
            SteamProcessState::Stopped,
            &options,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Json(_)), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), EMPTY);
    }

    #[test]
    fn a_missing_backup_blocks_a_repeated_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), WITH_OPTIONS);
        let options = options(dir.path());

        let first = apply_launch_options(
            &path,
            "620",
            Some("first %command%"),
            SteamProcessState::Stopped,
            &options,
        )
        .unwrap();
        fs::remove_file(first.backup.unwrap()).unwrap();
        let after_first = fs::read(&path).unwrap();

        let error = apply_launch_options(
            &path,
            "620",
            Some("second %command%"),
            SteamProcessState::Stopped,
            &options,
        )
        .unwrap_err();
        assert!(matches!(error, Error::BackupModified { .. }), "{error}");
        assert_eq!(fs::read(&path).unwrap(), after_first);
    }

    #[test]
    fn detects_steam_through_a_live_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Steam");
        let proc_root = dir.path().join("proc");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(proc_root.join("4242")).unwrap();
        fs::write(root.join("steam.pid"), "4242\n").unwrap();

        assert_eq!(
            detect_steam_process_in(&root, &proc_root),
            SteamProcessState::Running { pid: 4242 }
        );
    }

    #[test]
    fn a_stale_pid_file_does_not_count_as_running() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Steam");
        let proc_root = dir.path().join("proc");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(proc_root.join("77")).unwrap();
        fs::write(root.join("steam.pid"), "4242\n").unwrap();
        fs::write(proc_root.join("77/comm"), "bash\n").unwrap();

        assert_eq!(
            detect_steam_process_in(&root, &proc_root),
            SteamProcessState::Stopped
        );
    }

    #[test]
    fn detects_steam_by_scanning_proc_comm() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Steam");
        let proc_root = dir.path().join("proc");
        fs::create_dir_all(&root).unwrap();
        for (pid, comm) in [
            ("42", "steamwebhelper\n"),
            ("77", "steam\n"),
            ("78", "bash\n"),
        ] {
            fs::create_dir_all(proc_root.join(pid)).unwrap();
            fs::write(proc_root.join(pid).join("comm"), comm).unwrap();
        }
        fs::create_dir_all(proc_root.join("not-a-pid")).unwrap();

        assert_eq!(
            detect_steam_process_in(&root, &proc_root),
            SteamProcessState::Running { pid: 77 }
        );
    }

    #[test]
    fn user_id_falls_back_when_the_layout_is_unusual() {
        assert_eq!(
            user_id_for(Path::new("/steam/userdata/123/config/localconfig.vdf")),
            "123"
        );
        assert_eq!(user_id_for(Path::new("/tmp/localconfig.vdf")), "default");
        assert_eq!(
            user_id_for(Path::new("/steam/userdata/we ird/config/localconfig.vdf")),
            "we_ird"
        );
    }

    #[test]
    fn sha256_hex_is_lowercase_and_full_length() {
        let hash = sha256_hex(b"abc");
        assert_eq!(hash.len(), 64);
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
