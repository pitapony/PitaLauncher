//! PitaGames' own log directory: list, classify and tail the files under it.
//!
//! The launcher and its shim write to `<XDG_DATA_HOME>/pitagames/logs`:
//! `shim.log` (one launch line per game), the Proton log
//! (`steam-<appid>.log`) and DXVK's per-exe logs. The GUI's Logs screen and the
//! `pitagames-cli logs` diagnostic both render through this module, so they
//! cannot disagree about what is on disk.
//!
//! Everything here is read-only and bounded: [`tail`] reads at most
//! [`TAIL_BYTES`] from the end of a file, so a many-gigabyte game log never
//! blocks the UI. Framework-free and taking explicit paths, like
//! [`crate::proton`] and [`crate::compat`], so it is testable against
//! `tempfile` trees.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How many bytes from the end of a file [`tail`] reads.
pub const TAIL_BYTES: u64 = 256 * 1024;

/// One regular file in the log directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub kind: LogKind,
}

/// What wrote a log, inferred from its file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// `shim.log` — PitaGames' launch lines.
    Shim,
    /// `steam-<appid>.log` — `PROTON_LOG=1` with `PROTON_LOG_DIR`.
    Proton,
    /// `<exe>_d3d11.log` / `<exe>_dxgi.log` — `DXVK_LOG_LEVEL`.
    Dxvk,
    Other,
}

impl LogKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Shim => "shim",
            Self::Proton => "proton",
            Self::Dxvk => "dxvk",
            Self::Other => "log",
        }
    }

    /// Classify a file name. Case-insensitive; anything unrecognised is `Other`.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        if lower == "shim.log" {
            return Self::Shim;
        }
        if lower.starts_with("steam-") && lower.ends_with(".log") {
            return Self::Proton;
        }
        if lower.ends_with("_d3d9.log")
            || lower.ends_with("_d3d10.log")
            || lower.ends_with("_d3d11.log")
            || lower.ends_with("_dxgi.log")
            || lower.ends_with(".dxvk.log")
        {
            return Self::Dxvk;
        }
        Self::Other
    }
}

/// The files in `dir`, newest first, files only.
///
/// A missing or unreadable directory is an empty list, not an error: a machine
/// that has never launched a game has no logs, which is normal.
#[must_use]
pub fn list(dir: &Path) -> Vec<LogFile> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        files.push(LogFile {
            kind: LogKind::from_name(&name),
            name,
            path,
            size: metadata.len(),
            modified: metadata.modified().ok(),
        });
    }

    // Newest first; a name tie-break keeps the order deterministic when the
    // filesystem cannot report a modification time.
    files.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.name.cmp(&right.name))
    });
    files
}

/// A bounded tail of one log file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tail {
    /// The matching lines, oldest first, capped at the requested maximum.
    pub lines: Vec<String>,
    /// Lines in the read window before the `needle` filter.
    pub window_lines: usize,
    /// `true` when the file is larger than the window or more lines matched
    /// than were kept: what is shown is not the whole file.
    pub truncated: bool,
}

impl Tail {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Read the end of `path`, keeping up to `max_lines` lines that contain `needle`.
///
/// Only the last [`TAIL_BYTES`] bytes are read, and a byte-truncated first line
/// is dropped (it may be half a line). `needle` is a case-sensitive substring;
/// `None` keeps every line. A file that does not fit the window, or that has
/// more matching lines than `max_lines`, comes back `truncated`.
pub fn tail(path: &Path, max_lines: usize, needle: Option<&str>) -> std::io::Result<Tail> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    let dropped_bytes = start > 0;

    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::with_capacity((len - start) as usize);
    file.take(TAIL_BYTES).read_to_end(&mut buffer)?;

    let text = String::from_utf8_lossy(&buffer);
    let mut lines: Vec<&str> = text.lines().collect();
    if dropped_bytes && !lines.is_empty() {
        // The first line may be the tail of a line whose beginning was not read.
        lines.remove(0);
    }

    let window_lines = lines.len();
    let matched: Vec<&str> = match needle {
        Some(needle) if !needle.is_empty() => lines
            .into_iter()
            .filter(|line| line.contains(needle))
            .collect(),
        _ => lines,
    };

    let dropped_lines = matched.len() > max_lines;
    let kept = if dropped_lines {
        &matched[matched.len() - max_lines..]
    } else {
        &matched[..]
    };

    Ok(Tail {
        lines: kept.iter().map(|line| (*line).to_string()).collect(),
        window_lines,
        truncated: dropped_bytes || dropped_lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn kinds_are_inferred_from_the_name() {
        assert_eq!(LogKind::from_name("shim.log"), LogKind::Shim);
        assert_eq!(LogKind::from_name("steam-620.log"), LogKind::Proton);
        assert_eq!(LogKind::from_name("Game_d3d11.log"), LogKind::Dxvk);
        assert_eq!(LogKind::from_name("Game_dxgi.log"), LogKind::Dxvk);
        assert_eq!(LogKind::from_name("notes.txt"), LogKind::Other);
        assert_eq!(LogKind::from_name("SHIM.LOG"), LogKind::Shim);
    }

    #[test]
    fn list_reports_files_newest_first_and_skips_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "shim.log", "a\n");
        write(dir.path(), "steam-620.log", "b\n");
        fs::create_dir(dir.path().join("nested")).unwrap();

        let files = list(dir.path());
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|file| file.kind != LogKind::Other));
        assert_eq!(files[0].size, 2);
    }

    #[test]
    fn a_missing_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "shim.log", "one\ntwo\nthree\nfour\n");

        let view = tail(&path, 2, None).unwrap();
        assert_eq!(view.lines, vec!["three", "four"]);
        assert_eq!(view.window_lines, 4);
        assert!(view.truncated, "two of four lines were dropped");

        let whole = tail(&path, 10, None).unwrap();
        assert_eq!(whole.lines.len(), 4);
        assert!(!whole.truncated);
    }

    #[test]
    fn tail_filters_by_needle() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "shim.log",
            "appid=620 ok\nappid=440 fail\nappid=620 again\n",
        );

        let view = tail(&path, 10, Some("appid=620")).unwrap();
        assert_eq!(view.lines.len(), 2);
        assert!(view.lines[0].contains("appid=620 ok"));
        assert!(!view.truncated);

        let none = tail(&path, 10, Some("nothing")).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn tail_drops_the_partial_first_line_of_a_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut contents = String::new();
        // Comfortably over the window, so the read starts mid-file.
        for index in 0..40_000 {
            contents.push_str(&format!("line {index}\n"));
        }
        let path = write(dir.path(), "steam-620.log", &contents);

        let tail = tail(&path, 3, None).unwrap();
        assert!(tail.truncated);
        assert_eq!(tail.lines.len(), 3);
        // The last line is the real end of the file, and no line is a fragment.
        assert_eq!(tail.lines[2], "line 39999");
        assert!(tail.lines.iter().all(|line| line.starts_with("line ")));
    }

    #[test]
    fn tailing_a_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(tail(&dir.path().join("nope.log"), 10, None).is_err());
    }

    #[test]
    fn an_empty_file_has_an_empty_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "shim.log", "");
        let tail = tail(&path, 10, None).unwrap();
        assert!(tail.is_empty());
        assert!(!tail.truncated);
        assert_eq!(tail.window_lines, 0);
    }

    #[test]
    fn a_file_without_a_trailing_newline_keeps_its_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "shim.log", "first\nlast without newline");

        let tail = tail(&path, 10, None).unwrap();
        assert_eq!(tail.lines.last().unwrap(), "last without newline");
    }
}
