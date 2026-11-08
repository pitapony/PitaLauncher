//! Valve KeyValues (VDF) parser.
//!
//! Steam's `libraryfolders.vdf` and `appmanifest_*.acf` files are KeyValues
//! documents. The parser is deliberately small and dependency-free:
//!
//! - order-preserving (entries are a `Vec`, not a map);
//! - duplicate keys are kept, `get` returns the last one;
//! - `//` line comments, `[$WIN32]` conditionals and a UTF-8 BOM are handled;
//! - quoted strings support Valve's escapes.
//!
//! There is no serializer yet. The Steam config writer (M0) needs one, and it
//! must preserve comments and formatting, which is a different problem.

use std::fmt;

/// A parsed KeyValues value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    String(String),
    Object(Object),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Object(_) => None,
        }
    }

    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Self::Object(object) => Some(object),
            Self::String(_) => None,
        }
    }
}

/// An ordered KeyValues object.
///
/// Keys are compared ASCII-case-insensitively because Steam is inconsistent
/// between `AppState`/`appstate` and `LibraryFolders` across client versions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Object {
    entries: Vec<(String, Value)>,
}

impl Object {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[(String, Value)] {
        &self.entries
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> + '_ {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    /// Last matching key wins, matching how Steam treats re-declared keys.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .rev()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    pub fn get_object(&self, key: &str) -> Option<&Object> {
        self.get(key).and_then(Value::as_object)
    }
}

/// A syntax error with a 1-based source position.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} (line {line}, column {column})")]
pub struct ParseError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

/// Parse a KeyValues document into its root object.
pub fn parse(input: &str) -> Result<Object, ParseError> {
    Parser {
        bytes: input.as_bytes(),
        pos: 0,
    }
    .parse_document()
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn parse_document(mut self) -> Result<Object, ParseError> {
        if self.bytes.starts_with(b"\xEF\xBB\xBF") {
            self.pos = 3;
        }

        let mut root = Object::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return Ok(root),
                Some(b'}') => return Err(self.error("unexpected '}'")),
                Some(_) => {
                    let (key, value) = self.parse_pair()?;
                    root.entries.push((key, value));
                }
            }
        }
    }

    fn parse_pair(&mut self) -> Result<(String, Value), ParseError> {
        let key = self.parse_token("a key")?;
        self.skip_trivia();

        let value = match self.peek() {
            Some(b'{') => Value::Object(self.parse_object()?),
            Some(b'}') => return Err(self.error("expected a value, found '}'")),
            None => return Err(self.error("expected a value, found end of file")),
            Some(b'"') => Value::String(self.parse_quoted()?),
            Some(_) => Value::String(self.parse_bare()?),
        };

        self.skip_conditional()?;
        Ok((key, value))
    }

    fn parse_object(&mut self) -> Result<Object, ParseError> {
        debug_assert_eq!(self.peek(), Some(b'{'));
        self.pos += 1;

        let mut object = Object::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return Err(self.error("unexpected end of file inside an object")),
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(object);
                }
                Some(_) => {
                    let (key, value) = self.parse_pair()?;
                    object.entries.push((key, value));
                }
            }
        }
    }

    fn parse_token(&mut self, what: &str) -> Result<String, ParseError> {
        self.skip_trivia();
        match self.peek() {
            Some(b'"') => self.parse_quoted(),
            Some(b'{') | Some(b'}') => Err(self.error(&format!("expected {what}, found '}}'"))),
            None => Err(self.error(&format!("expected {what}, found end of file"))),
            Some(_) => self.parse_bare(),
        }
    }

    fn parse_bare(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_whitespace() || matches!(byte, b'{' | b'}' | b'[' | b']' | b'"') {
                break;
            }
            self.pos += 1;
        }

        if self.pos == start {
            return Err(self.error("expected a token"));
        }

        String::from_utf8(self.bytes[start..self.pos].to_vec())
            .map_err(|_| self.error_at(start, "token is not valid UTF-8"))
    }

    fn parse_quoted(&mut self) -> Result<String, ParseError> {
        let open = self.pos;
        self.pos += 1; // opening quote

        let mut out = Vec::new();
        loop {
            match self.next_byte() {
                None => return Err(self.error_at(open, "unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => {
                    let escape = self
                        .next_byte()
                        .ok_or_else(|| self.error_at(open, "unterminated escape sequence"))?;
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
                        b'?' => b'?',
                        // Valve keeps the character for unknown escapes.
                        other => other,
                    });
                }
                Some(byte) => out.push(byte),
            }
        }

        String::from_utf8(out).map_err(|_| self.error_at(open, "string is not valid UTF-8"))
    }

    fn skip_trivia(&mut self) {
        loop {
            while matches!(self.peek(), Some(byte) if byte.is_ascii_whitespace()) {
                self.pos += 1;
            }

            if self.starts_with(b"//") {
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

    /// Consume an optional `[$WIN32]` platform conditional after a value.
    fn skip_conditional(&mut self) -> Result<(), ParseError> {
        self.skip_trivia();
        if self.peek() != Some(b'[') {
            return Ok(());
        }

        let open = self.pos;
        self.pos += 1;
        while let Some(byte) = self.next_byte() {
            if byte == b']' {
                return Ok(());
            }
        }
        Err(self.error_at(open, "unterminated conditional"))
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next_byte(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn starts_with(&self, needle: &[u8]) -> bool {
        self.bytes[self.pos..].starts_with(needle)
    }

    fn error(&self, message: &str) -> ParseError {
        self.error_at(self.pos, message)
    }

    fn error_at(&self, position: usize, message: &str) -> ParseError {
        let (line, column) = line_column(self.bytes, position);
        ParseError {
            line,
            column,
            message: message.to_string(),
        }
    }
}

fn line_column(bytes: &[u8], position: usize) -> (usize, usize) {
    let position = position.min(bytes.len());
    let before = &bytes[..position];
    let line = before.iter().filter(|&&byte| byte == b'\n').count() + 1;
    let line_start = before
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |index| index + 1);
    (line, position - line_start + 1)
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(value) => f.write_str(value),
            Self::Object(object) => write!(f, "<object with {} entries>", object.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_objects_in_order() {
        let root = parse(
            r#"
            "root"
            {
                "a" "1"
                "b" { "c" "2" "d" "3" }
                "e" "4"
            }
            "#,
        )
        .unwrap();

        let root = root.get_object("root").unwrap();
        let keys: Vec<&str> = root.iter().map(|(key, _)| key).collect();
        assert_eq!(keys, ["a", "b", "e"]);
        assert_eq!(root.get_str("a"), Some("1"));
        assert_eq!(root.get_object("b").unwrap().get_str("d"), Some("3"));
    }

    #[test]
    fn decodes_valve_escapes() {
        let root = parse(r#""root" { "path" "line\nquote\"back\\slash" }"#).unwrap();
        assert_eq!(
            root.get_object("root").unwrap().get_str("path"),
            Some("line\nquote\"back\\slash")
        );
    }

    #[test]
    fn keeps_duplicate_keys_and_last_wins() {
        let root = parse(r#""root" { "k" "1" "k" "2" }"#).unwrap();
        let root = root.get_object("root").unwrap();
        assert_eq!(root.len(), 2);
        assert_eq!(root.get_str("k"), Some("2"));
    }

    #[test]
    fn skips_comments_and_conditionals() {
        let root = parse(
            r#"
            // leading comment
            "root"
            {
                "win" "1" [$WIN32] // trailing comment
                "linux" "2"
            }
            "#,
        )
        .unwrap();
        assert_eq!(root.get_object("root").unwrap().len(), 2);
    }

    #[test]
    fn accepts_bom_and_bare_tokens() {
        let root = parse("\u{feff}root { a b }").unwrap();
        assert_eq!(root.get_object("root").unwrap().get_str("a"), Some("b"));
    }

    #[test]
    fn keys_are_case_insensitive() {
        let root = parse(r#""AppState" { "appid" "570" }"#).unwrap();
        assert_eq!(
            root.get_object("appstate").unwrap().get_str("APPID"),
            Some("570")
        );
    }

    #[test]
    fn reports_unterminated_string_position() {
        let error = parse("\"root\"\n{\n    \"path\" \"/oops\n}").unwrap_err();
        assert!(error.message.contains("unterminated string"), "{error}");
        assert_eq!((error.line, error.column), (3, 12));
    }

    #[test]
    fn reports_truncated_object() {
        let error = parse("\"root\"\n{\n  \"a\" \"1\"\n").unwrap_err();
        assert!(error.message.contains("end of file"), "{error}");
    }

    #[test]
    fn errors_on_stray_closing_brace() {
        let error = parse("\"root\" { } }").unwrap_err();
        assert!(error.message.contains("unexpected '}'"), "{error}");
    }

    #[test]
    fn handles_utf8_values() {
        let root = parse(r#""root" { "title" "café ☕" }"#).unwrap();
        assert_eq!(
            root.get_object("root").unwrap().get_str("title"),
            Some("café ☕")
        );
    }
}
