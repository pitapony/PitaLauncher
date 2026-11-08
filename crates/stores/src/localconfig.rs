//! Format-preserving edits of `localconfig.vdf`.
//!
//! Steam keeps per-app launch options in
//! `<steam>/userdata/<id>/config/localconfig.vdf`:
//!
//! ```text
//! "UserLocalConfigStore" {
//!   "Software" { "Valve" { "Steam" { "apps" {
//!     "620" { "LaunchOptions" "-novid %command%" }
//!   } } } }
//! }
//! ```
//!
//! The file is Steam's, so a writer must not re-serialize it: Steam adds keys
//! constantly, users edit it by hand, and the plan requires a byte-identical
//! restore. [`edit`] therefore splices the single value (or the single
//! `"LaunchOptions"` pair) into the original bytes and leaves every other byte
//! untouched, using the shared span machinery in [`crate::vdf_edit`].
//! `crate::steam_config` adds the Steam-closed guard, the backup and the atomic
//! replace around it.
//!
//! Every edit is re-parsed with [`crate::vdf::parse`] before it is returned.

use crate::vdf_edit;

/// The key Steam uses for a game's launch options.
pub const LAUNCH_OPTIONS: &str = "LaunchOptions";

/// Where per-app launch options live inside `localconfig.vdf`.
pub const APP_OBJECT_PATH: [&str; 5] =
    ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"];

/// The result of a format-preserving edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The complete new document.
    pub text: String,
    /// The value before the edit; `None` means the key was absent.
    pub previous: Option<String>,
    /// `false` when the document would not change.
    pub changed: bool,
}

/// Why a `localconfig.vdf` edit was refused.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Steam appids are numeric; `{0}` is not")]
    BadAppId(String),

    #[error(transparent)]
    Vdf(#[from] crate::vdf::ParseError),

    #[error("localconfig.vdf has no `{0}` object; refusing to create Steam's layout")]
    MissingSection(String),

    #[error("`{app_id}` has a non-string `LaunchOptions` value")]
    NotAString { app_id: String },

    #[error("failed to scan the document: {0}")]
    Scanned(String),

    #[error("edit produced invalid VDF: {0}")]
    InvalidResult(String),
}

impl From<vdf_edit::Error> for Error {
    fn from(error: vdf_edit::Error) -> Self {
        Self::Scanned(error.to_string())
    }
}

/// Read one app's launch options without changing anything.
///
/// A missing `apps` section, missing app or missing key all yield `Ok(None)`:
/// only [`edit`] insists on the expected structure, because only a writer can
/// corrupt the file.
pub fn read(input: &str, app_id: &str) -> Result<Option<String>, Error> {
    validate_app_id(app_id)?;
    crate::vdf::parse(input)?;
    let root = vdf_edit::document(input)?;
    Ok(vdf_edit::navigate(&root, &APP_OBJECT_PATH)
        .and_then(|apps| apps.get_object(app_id))
        .and_then(|app| app.get_string(LAUNCH_OPTIONS))
        .map(str::to_string))
}

/// Set or remove one app's launch options.
///
/// `Some(value)` creates the key (and the app object) when needed; `None`
/// removes it. The returned document is byte-identical to the input outside the
/// edited pair, and is always valid VDF.
pub fn edit(input: &str, app_id: &str, value: Option<&str>) -> Result<Edit, Error> {
    validate_app_id(app_id)?;

    // A structurally broken file is never edited. The span scanner below
    // assumes the grammar; this check is what makes that assumption safe.
    crate::vdf::parse(input)?;
    let root = vdf_edit::document(input)?;

    let apps = vdf_edit::navigate(&root, &APP_OBJECT_PATH)
        .ok_or_else(|| Error::MissingSection(APP_OBJECT_PATH.join("/")))?;
    let app = apps.get_object(app_id);
    let previous = app
        .and_then(|app| app.get_string(LAUNCH_OPTIONS))
        .map(str::to_string);

    let text = match (value, app) {
        (Some(value), Some(app)) => match app.get(LAUNCH_OPTIONS) {
            Some(pair) => {
                let Some(span) = pair.value.string_span() else {
                    return Err(Error::NotAString {
                        app_id: app_id.to_string(),
                    });
                };
                vdf_edit::splice(input, span.start, span.end, &vdf_edit::quote(value))
            }
            None => vdf_edit::insert_pair(input, app, LAUNCH_OPTIONS, value),
        },
        (Some(value), None) => {
            vdf_edit::insert_object(input, apps, app_id, &[(LAUNCH_OPTIONS, value)])
        }
        (None, Some(app)) => match app.get(LAUNCH_OPTIONS) {
            Some(pair) => {
                // An app object that only ever held the launch options goes away
                // with them, so a set-then-remove round trip restores the
                // original layout instead of leaving `"620" { }` behind.
                match apps.get(app_id).filter(|_| app.pairs.len() == 1) {
                    Some(app_pair) => vdf_edit::remove_pair(input, app_pair),
                    None => vdf_edit::remove_pair(input, pair),
                }
            }
            None => input.to_string(),
        },
        (None, None) => input.to_string(),
    };

    if text != input {
        // Never hand a corrupt document to the atomic writer.
        crate::vdf::parse(&text).map_err(|error| Error::InvalidResult(error.to_string()))?;
    }

    Ok(Edit {
        changed: text != input,
        text,
        previous,
    })
}

fn validate_app_id(app_id: &str) -> Result<(), Error> {
    if app_id.is_empty() || !app_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::BadAppId(app_id.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = include_str!("../../../fixtures/steam/localconfig_empty.vdf");
    const WITH_OPTIONS: &str = include_str!("../../../fixtures/steam/localconfig_with_options.vdf");
    const QUOTING: &str = include_str!("../../../fixtures/steam/localconfig_quoting.vdf");

    fn options(text: &str, app_id: &str) -> Option<String> {
        read(text, app_id).unwrap()
    }

    #[test]
    fn reads_existing_launch_options() {
        assert_eq!(
            options(WITH_OPTIONS, "620").as_deref(),
            Some("-novid %command%")
        );
        assert_eq!(options(WITH_OPTIONS, "440"), None);
        assert_eq!(options(EMPTY, "620"), None);
    }

    #[test]
    fn decodes_quoted_launch_options() {
        assert_eq!(
            options(QUOTING, "620").as_deref(),
            Some(r#"WINEDLLOVERRIDES="dxgi=n,b" %command% -novid"#)
        );
        assert_eq!(
            options(QUOTING, "440").as_deref(),
            Some(r#""C:\Games\Game\game.exe" --flag"#)
        );
    }

    #[test]
    fn setting_a_missing_key_reflows_only_the_app_object() {
        let edit = edit(EMPTY, "620", Some("pitagames-shim 620 %command%")).unwrap();
        assert!(edit.changed);
        assert_eq!(edit.previous, None);
        assert_eq!(
            options(&edit.text, "620").as_deref(),
            Some("pitagames-shim 620 %command%")
        );
        // The section scaffolding is created exactly once, before the close.
        assert!(edit.text.contains(
            "\t\t\t\t\t\"620\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"pitagames-shim 620 %command%\"\n\t\t\t\t\t}\n\t\t\t\t}"
        ));
        // The rest of the document is untouched.
        assert!(edit.text.contains("\"friends\"\n\t{\n\t}"));
    }

    #[test]
    fn replacing_a_value_keeps_every_other_byte() {
        let edit = edit(WITH_OPTIONS, "620", Some("DXVK_HUD=fps %command%")).unwrap();
        assert!(edit.changed);
        assert_eq!(edit.previous.as_deref(), Some("-novid %command%"));

        let expected = WITH_OPTIONS.replacen("-novid %command%", "DXVK_HUD=fps %command%", 1);
        assert_eq!(edit.text, expected);
    }

    #[test]
    fn removing_a_key_restores_the_fixture_byte_for_byte() {
        let removed = edit(WITH_OPTIONS, "620", None).unwrap();
        assert!(removed.changed);
        assert_eq!(removed.previous.as_deref(), Some("-novid %command%"));
        assert_eq!(options(&removed.text, "620"), None);
        assert!(!removed.text.contains("-novid"));

        // Removing a line that the writer inserted returns the empty fixture.
        let inserted = edit(EMPTY, "620", Some("x %command%")).unwrap();
        let removed_again = edit(&inserted.text, "620", None).unwrap();
        assert_eq!(removed_again.text, EMPTY);
    }

    #[test]
    fn setting_the_same_value_is_a_no_op() {
        let edit = edit(WITH_OPTIONS, "620", Some("-novid %command%")).unwrap();
        assert!(!edit.changed);
        assert_eq!(edit.text, WITH_OPTIONS);
    }

    #[test]
    fn removing_an_absent_key_is_a_no_op() {
        let edit = edit(WITH_OPTIONS, "440", None).unwrap();
        assert!(!edit.changed);
        assert_eq!(edit.text, WITH_OPTIONS);
        assert_eq!(edit.previous, None);
    }

    #[test]
    fn values_with_quotes_and_backslashes_round_trip() {
        let value = r#"WINEDLLOVERRIDES="dxgi=n,b" %command% "C:\Games\Game" --x"#;
        let edit = edit(EMPTY, "620", Some(value)).unwrap();
        assert_eq!(options(&edit.text, "620").as_deref(), Some(value));
        // The escaped form is valid VDF, not just readable by our scanner.
        crate::vdf::parse(&edit.text).unwrap();
    }

    #[test]
    fn an_inline_empty_apps_object_is_supported() {
        let input = "\"UserLocalConfigStore\" { \"Software\" { \"Valve\" { \"Steam\" { \"apps\" { } } } } }\n";
        let edit = edit(input, "620", Some("%command%")).unwrap();
        assert_eq!(options(&edit.text, "620").as_deref(), Some("%command%"));
        crate::vdf::parse(&edit.text).unwrap();
    }

    #[test]
    fn an_inline_app_object_is_supported() {
        let input = "\"UserLocalConfigStore\" { \"Software\" { \"Valve\" { \"Steam\" { \"apps\" { \"620\" { } } } } } }\n";
        let edit = edit(input, "620", Some("%command%")).unwrap();
        assert_eq!(options(&edit.text, "620").as_deref(), Some("%command%"));
        crate::vdf::parse(&edit.text).unwrap();
    }

    #[test]
    fn an_unexpected_layout_is_refused_not_invented() {
        let error = edit("\"localconfig\" { \"apps\" { } }\n", "620", Some("x")).unwrap_err();
        assert!(matches!(error, Error::MissingSection(_)), "{error}");
        assert!(error.to_string().contains("UserLocalConfigStore"));
    }

    #[test]
    fn non_numeric_appids_are_refused() {
        assert!(matches!(
            edit(EMPTY, "portal", None),
            Err(Error::BadAppId(_))
        ));
        assert!(matches!(read(EMPTY, ""), Err(Error::BadAppId(_))));
    }

    #[test]
    fn broken_files_are_never_edited() {
        let error = edit("\"UserLocalConfigStore\" {", "620", Some("x")).unwrap_err();
        assert!(matches!(error, Error::Vdf(_)), "{error}");
    }

    #[test]
    fn comments_and_conditionals_survive_an_edit() {
        let input = r#"
// Steam's own header comment
"UserLocalConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"apps"
				{
					"620"
					{
						// keep me
						"LastPlayed"		"1710000000" [$WIN32]
					}
				}
			}
		}
	}
}
"#;
        let edit = edit(input, "620", Some("%command%")).unwrap();
        assert!(edit.text.contains("// keep me"));
        assert!(edit.text.contains("[$WIN32]"));
        assert_eq!(options(&edit.text, "620").as_deref(), Some("%command%"));
    }
}
