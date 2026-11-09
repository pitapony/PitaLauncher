//! Format-preserving edits of `config.vdf`'s `CompatToolMapping`.
//!
//! Steam keeps the "force a specific Steam Play compatibility tool" choice per
//! game in `<steam>/config/config.vdf`:
//!
//! ```text
//! "InstallConfigStore" {
//!   "Software" { "Valve" { "Steam" { "CompatToolMapping" {
//!     "620" {
//!       "name"     "proton_9"
//!       "config"   ""
//!       "Priority" "250"
//!     }
//!   } } } }
//! }
//! ```
//!
//! Unlike `localconfig.vdf` this file has no Steam user; it belongs to the
//! whole install. The same rules apply: it is Steam's file, so [`edit`] splices
//! only the mapping's pairs into the original bytes through
//! [`crate::vdf_edit`] and `crate::steam_config` wraps it with the Steam-closed
//! guard, the backup and the atomic replace. Every edit is re-parsed with
//! [`crate::vdf::parse`] before it is returned.

use serde::{Deserialize, Serialize};

use crate::vdf_edit;

/// Where per-game compatibility-tool mappings live inside `config.vdf`.
pub const COMPAT_TOOL_MAPPING_PATH: [&str; 5] = [
    "InstallConfigStore",
    "Software",
    "Valve",
    "Steam",
    "CompatToolMapping",
];

/// The key holding the compatibility tool's Steam name.
pub const NAME: &str = "name";
/// The (usually empty) per-tool configuration blob Steam writes.
pub const CONFIG: &str = "config";
/// The mapping priority Steam writes for a per-game override.
pub const PRIORITY: &str = "Priority";

/// The priority Steam writes for a per-game override.
pub const DEFAULT_PRIORITY: i32 = 250;

/// One `CompatToolMapping` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatTool {
    /// The tool name Steam matches, e.g. `proton_9`, `GE-Proton9-20`,
    /// `proton-cachyos`.
    pub name: String,
    /// Steam's per-tool config string; normally empty for per-game overrides.
    #[serde(default)]
    pub config: String,
    #[serde(default = "default_priority")]
    pub priority: i32,
}

fn default_priority() -> i32 {
    DEFAULT_PRIORITY
}

impl CompatTool {
    /// A mapping with Steam's default priority and an empty config.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            config: String::new(),
            priority: DEFAULT_PRIORITY,
        }
    }

    /// The three pairs this mapping writes, in Steam's own order.
    fn pairs(&self) -> [(&'static str, String); 3] {
        [
            (NAME, self.name.clone()),
            (CONFIG, self.config.clone()),
            (PRIORITY, self.priority.to_string()),
        ]
    }
}

/// The result of a format-preserving edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The complete new document.
    pub text: String,
    /// The mapping before the edit; `None` means the app had none.
    pub previous: Option<CompatTool>,
    /// `false` when the document would not change.
    pub changed: bool,
}

/// Why a `config.vdf` edit was refused.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Steam appids are numeric; `{0}` is not")]
    BadAppId(String),

    #[error(transparent)]
    Vdf(#[from] crate::vdf::ParseError),

    #[error("config.vdf has no `{0}` object; refusing to create Steam's layout")]
    MissingSection(String),

    #[error("a compatibility tool name must not be empty")]
    EmptyName,

    #[error("`{app_id}` has a non-string `{key}` value in CompatToolMapping")]
    NotAString { app_id: String, key: String },

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

/// Read one app's compatibility-tool mapping without changing anything.
///
/// A missing section, missing app or a mapping without a `name` all yield
/// `Ok(None)`. Only [`edit`] insists on the expected structure, because only a
/// writer can corrupt the file.
pub fn read(input: &str, app_id: &str) -> Result<Option<CompatTool>, Error> {
    validate_app_id(app_id)?;
    crate::vdf::parse(input)?;
    let root = vdf_edit::document(input)?;
    Ok(vdf_edit::navigate(&root, &COMPAT_TOOL_MAPPING_PATH)
        .and_then(|mapping| mapping.get_object(app_id))
        .and_then(read_tool))
}

/// Set (`Some`) or remove (`None`) one app's compatibility-tool mapping.
///
/// `Some(tool)` replaces or creates the mapping; `None` removes it. The result
/// is byte-identical to the input outside the edited pairs and is always valid
/// VDF. A lead mapping created by us and then cleared returns the original
/// layout; a mapping that already existed loses exactly the compat pairs.
pub fn edit(input: &str, app_id: &str, tool: Option<&CompatTool>) -> Result<Edit, Error> {
    validate_app_id(app_id)?;

    // A structurally broken file is never edited. The span scanner assumes the
    // grammar; this check is what makes that assumption safe.
    crate::vdf::parse(input)?;
    let root = vdf_edit::document(input)?;

    let mapping = vdf_edit::navigate(&root, &COMPAT_TOOL_MAPPING_PATH)
        .ok_or_else(|| Error::MissingSection(COMPAT_TOOL_MAPPING_PATH.join("/")))?;
    let app = mapping.get_object(app_id);
    let previous = app.and_then(read_tool);

    let text = match tool {
        Some(tool) => {
            if tool.name.trim().is_empty() {
                return Err(Error::EmptyName);
            }
            set_tool(input, app, mapping, app_id, tool)?
        }
        None => remove_tool(input, app, mapping, app_id),
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

fn read_tool(app: &vdf_edit::SpannedObject) -> Option<CompatTool> {
    let name = app.get_string(NAME)?.to_string();
    let config = app.get_string(CONFIG).unwrap_or_default().to_string();
    let priority = app
        .get_string(PRIORITY)
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_PRIORITY);
    Some(CompatTool {
        name,
        config,
        priority,
    })
}

/// Write all three pairs of `tool` into the mapping for `app_id`.
fn set_tool(
    input: &str,
    app: Option<&vdf_edit::SpannedObject>,
    mapping: &vdf_edit::SpannedObject,
    app_id: &str,
    tool: &CompatTool,
) -> Result<String, Error> {
    let pairs = tool.pairs();

    // No mapping yet: insert the whole object at once so the layout matches
    // Steam's, in one splice.
    if app.is_none() {
        let borrowed: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        return Ok(vdf_edit::insert_object(input, mapping, app_id, &borrowed));
    };

    // An existing mapping is updated one pair at a time, re-scanning between
    // splices because each one shifts the spans after it. The app object now
    // exists, so a missing key is a plain `insert_pair`.
    let mut text = input.to_string();
    for (key, value) in &pairs {
        let root = vdf_edit::document(&text)?;
        let current = vdf_edit::navigate(&root, &COMPAT_TOOL_MAPPING_PATH)
            .and_then(|mapping| mapping.get_object(app_id))
            .ok_or_else(|| Error::MissingSection(COMPAT_TOOL_MAPPING_PATH.join("/")))?;
        text = match current.get(key) {
            Some(pair) => {
                let Some(span) = pair.value.string_span() else {
                    return Err(Error::NotAString {
                        app_id: app_id.to_string(),
                        key: key.to_string(),
                    });
                };
                vdf_edit::splice(&text, span.start, span.end, &vdf_edit::quote(value))
            }
            None => vdf_edit::insert_pair(&text, current, key, value),
        };
    }
    Ok(text)
}

/// Remove the mapping for `app_id`.
fn remove_tool(
    input: &str,
    app: Option<&vdf_edit::SpannedObject>,
    mapping: &vdf_edit::SpannedObject,
    app_id: &str,
) -> String {
    let Some(app) = app else {
        return input.to_string();
    };

    let compat_keys = [NAME, CONFIG, PRIORITY];
    let present = compat_keys
        .iter()
        .filter(|key| app.get(key).is_some())
        .count();
    if present == 0 {
        return input.to_string();
    }

    // An object that only ever held compat pairs (one we created, or one Steam
    // wrote) goes away with them, so a set-then-remove round trip restores the
    // original layout instead of leaving `"620" { }` behind.
    if present == app.pairs.len()
        && let Some(app_pair) = mapping.get(app_id)
    {
        return vdf_edit::remove_pair(input, app_pair);
    }

    // The object carries unrelated keys: drop only the compat pairs, one splice
    // at a time, until none is left.
    let mut text = input.to_string();
    while let Some(next) = remove_one_compat_pair(&text, app_id) {
        text = next;
    }
    text
}

/// Remove the first compat pair still present in `app_id`, if any.
fn remove_one_compat_pair(input: &str, app_id: &str) -> Option<String> {
    let root = vdf_edit::document(input).ok()?;
    let app = vdf_edit::navigate(&root, &COMPAT_TOOL_MAPPING_PATH)?.get_object(app_id)?;
    let pair = [NAME, CONFIG, PRIORITY]
        .iter()
        .find_map(|key| app.get(key))?;
    Some(vdf_edit::remove_pair(input, pair))
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

    const EMPTY: &str = include_str!("../../../fixtures/steam/config_empty.vdf");
    const WITH_MAPPING: &str = include_str!("../../../fixtures/steam/config_with_compat.vdf");

    fn tool(text: &str, app_id: &str) -> Option<CompatTool> {
        read(text, app_id).unwrap()
    }

    #[test]
    fn reads_existing_mappings() {
        assert_eq!(
            tool(WITH_MAPPING, "620"),
            Some(CompatTool {
                name: "proton_9".to_string(),
                config: String::new(),
                priority: 250,
            })
        );
        assert_eq!(tool(WITH_MAPPING, "440"), None);
        assert_eq!(tool(EMPTY, "620"), None);
    }

    #[test]
    fn a_mapping_without_a_name_reads_as_absent() {
        let input = "\"InstallConfigStore\" {\n\t\"Software\" {\n\t\t\"Valve\" {\n\t\t\t\"Steam\" {\n\t\t\t\t\"CompatToolMapping\" {\n\t\t\t\t\t\"620\" {\n\t\t\t\t\t\t\"Priority\"\t\t\"250\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
        assert_eq!(tool(input, "620"), None);
    }

    #[test]
    fn a_missing_priority_defaults() {
        let input = "\"InstallConfigStore\" {\n\t\"Software\" {\n\t\t\"Valve\" {\n\t\t\t\"Steam\" {\n\t\t\t\t\"CompatToolMapping\" {\n\t\t\t\t\t\"620\" {\n\t\t\t\t\t\t\"name\"\t\t\"proton_9\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
        assert_eq!(tool(input, "620").unwrap().priority, DEFAULT_PRIORITY);
    }

    #[test]
    fn setting_a_new_mapping_inserts_the_whole_object() {
        let edit = edit(EMPTY, "620", Some(&CompatTool::new("proton-cachyos"))).unwrap();
        assert!(edit.changed);
        assert_eq!(edit.previous, None);
        assert!(edit.text.contains(
            "\t\t\t\t\t\"620\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"proton-cachyos\"\n\t\t\t\t\t\t\"config\"\t\t\"\"\n\t\t\t\t\t\t\"Priority\"\t\t\"250\"\n\t\t\t\t\t}\n"
        ));
        assert_eq!(tool(&edit.text, "620").unwrap().name, "proton-cachyos");
        // The rest of the document is untouched.
        assert!(edit.text.contains("\"last_changelist\"\t\t\"1234\""));
    }

    #[test]
    fn replacing_a_mapping_keeps_every_other_byte() {
        let edit = edit(WITH_MAPPING, "620", Some(&CompatTool::new("GE-Proton9-20"))).unwrap();
        assert!(edit.changed);
        assert_eq!(edit.previous.unwrap().name, "proton_9");
        let expected = WITH_MAPPING.replacen("proton_9", "GE-Proton9-20", 1);
        assert_eq!(edit.text, expected);
    }

    #[test]
    fn set_then_clear_restores_the_fixture_byte_for_byte() {
        let inserted = edit(EMPTY, "620", Some(&CompatTool::new("proton-cachyos"))).unwrap();
        let removed = edit(&inserted.text, "620", None).unwrap();
        assert!(removed.changed);
        assert_eq!(removed.text, EMPTY);
        assert_eq!(tool(&removed.text, "620"), None);
    }

    #[test]
    fn clearing_an_existing_mapping_removes_only_that_object() {
        let removed = edit(WITH_MAPPING, "620", None).unwrap();
        assert!(removed.changed);
        assert_eq!(removed.previous.unwrap().name, "proton_9");
        assert_eq!(tool(&removed.text, "620"), None);
        assert!(!removed.text.contains("proton_9"));
        // The empty section and everything else survive.
        assert!(removed.text.contains("\"CompatToolMapping\""));
        crate::vdf::parse(&removed.text).unwrap();
    }

    #[test]
    fn setting_the_same_mapping_is_a_no_op() {
        let existing = tool(WITH_MAPPING, "620").unwrap();
        let edit = edit(WITH_MAPPING, "620", Some(&existing)).unwrap();
        assert!(!edit.changed);
        assert_eq!(edit.text, WITH_MAPPING);
    }

    #[test]
    fn clearing_an_absent_mapping_is_a_no_op() {
        let edit = edit(WITH_MAPPING, "440", None).unwrap();
        assert!(!edit.changed);
        assert_eq!(edit.text, WITH_MAPPING);
        assert_eq!(edit.previous, None);
    }

    #[test]
    fn a_missing_section_is_refused_not_created() {
        let error = edit(
            "\"InstallConfigStore\" { \"Software\" { } }\n",
            "620",
            Some(&CompatTool::new("proton_9")),
        )
        .unwrap_err();
        assert!(matches!(error, Error::MissingSection(_)), "{error}");
        assert!(error.to_string().contains("CompatToolMapping"));
    }

    #[test]
    fn an_empty_name_is_refused() {
        let error = edit(EMPTY, "620", Some(&CompatTool::new("  "))).unwrap_err();
        assert!(matches!(error, Error::EmptyName), "{error}");
    }

    #[test]
    fn non_numeric_appids_are_refused() {
        assert!(matches!(
            edit(EMPTY, "portal", Some(&CompatTool::new("x"))),
            Err(Error::BadAppId(_))
        ));
        assert!(matches!(read(EMPTY, ""), Err(Error::BadAppId(_))));
    }

    #[test]
    fn broken_files_are_never_edited() {
        let error = edit(
            "\"InstallConfigStore\" {",
            "620",
            Some(&CompatTool::new("proton_9")),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Vdf(_)), "{error}");
    }

    #[test]
    fn unrelated_keys_inside_the_mapping_object_are_kept() {
        let input = "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"CompatToolMapping\"\n\t\t\t\t{\n\t\t\t\t\t\"620\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"proton_9\"\n\t\t\t\t\t\t\"SomethingElse\"\t\t\"keep me\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
        let removed = edit(input, "620", None).unwrap();
        assert_eq!(tool(&removed.text, "620"), None);
        assert!(removed.text.contains("\"SomethingElse\"\t\t\"keep me\""));
        assert!(removed.text.contains("\"620\""));
        crate::vdf::parse(&removed.text).unwrap();
    }

    #[test]
    fn an_inline_mapping_object_is_supported() {
        let input = "\"InstallConfigStore\" { \"Software\" { \"Valve\" { \"Steam\" { \"CompatToolMapping\" { } } } } }\n";
        let edit = edit(input, "620", Some(&CompatTool::new("proton_9"))).unwrap();
        assert_eq!(tool(&edit.text, "620").unwrap().name, "proton_9");
        crate::vdf::parse(&edit.text).unwrap();
    }

    #[test]
    fn comments_and_conditionals_survive_an_edit() {
        let input = r#"
// Steam's own header comment
"InstallConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"CompatToolMapping"
				{
					"620"
					{
						// keep me
						"name"		"proton_9" [$WIN32]
					}
				}
			}
		}
	}
}
"#;
        let edit = edit(input, "620", Some(&CompatTool::new("GE-Proton9-20"))).unwrap();
        assert!(edit.text.contains("// keep me"));
        assert!(edit.text.contains("[$WIN32]"));
        assert_eq!(tool(&edit.text, "620").unwrap().name, "GE-Proton9-20");
    }
}
