//! The `.desktop` entry PitaGames generates for itself.
//!
//! A packaged install ships `/usr/share/applications/pitagames.desktop` (the
//! PKGBUILD task). An uninstalled checkout has no such file, so the launcher
//! writes one into the user's own applications directory on startup:
//!
//! ```text
//! ~/.local/share/applications/pitagames.desktop
//! ```
//!
//! Regenerating it on every launch is what keeps the entry honest: the `Exec`
//! line points at wherever the binary actually lives, so a rebuilt or moved
//! checkout still launches. Writing is idempotent — an identical file is left
//! alone — and the GUI skips it entirely when a packaged entry already exists
//! (a user entry would shadow the packaged one).
//!
//! The rendered text is a contract: `packaging/pitagames.desktop` is checked in
//! and a test asserts it matches [`DesktopEntry::default_for`] byte for byte, so
//! the generated and the packaged entry can never drift.
//!
//! Quoting follows the Desktop Entry specification's two layers: the `Exec`
//! value is first an escaped string (`\\`, `\n`, `\t`, `\r`), then a
//! whitespace-separated argument list in which reserved characters are
//! double-quoted. `%` is doubled inside `Exec` because it introduces field
//! codes. Nothing here ever builds a shell command.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process;

/// The application's file name stem: `pitagames.desktop`, `Icon=pitagames`.
pub const APP_ID: &str = "pitagames";

/// The generated file's name inside `<XDG_DATA_HOME>/applications`.
pub const ENTRY_FILE_NAME: &str = "pitagames.desktop";

/// Desktop Entry specification version written into the file.
pub const DESKTOP_SPEC_VERSION: &str = "1.5";

/// A `[Desktop Entry]` file, rendered from typed fields.
///
/// The field set is deliberately small: it is the launcher's own entry, not a
/// general-purpose desktop-file writer. Unknown fields belong in the packaged
/// file if a desktop environment ever needs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// `Name`, shown by application menus.
    pub name: String,
    /// `GenericName`, a one-word category label.
    pub generic_name: Option<String>,
    /// `Comment`, the tooltip/subtitle line.
    pub comment: String,
    /// `Exec`: the argv of the launcher, rendered with spec quoting.
    pub exec: Vec<String>,
    /// `Icon`: a themed icon name (the package installs `pitagames.svg`).
    pub icon: String,
    /// `Categories`, written with the required trailing semicolons.
    pub categories: Vec<String>,
    /// `Keywords`, for menu search.
    pub keywords: Vec<String>,
    /// `StartupWMClass`, so the window is matched to this entry.
    pub wm_class: Option<String>,
}

impl DesktopEntry {
    /// PitaGames' own entry, launching `exec` (an argv, never a shell word).
    pub fn default_for(exec: Vec<String>) -> Self {
        Self {
            name: "PitaGames".to_string(),
            generic_name: Some("Game launcher".to_string()),
            comment: "Launch and configure your Steam library".to_string(),
            exec,
            icon: APP_ID.to_string(),
            categories: vec!["Game".to_string()],
            keywords: ["game", "launcher", "steam", "proton"]
                .iter()
                .map(|keyword| (*keyword).to_string())
                .collect(),
            wm_class: Some(APP_ID.to_string()),
        }
    }

    /// Validate and render the file contents (always newline-terminated).
    pub fn render(&self) -> Result<String, DesktopEntryError> {
        self.validate()?;

        let mut out = String::new();
        out.push_str("[Desktop Entry]\n");
        out.push_str("Type=Application\n");
        out.push_str("Version=");
        out.push_str(DESKTOP_SPEC_VERSION);
        out.push('\n');

        push_key(&mut out, "Name", &self.name);
        if let Some(generic_name) = self.generic_name.as_deref() {
            push_key(&mut out, "GenericName", generic_name);
        }
        push_key(&mut out, "Comment", &self.comment);

        out.push_str("Exec=");
        out.push_str(&render_exec(&self.exec)?);
        out.push('\n');

        push_key(&mut out, "Icon", &self.icon);
        out.push_str("Terminal=false\n");

        out.push_str("Categories=");
        push_list(&mut out, &self.categories)?;
        out.push('\n');

        out.push_str("Keywords=");
        push_list(&mut out, &self.keywords)?;
        out.push('\n');

        if let Some(wm_class) = self.wm_class.as_deref() {
            push_key(&mut out, "StartupWMClass", wm_class);
        }

        Ok(out)
    }

    /// Whether `path` already holds exactly this entry (a missing file is not a
    /// match).
    pub fn matches(&self, path: impl AsRef<Path>) -> Result<bool, DesktopEntryError> {
        let path = path.as_ref();
        let contents = self.render()?;
        match fs::read(path) {
            Ok(existing) => Ok(existing == contents.as_bytes()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(DesktopEntryError::Io {
                action: "read",
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Write the entry to `path` atomically.
    ///
    /// An identical file is left untouched ([`InstallOutcome::Unchanged`]) so a
    /// launch that changes nothing does not rewrite the user's applications
    /// directory on every start.
    pub fn install(&self, path: impl AsRef<Path>) -> Result<InstallOutcome, DesktopEntryError> {
        let path = path.as_ref();
        let contents = self.render()?;

        match fs::read(path) {
            Ok(existing) if existing == contents.as_bytes() => {
                return Ok(InstallOutcome::Unchanged);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(DesktopEntryError::Io {
                    action: "read",
                    path: path.to_path_buf(),
                    source,
                });
            }
        }

        atomic_write(path, contents.as_bytes())?;
        Ok(InstallOutcome::Installed)
    }

    /// Remove the entry. `Ok(false)` means it was already gone.
    pub fn remove(path: impl AsRef<Path>) -> Result<bool, DesktopEntryError> {
        let path = path.as_ref();
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(DesktopEntryError::Io {
                action: "remove",
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    fn validate(&self) -> Result<(), DesktopEntryError> {
        for (field, value) in [
            ("Name", self.name.as_str()),
            ("Comment", self.comment.as_str()),
            ("Icon", self.icon.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(DesktopEntryError::Empty { field });
            }
            if value.contains(['\n', '\r', '\0']) {
                return Err(DesktopEntryError::Multiline { field });
            }
        }

        if self.exec.is_empty() {
            return Err(DesktopEntryError::Empty { field: "Exec" });
        }
        if self.exec[0].is_empty() {
            return Err(DesktopEntryError::Empty {
                field: "Exec (program)",
            });
        }

        Ok(())
    }
}

/// Whether a packaged entry already exists in one of the XDG data directories
/// (`/usr/share`, `/usr/local/share`, …).
///
/// A user-level entry shadows a packaged one, so the launcher only generates its
/// own when the package did not install one.
pub fn packaged_entry(data_dirs: &[PathBuf]) -> bool {
    data_dirs
        .iter()
        .any(|dir| dir.join("applications").join(ENTRY_FILE_NAME).is_file())
}

/// What [`DesktopEntry::install`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The file was written (new or replaced).
    Installed,
    /// The file already contained exactly this entry.
    Unchanged,
}

impl InstallOutcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Unchanged => "unchanged",
        }
    }
}

/// `Key=escaped value` plus a newline.
fn push_key(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('=');
    out.push_str(&escape_string(value));
    out.push('\n');
}

/// A `;`-terminated list value (the spec requires the trailing semicolon).
fn push_list(out: &mut String, values: &[String]) -> Result<(), DesktopEntryError> {
    for value in values {
        if value.contains(['\n', '\r', '\0', ';']) {
            return Err(DesktopEntryError::Unrepresentable {
                field: "list value",
            });
        }
        out.push_str(&escape_string(value));
        out.push(';');
    }
    Ok(())
}

/// The generic "string" escaping applied to every value before anything else.
pub(crate) fn escape_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// Render an argv as an `Exec` value.
pub(crate) fn render_exec(argv: &[String]) -> Result<String, DesktopEntryError> {
    let mut rendered = Vec::with_capacity(argv.len());
    for argument in argv {
        if argument.contains(['\n', '\r', '\0']) {
            return Err(DesktopEntryError::Unrepresentable { field: "Exec" });
        }
        rendered.push(escape_string(&quote_argument(argument)));
    }
    Ok(rendered.join(" "))
}

/// Quote one argument the way `Exec` requires.
///
/// An argument with no reserved characters is written bare (that is what hand
/// written desktop files look like and what makes `Exec=pitagames` legible).
/// Anything else is double-quoted with `\`, `"`, `` ` `` and `$` escaped, and
/// `%` doubled because the spec reserves it for field codes.
fn quote_argument(argument: &str) -> String {
    const RESERVED: [char; 19] = [
        ' ', '\t', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')',
        '`', '%',
    ];

    if !argument.is_empty() && !argument.contains(RESERVED) {
        return argument.to_string();
    }

    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    for character in argument.chars() {
        match character {
            '\\' | '"' | '`' | '$' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// Write `bytes` through a sibling temp file so a reader never sees half a file.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), DesktopEntryError> {
    let directory = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&directory).map_err(|source| DesktopEntryError::Io {
        action: "create parent of",
        path: path.to_path_buf(),
        source,
    })?;

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| ENTRY_FILE_NAME.to_string());
    let temp = directory.join(format!(".{name}.tmp-{}", process::id()));

    let result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        if let Ok(dir) = fs::File::open(&directory) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();

    if let Err(source) = result {
        let _ = fs::remove_file(&temp);
        return Err(DesktopEntryError::Io {
            action: "write",
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(())
}

/// Desktop-entry failures.
#[derive(Debug, thiserror::Error)]
pub enum DesktopEntryError {
    /// A required field was blank.
    #[error("{field} must not be empty")]
    Empty { field: &'static str },

    /// A field that must stay on one line contains a line break.
    #[error("{field} must be a single line")]
    Multiline { field: &'static str },

    /// A value the desktop-entry format cannot represent (newline, NUL, or a
    /// semicolon inside a list item).
    #[error("{field} contains a character a desktop entry cannot represent")]
    Unrepresentable { field: &'static str },

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

    fn entry() -> DesktopEntry {
        DesktopEntry::default_for(vec!["pitagames".to_string()])
    }

    /// The packaged entry and the generated one are the same file; the
    /// PKGBUILD installs the checked-in copy, uninstalled runs regenerate it.
    #[test]
    fn the_packaged_entry_matches_the_renderer() {
        let packaged = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/pitagames.desktop"
        ));
        assert_eq!(entry().render().unwrap(), packaged);
    }

    #[test]
    fn the_default_entry_is_a_launchable_application() {
        let rendered = entry().render().unwrap();
        assert!(rendered.starts_with("[Desktop Entry]\nType=Application\n"));
        assert!(rendered.contains("Name=PitaGames\n"));
        assert!(rendered.contains("Exec=pitagames\n"));
        assert!(rendered.contains("Icon=pitagames\n"));
        assert!(rendered.contains("Categories=Game;\n"));
        assert!(rendered.contains("Keywords=game;launcher;steam;proton;\n"));
        assert!(rendered.contains("StartupWMClass=pitagames\n"));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn an_absolute_exec_path_needs_no_quotes() {
        let entry = DesktopEntry::default_for(vec![
            "/home/tester/code/pitagames/target/debug/pitagames".to_string(),
        ]);
        let rendered = entry.render().unwrap();
        assert!(
            rendered.contains("Exec=/home/tester/code/pitagames/target/debug/pitagames\n"),
            "{rendered}"
        );
    }

    #[test]
    fn spaces_quotes_and_percent_signs_are_escaped() {
        let entry = DesktopEntry::default_for(vec![
            "/home/two words/pitagames".to_string(),
            "--title".to_string(),
            "say \"hi\" $HOME".to_string(),
            "100%".to_string(),
        ]);
        let rendered = entry.render().unwrap();
        // Exec quoting, then the string layer doubles the backslashes.
        assert!(
            rendered.contains(
                r#"Exec="/home/two words/pitagames" --title "say \\"hi\\" \\$HOME" "100%%""#
            ),
            "{rendered}"
        );
    }

    #[test]
    fn an_empty_program_is_refused() {
        let entry = DesktopEntry::default_for(vec![String::new()]);
        assert!(matches!(
            entry.render().unwrap_err(),
            DesktopEntryError::Empty { .. }
        ));

        let entry = DesktopEntry::default_for(Vec::new());
        assert!(matches!(
            entry.render().unwrap_err(),
            DesktopEntryError::Empty { field: "Exec" }
        ));
    }

    #[test]
    fn a_multiline_name_is_refused_rather_than_truncated() {
        let mut entry = entry();
        entry.name = "Pita\nGames".to_string();
        assert!(matches!(
            entry.render().unwrap_err(),
            DesktopEntryError::Multiline { field: "Name" }
        ));
    }

    #[test]
    fn an_argument_with_a_line_break_is_refused() {
        let entry = DesktopEntry::default_for(vec!["pitagames\n--evil".to_string()]);
        assert!(matches!(
            entry.render().unwrap_err(),
            DesktopEntryError::Unrepresentable { .. }
        ));
    }

    #[test]
    fn installing_writes_once_and_then_reports_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("applications").join(ENTRY_FILE_NAME);
        let entry = entry();

        assert_eq!(entry.install(&path).unwrap(), InstallOutcome::Installed);
        assert!(path.is_file());
        assert_eq!(fs::read_to_string(&path).unwrap(), entry.render().unwrap());

        assert_eq!(entry.install(&path).unwrap(), InstallOutcome::Unchanged);
    }

    #[test]
    fn a_changed_exec_path_replaces_the_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ENTRY_FILE_NAME);

        DesktopEntry::default_for(vec!["/old/pitagames".to_string()])
            .install(&path)
            .unwrap();
        DesktopEntry::default_for(vec!["/new/pitagames".to_string()])
            .install(&path)
            .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("Exec=/new/pitagames\n"), "{contents}");
    }

    #[test]
    fn matches_reports_missing_and_different_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ENTRY_FILE_NAME);
        let entry = entry();

        assert!(!entry.matches(&path).unwrap());
        entry.install(&path).unwrap();
        assert!(entry.matches(&path).unwrap());

        fs::write(&path, "[Desktop Entry]\nName=Other\n").unwrap();
        assert!(!entry.matches(&path).unwrap());
    }

    #[test]
    fn removing_reports_whether_the_file_existed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ENTRY_FILE_NAME);

        assert!(!DesktopEntry::remove(&path).unwrap());
        entry().install(&path).unwrap();
        assert!(DesktopEntry::remove(&path).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn a_write_leaves_no_temp_files_behind() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ENTRY_FILE_NAME);
        entry().install(&path).unwrap();

        let names: Vec<String> = fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [ENTRY_FILE_NAME]);
    }

    #[test]
    fn a_packaged_entry_is_found_in_any_data_directory() {
        let temp = tempfile::tempdir().unwrap();
        let empty = temp.path().join("empty");
        let shipped = temp.path().join("share");
        fs::create_dir_all(shipped.join("applications")).unwrap();

        assert!(!packaged_entry(&[empty.clone(), shipped.clone()]));
        fs::write(shipped.join("applications").join(ENTRY_FILE_NAME), "x").unwrap();
        assert!(packaged_entry(&[empty, shipped]));
    }
}
