//! The catalog seam: one key → one user-facing string, `en` first.
//!
//! v1 ships one language and no translations. The point of the seam is that a
//! user-facing string is a *key* looked up in a catalog, not a literal buried in
//! a component, so adding a language later is a data change (one more catalog
//! file) rather than a rewrite of every screen.
//!
//! - [`t`] is the one lookup the UI calls: `t("library.title")`.
//! - [`Language`] is the closed set of shipped catalogs; [`Language::ALL`] is
//!   the list the tests iterate, so a new language is added in one place.
//! - Each catalog is TOML data ([`Language::source`]) parsed by [`Catalog`]; the
//!   `en` file lives at `crates/core/data/i18n/en.toml`.
//!
//! The seam is deliberately a function, not a framework: `crates/core` has no
//! UI dependency, and the app depends on this crate already. A missing key
//! falls back to the key itself so the UI never panics; a test in this module
//! scans the app sources and fails when a used key is missing or an entry is
//! unused, so that fallback is a safety net rather than a habit.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The `en` catalog, embedded so the shipped binary carries its own strings.
pub const EN_SOURCE: &str = include_str!("../data/i18n/en.toml");

/// A shipped language.
///
/// Adding a language is a data change: add a variant, list it in
/// [`Language::ALL`], point [`Language::source`] at a new TOML file whose keys
/// match `en`, and fill in the strings. Nothing else in the app changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// English, the only catalog in v1.
    En,
}

impl Language {
    /// Every shipped catalog, in the order a language picker would show them.
    ///
    /// This is the one list the tests iterate, so a new variant that is not
    /// listed here fails the "catalogs agree" test.
    pub const ALL: &'static [Language] = &[Language::En];

    /// The BCP-47-ish tag the catalog is filed under.
    pub const fn code(self) -> &'static str {
        match self {
            Self::En => "en",
        }
    }

    /// The TOML source for this language.
    pub const fn source(self) -> &'static str {
        match self {
            Self::En => EN_SOURCE,
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Why a catalog could not be read.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The source is not a flat TOML table of `"key" = "string"`.
    #[error("the {language} catalog is not valid TOML: {source}")]
    Parse {
        language: Language,
        source: toml::de::Error,
    },
    /// A key was empty.
    #[error("the {language} catalog has an empty key")]
    EmptyKey { language: Language },
    /// A value was empty; an untranslated entry must be caught, not shipped.
    #[error("the {language} catalog has an empty string for `{key}`")]
    EmptyValue { language: Language, key: String },
}

/// One language's keys and strings.
#[derive(Debug, Clone)]
pub struct Catalog {
    language: Language,
    entries: BTreeMap<String, String>,
}

impl Catalog {
    /// Parse a language's embedded source.
    pub fn parse(language: Language) -> Result<Self, CatalogError> {
        Self::from_source(language, language.source())
    }

    /// Parse an explicit source, so a test can use a fixture.
    pub fn from_source(language: Language, source: &str) -> Result<Self, CatalogError> {
        let entries: BTreeMap<String, String> =
            toml::from_str(source).map_err(|source| CatalogError::Parse { language, source })?;
        for (key, value) in &entries {
            if key.trim().is_empty() {
                return Err(CatalogError::EmptyKey { language });
            }
            if value.trim().is_empty() {
                return Err(CatalogError::EmptyValue {
                    language,
                    key: key.clone(),
                });
            }
        }
        Ok(Self { language, entries })
    }

    /// The language this catalog speaks.
    pub fn language(&self) -> Language {
        self.language
    }

    /// The string for a key, or `None` when the key is not catalogued.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    /// Every key, sorted (the backing map is ordered).
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// How many entries the catalog holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the catalog holds no keys.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The lookup itself: a catalog's string, else the key.
///
/// Split from [`t`] so the fallback rule is testable with a fixture catalog and
/// an arbitrary key, without touching the shipped `en` file.
fn lookup<'a>(catalog: &'a Catalog, key: &'a str) -> &'a str {
    catalog.get(key).unwrap_or(key)
}

/// The `en` catalog, parsed once.
///
/// A malformed catalog yields an empty one rather than a panic: the module's
/// own test fails on the malformed source long before a user sees a key.
pub fn en() -> &'static Catalog {
    static EN: OnceLock<Catalog> = OnceLock::new();
    EN.get_or_init(|| Catalog::parse(Language::En).unwrap_or_else(|_| Catalog {
        language: Language::En,
        entries: BTreeMap::new(),
    }))
}

/// The one lookup a screen calls: `t("library.title")`.
///
/// The key is `&'static str`, so only a literal (or a `const`) can be passed:
/// the key set is fixed at compile time and the source scan can enumerate it.
pub fn t(key: &'static str) -> &'static str {
    lookup(en(), key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;

    #[test]
    fn the_en_catalog_parses_and_has_no_empty_values() {
        let catalog = Catalog::parse(Language::En).expect("the en catalog parses");
        assert_eq!(catalog.language(), Language::En);
        assert!(!catalog.is_empty(), "the en catalog has strings");
        assert_eq!(catalog.len(), catalog.keys().count());
        for (key, value) in catalog.entries.iter() {
            assert!(!key.trim().is_empty(), "empty key");
            assert!(!value.trim().is_empty(), "`{key}` has an empty string");
        }
    }

    #[test]
    fn every_language_catalog_has_the_same_keys_as_en() {
        // Adding a language must not be able to ship a half-translated catalog:
        // every shipped language names exactly the `en` key set.
        let english: BTreeSet<String> = Catalog::parse(Language::En)
            .unwrap()
            .keys()
            .map(str::to_string)
            .collect();
        for language in Language::ALL {
            let catalog = Catalog::parse(*language).expect("a shipped catalog parses");
            let keys: BTreeSet<String> = catalog.keys().map(str::to_string).collect();
            assert_eq!(
                keys, english,
                "the {language} catalog does not match the en key set"
            );
        }
    }

    #[test]
    fn a_second_language_is_only_a_source() {
        // The whole point of the seam: a new language is a new source with the
        // same keys, not a new code path.
        let french = Catalog::from_source(
            Language::En,
            "\"nav.home\" = \"Accueil\"\n\"nav.library\" = \"Bibliothèque\"\n",
        )
        .expect("a fixture catalog parses");
        assert_eq!(french.get("nav.home"), Some("Accueil"));
        assert_eq!(french.len(), 2);
    }

    #[test]
    fn lookup_falls_back_to_the_key_and_reads_the_catalog() {
        let catalog =
            Catalog::from_source(Language::En, "greeting = \"Hello\"\n").expect("parses");
        assert_eq!(lookup(&catalog, "greeting"), "Hello");
        assert_eq!(
            lookup(&catalog, "missing.key"),
            "missing.key",
            "an unknown key shows itself rather than panicking"
        );
    }

    #[test]
    fn a_blank_string_is_rejected() {
        let error = Catalog::from_source(Language::En, "key = \"  \"").unwrap_err();
        assert!(matches!(error, CatalogError::EmptyValue { .. }));
        let error = Catalog::from_source(Language::En, "\"\" = \"x\"").unwrap_err();
        assert!(matches!(error, CatalogError::EmptyKey { .. }));
        assert!(Catalog::from_source(Language::En, "not toml").is_err());
    }

    // -- The source scan: the UI and the catalog cannot drift ----------------

    /// The app's production sources, with test code removed.
    ///
    /// The GUI crate cannot be built on a non-Linux host, so the check reads the
    /// files instead of compiling them. `crates/core` sits beside `crates/app`
    /// in the workspace, so the path is stable.
    fn app_sources() -> Vec<(PathBuf, String)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/")
            .join("app/src");
        let mut sources = Vec::new();
        for entry in fs::read_dir(&dir).expect("read the app sources") {
            let path = entry.expect("a source entry").path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let text = fs::read_to_string(&path).expect("read an app source");
            // Production code only: a test may name a key to assert on.
            let production = text.split("#[cfg(test)]").next().unwrap_or(&text);
            sources.push((path, production.to_string()));
        }
        assert!(!sources.is_empty(), "the app sources were found");
        sources
    }

    /// Every key passed to `t("…")` in the app's production code.
    fn keys_used_by_the_ui() -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        for (path, text) in app_sources() {
            for key in scan_t_calls(&text) {
                keys.insert(format!("{}::{key}", path.display()));
            }
        }
        // Report the bare key set; the path is only for a helpful message.
        keys.into_iter()
            .map(|entry| entry.split("::").last().unwrap().to_string())
            .collect()
    }

    /// Find `t("key")` calls in one source file.
    ///
    /// Keys are plain ASCII with no escapes, so the scan can be a small hand
    /// parser instead of a regex dependency. `format(` and friends never match:
    /// a `t` preceded by an identifier character is not a call to [`t`].
    fn scan_t_calls(text: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let mut cursor = 0;
        while let Some(found) = text[cursor..].find("t(") {
            let at = cursor + found;
            let preceded_by_word = text[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if !preceded_by_word {
                let rest = text[at + 2..].trim_start();
                if let Some(after_quote) = rest.strip_prefix('"')
                    && let Some(end) = after_quote.find('"')
                {
                    keys.push(after_quote[..end].to_string());
                }
            }
            cursor = at + 2;
        }
        keys
    }

    #[test]
    fn the_scan_finds_t_calls_and_ignores_lookalikes() {
        let text = r#"
            let a = t("nav.home");
            let b = format("{}", x);
            let c = i18n::t("action.play");
            let d = "literal t(\"x\")";
        "#;
        let found = scan_t_calls(text);
        assert_eq!(found, vec!["nav.home", "action.play"]);
    }

    #[test]
    fn every_key_the_ui_uses_is_in_the_catalog() {
        let catalog = Catalog::parse(Language::En).expect("the en catalog parses");
        let known: BTreeSet<&str> = catalog.keys().collect();
        let mut missing: BTreeSet<String> = BTreeSet::new();
        for (path, text) in app_sources() {
            for key in scan_t_calls(&text) {
                if !known.contains(key.as_str()) {
                    missing.insert(format!("{}: {key}", path.display()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "the UI uses keys the en catalog does not define:\n{}",
            missing.into_iter().collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn every_catalog_entry_is_used_by_the_ui() {
        let catalog = Catalog::parse(Language::En).expect("the en catalog parses");
        let used = keys_used_by_the_ui();
        let unused: Vec<&str> = catalog
            .keys()
            .filter(|key| !used.contains(*key))
            .collect();
        assert!(
            unused.is_empty(),
            "the en catalog has entries no screen uses:\n{}",
            unused.join("\n")
        );
    }
}
