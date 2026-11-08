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
//! untouched. `crate::steam_config` adds the Steam-closed guard, the backup and
//! the atomic replace around it.
//!
//! The parser here is a span-aware copy of [`crate::vdf`]'s grammar. It is
//! deliberately separate: the VDF parser produces values, this one produces
//! offsets, and changing one must never silently change the other. Every edit
//! is re-parsed with [`crate::vdf::parse`] before it is returned.

use crate::vdf;

/// The key Steam uses for a game's launch options.
pub const LAUNCH_OPTIONS: &str = "LaunchOptions";

/// Where per-app launch options live inside `localconfig.vdf`.
pub const APP_OBJECT_PATH: [&str; 5] =
    ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"];

/// A byte range in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

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
    Vdf(#[from] vdf::ParseError),

    #[error("localconfig.vdf has no `{0}` object; refusing to create Steam's layout")]
    MissingSection(String),

    #[error("`{app_id}` has a non-string `LaunchOptions` value")]
    NotAString { app_id: String },

    #[error("{message} (byte {position})")]
    Scan {
        position: usize,
        message: &'static str,
    },

    #[error("unexpected end of input while scanning localconfig.vdf")]
    Truncated,

    #[error("edit produced invalid VDF: {0}")]
    InvalidResult(String),
}

/// Read one app's launch options without changing anything.
///
/// A missing `apps` section, missing app or missing key all yield `Ok(None)`:
/// only [`edit`] insists on the expected structure, because only a writer can
/// corrupt the file.
pub fn read(input: &str, app_id: &str) -> Result<Option<String>, Error> {
    validate_app_id(app_id)?;
    vdf::parse(input)?;
    let root = Scanner::new(input).document()?;
    Ok(navigate(&root, &APP_OBJECT_PATH)
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
    vdf::parse(input)?;
    let root = Scanner::new(input).document()?;

    let apps = navigate(&root, &APP_OBJECT_PATH)
        .ok_or_else(|| Error::MissingSection(APP_OBJECT_PATH.join("/")))?;
    let app = apps.get_object(app_id);
    let previous = app
        .and_then(|app| app.get_string(LAUNCH_OPTIONS))
        .map(str::to_string);

    let text = match (value, app) {
        (Some(value), Some(app)) => match app.get(LAUNCH_OPTIONS) {
            Some(pair) => {
                if !matches!(pair.value, SpannedValue::String { .. }) {
                    return Err(Error::NotAString {
                        app_id: app_id.to_string(),
                    });
                }
                let span = pair.value.span();
                splice(input, span.start, span.end, &quote(value))
            }
            None => insert_pair(input, app, LAUNCH_OPTIONS, value),
        },
        (Some(value), None) => insert_app(input, apps, app_id, value),
        (None, Some(app)) => match app.get(LAUNCH_OPTIONS) {
            Some(pair) => {
                // An app object that only ever held the launch options goes away
                // with them, so a set-then-remove round trip restores the
                // original layout instead of leaving `"620" { }` behind.
                match apps.get(app_id).filter(|_| app.pairs.len() == 1) {
                    Some(app_pair) => remove_pair(input, app_pair),
                    None => remove_pair(input, pair),
                }
            }
            None => input.to_string(),
        },
        (None, None) => input.to_string(),
    };

    if text != input {
        // Never hand a corrupt document to the atomic writer.
        vdf::parse(&text).map_err(|error| Error::InvalidResult(error.to_string()))?;
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

/// Escape a value the way Valve's serializer does.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The indentation style around an object's closing brace.
enum Layout<'a> {
    /// The `}` starts its own line; `indent` is the whitespace before it.
    Multiline { indent: &'a str },
    /// The `}` shares a line with other tokens.
    Inline,
}

fn layout(input: &str, close: usize) -> Layout<'_> {
    let line_start = input[..close].rfind('\n').map_or(0, |index| index + 1);
    let indent = &input[line_start..close];
    if indent.bytes().all(is_horizontal_space) {
        Layout::Multiline { indent }
    } else {
        Layout::Inline
    }
}

/// Insert a `key value` pair as the last entry of an object.
fn insert_pair(input: &str, object: &SpannedObject, key: &str, value: &str) -> String {
    let body = format!("{key}\t\t{}", quote(value));
    match layout(input, object.close) {
        Layout::Multiline { indent } => {
            let line_start = line_start(input, object.close);
            splice(
                input,
                line_start,
                line_start,
                &format!("{indent}\t{body}\n"),
            )
        }
        Layout::Inline => {
            let needs_space = !input[..object.close]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
            let lead = if needs_space { " " } else { "" };
            splice(input, object.close, object.close, &format!("{lead}{body} "))
        }
    }
}

/// Insert a whole app object (and its `LaunchOptions`) into `apps`.
fn insert_app(input: &str, apps: &SpannedObject, app_id: &str, value: &str) -> String {
    let quoted = quote(value);
    match layout(input, apps.close) {
        Layout::Multiline { indent } => {
            let child = format!("{indent}\t");
            let inner = format!("{child}\t");
            let body = format!(
                "{child}\"{app_id}\"\n{child}{{\n{inner}\"{LAUNCH_OPTIONS}\"\t\t{quoted}\n{child}}}\n"
            );
            let line_start = line_start(input, apps.close);
            splice(input, line_start, line_start, &body)
        }
        Layout::Inline => {
            // An inline `apps` object gets an inline app object, not a bare
            // string pair: the app key must keep holding a `LaunchOptions`.
            let needs_space = !input[..apps.close]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
            let lead = if needs_space { " " } else { "" };
            let body = format!("\"{app_id}\" {{ \"{LAUNCH_OPTIONS}\"\t\t{quoted} }}");
            splice(input, apps.close, apps.close, &format!("{lead}{body} "))
        }
    }
}

/// Remove a pair, taking the whitespace with it when it owns its line.
fn remove_pair(input: &str, pair: &SpannedPair) -> String {
    let key_start = pair.key_span.start;
    let value_end = pair.value.span().end;

    let start = line_start(input, key_start);
    let left_alone = input[start..key_start].bytes().all(is_horizontal_space);
    let end = input[value_end..]
        .find('\n')
        .map_or(input.len(), |offset| value_end + offset + 1);
    let right_alone = input[value_end..end]
        .bytes()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'));

    if left_alone && right_alone {
        return splice(input, start, end, "");
    }

    // Compact or shared-line layouts: remove the pair and the space run before
    // it, but never a newline (that would join the neighbouring lines).
    let mut start = key_start;
    while start > 0 && is_horizontal_space(input.as_bytes()[start - 1]) {
        start -= 1;
    }
    splice(input, start, value_end, "")
}

fn line_start(input: &str, position: usize) -> usize {
    input[..position].rfind('\n').map_or(0, |index| index + 1)
}

fn is_horizontal_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

fn splice(input: &str, start: usize, end: usize, replacement: &str) -> String {
    let mut out = String::with_capacity(input.len() + replacement.len());
    out.push_str(&input[..start]);
    out.push_str(replacement);
    out.push_str(&input[end..]);
    out
}

fn navigate<'a>(root: &'a SpannedObject, path: &[&str]) -> Option<&'a SpannedObject> {
    let mut current = root;
    for key in path {
        current = current.get_object(key)?;
    }
    Some(current)
}

// ---------------------------------------------------------------------------
// Span-aware scanner
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SpannedObject {
    open: usize,
    close: usize,
    pairs: Vec<SpannedPair>,
}

#[derive(Debug)]
struct SpannedPair {
    key: String,
    key_span: Span,
    value: SpannedValue,
}

#[derive(Debug)]
enum SpannedValue {
    String { span: Span, text: String },
    Object(SpannedObject),
}

impl SpannedValue {
    fn span(&self) -> Span {
        match self {
            Self::String { span, .. } => *span,
            Self::Object(object) => Span {
                start: object.open,
                end: object.close + 1,
            },
        }
    }
}

impl SpannedObject {
    /// Last matching key wins, mirroring [`crate::vdf::Object::get`].
    fn get(&self, key: &str) -> Option<&SpannedPair> {
        self.pairs
            .iter()
            .rev()
            .find(|pair| pair.key.eq_ignore_ascii_case(key))
    }

    fn get_object(&self, key: &str) -> Option<&SpannedObject> {
        match &self.get(key)?.value {
            SpannedValue::Object(object) => Some(object),
            SpannedValue::String { .. } => None,
        }
    }

    fn get_string(&self, key: &str) -> Option<&str> {
        match &self.get(key)?.value {
            SpannedValue::String { text, .. } => Some(text),
            SpannedValue::Object(_) => None,
        }
    }
}

struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(input: &'a str) -> Self {
        let mut pos = 0;
        if input.as_bytes().starts_with(b"\xEF\xBB\xBF") {
            pos = 3;
        }
        Self {
            bytes: input.as_bytes(),
            pos,
        }
    }

    /// Parse the document root, which has no braces of its own.
    fn document(mut self) -> Result<SpannedObject, Error> {
        let mut pairs = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => break,
                Some(b'}') => return Err(self.scan_error("unexpected '}'")),
                Some(_) => pairs.push(self.pair()?),
            }
        }
        Ok(SpannedObject {
            open: 0,
            close: self.bytes.len(),
            pairs,
        })
    }

    fn object(&mut self) -> Result<SpannedObject, Error> {
        let open = self.pos;
        debug_assert_eq!(self.peek(), Some(b'{'));
        self.pos += 1;

        let mut pairs = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return Err(Error::Truncated),
                Some(b'}') => {
                    let close = self.pos;
                    self.pos += 1;
                    return Ok(SpannedObject { open, close, pairs });
                }
                Some(_) => pairs.push(self.pair()?),
            }
        }
    }

    fn pair(&mut self) -> Result<SpannedPair, Error> {
        self.skip_trivia();
        let start = self.pos;
        let key = self.token()?;
        let key_span = Span {
            start,
            end: self.pos,
        };

        self.skip_trivia();
        let value = match self.peek() {
            Some(b'{') => SpannedValue::Object(self.object()?),
            Some(b'"') => {
                let start = self.pos;
                let text = self.quoted()?;
                SpannedValue::String {
                    span: Span {
                        start,
                        end: self.pos,
                    },
                    text,
                }
            }
            None => return Err(Error::Truncated),
            Some(b'}') => return Err(self.scan_error("expected a value")),
            Some(_) => {
                let start = self.pos;
                let text = self.bare()?;
                SpannedValue::String {
                    span: Span {
                        start,
                        end: self.pos,
                    },
                    text,
                }
            }
        };

        self.skip_conditional()?;
        Ok(SpannedPair {
            key,
            key_span,
            value,
        })
    }

    fn token(&mut self) -> Result<String, Error> {
        self.skip_trivia();
        match self.peek() {
            Some(b'"') => self.quoted(),
            Some(b'{') | Some(b'}') => Err(self.scan_error("expected a key")),
            None => Err(Error::Truncated),
            Some(_) => self.bare(),
        }
    }

    fn quoted(&mut self) -> Result<String, Error> {
        debug_assert_eq!(self.peek(), Some(b'"'));
        self.pos += 1;

        let mut out = Vec::new();
        loop {
            match self.next_byte() {
                None => return Err(self.scan_error("unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => {
                    let escape = self
                        .next_byte()
                        .ok_or_else(|| self.scan_error("unterminated escape sequence"))?;
                    out.push(match escape {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'v' => 0x0B,
                        b'b' => 0x08,
                        b'f' => 0x0C,
                        b'a' => 0x07,
                        b'\\' => b'\\',
                        b'"' => b'"',
                        b'\'' => b'\'',
                        other => other,
                    });
                }
                Some(byte) => out.push(byte),
            }
        }

        String::from_utf8(out).map_err(|_| self.scan_error("string is not valid UTF-8"))
    }

    fn bare(&mut self) -> Result<String, Error> {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_whitespace() || matches!(byte, b'{' | b'}' | b'[' | b']' | b'"') {
                break;
            }
            self.pos += 1;
        }

        if self.pos == start {
            return Err(self.scan_error("expected a token"));
        }

        String::from_utf8(self.bytes[start..self.pos].to_vec())
            .map_err(|_| self.scan_error("token is not valid UTF-8"))
    }

    fn skip_trivia(&mut self) {
        loop {
            while matches!(self.peek(), Some(byte) if byte.is_ascii_whitespace()) {
                self.pos += 1;
            }
            if self.bytes[self.pos..].starts_with(b"//") {
                while let Some(byte) = self.next_byte() {
                    if byte == b'\n' {
                        break;
                    }
                }
                continue;
            }
            break;
        }
    }

    fn skip_conditional(&mut self) -> Result<(), Error> {
        self.skip_trivia();
        if self.peek() != Some(b'[') {
            return Ok(());
        }

        self.pos += 1;
        while let Some(byte) = self.next_byte() {
            if byte == b']' {
                return Ok(());
            }
        }
        Err(self.scan_error("unterminated conditional"))
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next_byte(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn scan_error(&self, message: &'static str) -> Error {
        // The public VDF parser already reported syntax errors before this
        // scanner runs, so reaching here is an internal inconsistency.
        Error::Scan {
            position: self.pos,
            message,
        }
    }
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

        let original = WITH_OPTIONS.replacen("-novid %command%", "DXVK_HUD=fps %command%", 1);
        assert_eq!(edit.text, original);
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
        vdf::parse(&edit.text).unwrap();
    }

    #[test]
    fn an_inline_empty_apps_object_is_supported() {
        let input = "\"UserLocalConfigStore\" { \"Software\" { \"Valve\" { \"Steam\" { \"apps\" { } } } } }\n";
        let edit = edit(input, "620", Some("%command%")).unwrap();
        assert_eq!(options(&edit.text, "620").as_deref(), Some("%command%"));
        vdf::parse(&edit.text).unwrap();
    }

    #[test]
    fn an_inline_app_object_is_supported() {
        let input = "\"UserLocalConfigStore\" { \"Software\" { \"Valve\" { \"Steam\" { \"apps\" { \"620\" { } } } } } }\n";
        let edit = edit(input, "620", Some("%command%")).unwrap();
        assert_eq!(options(&edit.text, "620").as_deref(), Some("%command%"));
        vdf::parse(&edit.text).unwrap();
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
