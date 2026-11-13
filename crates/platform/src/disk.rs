//! Filesystem space: free bytes, bounded directory size, and the pre-flight
//! check an install runs before it starts.
//!
//! An install that runs out of space halfway through leaves a broken game
//! directory behind, and on a machine whose library shares a disk with `/` a
//! full filesystem is also a system-wide problem. So the launcher asks two
//! questions before handing a game to a store CLI: how much room is left on the
//! filesystem that will hold it ([`space`] / [`check`]), and how much room the
//! thing we are about to replace already takes ([`dir_size`]).
//!
//! All of it is read-only and bounded. [`space`] talks to `statvfs` (no shell,
//! no parsing `df`), and [`dir_size`] walks a tree with entry and depth caps, so
//! asking "how big is this prefix?" can never turn a 200 GB game directory into
//! a hang. Framework-free and taking explicit paths, like [`crate::logs`] and
//! [`crate::proton`].

use std::ffi::CString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use serde::Serialize;

/// Default number of entries [`dir_size`] visits before it gives up.
pub const MAX_SIZE_ENTRIES: u64 = 200_000;

/// Default directory depth [`dir_size`] descends to.
pub const MAX_SIZE_DEPTH: usize = 32;

/// Bytes on the filesystem that holds a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Space {
    /// Total capacity of the filesystem.
    pub total: u64,
    /// Free bytes, including the blocks reserved for root.
    pub free: u64,
    /// Bytes a normal user may actually write. This is the number an install
    /// check must use: on a machine with a root reservation `free` overstates
    /// the room the launcher has.
    pub available: u64,
}

impl Space {
    /// The fraction of the filesystem in use, `0.0..=1.0`.
    ///
    /// `1.0` when the total is unknown (a statvfs with no blocks), so a caller
    /// showing "full" is never told there is plenty of room by a zero-size
    /// filesystem.
    pub fn used_fraction(self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        let used = self.total.saturating_sub(self.available);
        used as f32 / self.total as f32
    }
}

/// Free space on the filesystem that contains `path`.
///
/// An install directory does not exist yet when the check runs, so a missing
/// path walks up to its nearest existing ancestor — the filesystem the install
/// *would* land on. Only a path with no existing ancestor (or one containing a
/// NUL byte) is an error.
pub fn space(path: &Path) -> io::Result<Space> {
    let mut probe = path;
    loop {
        match statvfs(probe) {
            Ok(space) => return Ok(space),
            Err(error) if error.kind() == io::ErrorKind::NotFound => match probe.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => probe = parent,
                _ => return Err(error),
            },
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
// `fsblkcnt_t` (the block counts) is `u32` on Darwin and `u64` on Linux, while
// `f_frsize`/`f_bsize` (the block size) are `c_ulong` on both. The casts to a
// common `u64` are therefore needed on one platform and redundant on the other.
#[allow(clippy::unnecessary_cast)]
fn statvfs(path: &Path) -> io::Result<Space> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;

    // SAFETY: `c_path` is a valid NUL-terminated C string and `buf` points at a
    // correctly sized, writable `statvfs`.
    let mut buf = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(c_path.as_ptr(), buf.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let buf = unsafe { buf.assume_init() };

    // `f_frsize` is the fragment size the block counts are expressed in; a
    // filesystem that reports 0 falls back to the I/O block size.
    let unit = if buf.f_frsize == 0 {
        buf.f_bsize
    } else {
        buf.f_frsize
    } as u64;

    Ok(Space {
        total: (buf.f_blocks as u64).saturating_mul(unit),
        free: (buf.f_bfree as u64).saturating_mul(unit),
        available: (buf.f_bavail as u64).saturating_mul(unit),
    })
}

#[cfg(not(unix))]
fn statvfs(_path: &Path) -> io::Result<Space> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem space is only implemented on Unix",
    ))
}

/// Bounds for one [`dir_size`] walk; [`dir_size`] uses the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeLimits {
    pub max_entries: u64,
    pub max_depth: usize,
}

impl Default for SizeLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_SIZE_ENTRIES,
            max_depth: MAX_SIZE_DEPTH,
        }
    }
}

/// How much a directory tree holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct DirSize {
    /// Sum of the regular files' lengths.
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    /// The walk stopped at a limit before it finished; `bytes` is a lower bound.
    pub truncated: bool,
    /// The root existed and was walked. `false` means "not there", which is
    /// different from "there and empty".
    pub scanned: bool,
}

impl DirSize {
    /// A one-line human summary, e.g. `1.2 GiB in 340 files`.
    pub fn summary(self) -> String {
        let size = human_bytes(self.bytes);
        if !self.scanned {
            return "not present".to_string();
        }
        let files = self.files;
        let suffix = if self.truncated { " (partial)" } else { "" };
        format!("{size} in {files} file(s){suffix}")
    }
}

/// Walk a directory and total the regular files under it.
///
/// A missing directory is an empty, `scanned = false` result rather than an
/// error; a symlink is neither followed nor counted, so a self-referential tree
/// cannot loop and a file reached through two names is not measured twice.
pub fn dir_size(path: &Path) -> DirSize {
    dir_size_limited(path, SizeLimits::default())
}

/// [`dir_size`] with explicit walk bounds, for tests and callers that know the
/// tree is huge.
pub fn dir_size_limited(root: &Path, limits: SizeLimits) -> DirSize {
    let mut size = DirSize::default();
    if !root.is_dir() {
        return size;
    }
    size.scanned = true;

    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut visited = 0u64;

    'walk: while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > limits.max_entries {
                size.truncated = true;
                break 'walk;
            }

            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                size.dirs += 1;
                if depth < limits.max_depth {
                    stack.push((entry.path(), depth + 1));
                } else {
                    size.truncated = true;
                }
            } else if file_type.is_file()
                && let Ok(metadata) = entry.metadata()
            {
                size.files += 1;
                size.bytes = size.bytes.saturating_add(metadata.len());
            }
        }
    }

    size
}

/// The verdict of a pre-install space check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SpaceCheck {
    /// Bytes the install is expected to need.
    pub required: u64,
    /// Bytes the filesystem can give a normal user.
    pub available: u64,
}

impl SpaceCheck {
    /// Whether the filesystem has room for the install.
    pub const fn ok(self) -> bool {
        self.available >= self.required
    }

    /// How many bytes short the filesystem is (`0` when it is not).
    pub const fn shortfall(self) -> u64 {
        self.required.saturating_sub(self.available)
    }
}

/// Ask whether the filesystem that holds `path` has `required` bytes for a user.
pub fn check(path: &Path, required: u64) -> io::Result<SpaceCheck> {
    Ok(SpaceCheck {
        required,
        available: space(path)?.available,
    })
}

/// A compact base-1024 byte count (`1.2 GiB`), matching the store CLIs' style.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: usize) {
        fs::write(path, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn a_tempdir_reports_positive_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let space = space(dir.path()).unwrap();

        assert!(space.total > 0, "{space:?}");
        assert!(space.available <= space.free);
        assert!(space.available <= space.total);
        assert!((0.0..=1.0).contains(&space.used_fraction()));
    }

    #[test]
    fn a_path_that_does_not_exist_resolves_its_nearest_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-yet/created");

        // An install directory does not exist when its check runs, so `space`
        // walks up to the filesystem the path would land on.
        let space = space(&missing).unwrap();
        assert!(space.total > 0);
    }

    #[test]
    fn space_on_a_path_with_no_existing_ancestor_is_an_error() {
        // An empty path has no parent to fall back to, and a NUL byte cannot
        // reach `statvfs` at all.
        assert!(space(Path::new("")).is_err());
        assert!(space(Path::new("nul\0byte")).is_err());
    }

    #[test]
    fn check_compares_against_available() {
        let dir = tempfile::tempdir().unwrap();
        let plenty = check(dir.path(), 1).unwrap();
        assert!(plenty.ok());
        assert_eq!(plenty.shortfall(), 0);

        let impossible = check(dir.path(), u64::MAX).unwrap();
        assert!(!impossible.ok());
        assert!(impossible.shortfall() > 0);
    }

    #[test]
    fn dir_size_totals_files_and_directories() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("a.bin"), 100);
        let nested = root.path().join("sub");
        fs::create_dir(&nested).unwrap();
        write(&nested.join("b.bin"), 250);

        let size = dir_size(root.path());
        assert_eq!(size.bytes, 350);
        assert_eq!(size.files, 2);
        assert_eq!(size.dirs, 1);
        assert!(size.scanned && !size.truncated);
        assert_eq!(size.summary(), "350 B in 2 file(s)");
    }

    #[test]
    fn dir_size_of_a_missing_directory_is_not_scanned() {
        let root = tempfile::tempdir().unwrap();
        let size = dir_size(&root.path().join("nope"));
        assert!(!size.scanned);
        assert_eq!(size.bytes, 0);
        assert_eq!(size.summary(), "not present");
    }

    #[test]
    fn dir_size_does_not_follow_symlinks() {
        #[cfg(unix)]
        {
            let root = tempfile::tempdir().unwrap();
            write(&root.path().join("real.bin"), 64);
            std::os::unix::fs::symlink(root.path(), root.path().join("loop")).unwrap();

            let size = dir_size(root.path());
            assert_eq!(size.files, 1, "the symlink must not be counted or walked");
            assert_eq!(size.bytes, 64);
        }
    }

    #[test]
    fn dir_size_respects_the_entry_cap() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..10 {
            write(&root.path().join(format!("f{index}.bin")), 1);
        }

        let size = dir_size_limited(
            root.path(),
            SizeLimits {
                max_entries: 3,
                max_depth: 4,
            },
        );
        assert!(size.truncated);
        assert!(size.files <= 3, "{size:?}");
    }

    #[test]
    fn dir_size_stops_at_the_depth_cap() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();
        write(&deep.join("leaf.bin"), 10);

        let size = dir_size_limited(
            root.path(),
            SizeLimits {
                max_entries: 100,
                max_depth: 1,
            },
        );
        assert!(size.truncated);
        assert_eq!(size.bytes, 0, "the file is below the depth cap");
    }

    #[test]
    fn human_bytes_uses_base_1024() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }
}
