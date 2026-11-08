//! Span-aware, format-preserving KeyValues edits.
//!
//! Steam's config files are the user's and the client's: `localconfig.vdf`,
//! `config.vdf` and friends gain keys constantly, get hand-edited, and must be
//! restorable byte-for-byte. The writers therefore never re-serialise a
//! document. This module scans the original bytes into spans and exposes the
//! few splices the writers need ([`insert_pair`], [`insert_object`],
//! [`remove_pair`]); every other byte is copied through untouched.
//!
//! It is a separate scanner from [`crate::vdf`] on purpose: `vdf` produces
//! values for reading, this produces offsets for editing, and changing one must
//! never silently change the other. Callers always re-parse an edit with
//! [`crate::vdf::parse`] before it is written, so a scanner bug can never hand
//! a corrupt file to the atomic writer.

/// A byte range in the source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub start: usize,
    pub end: usize,
}

/// Why a span edit was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("{message} (byte {position})")]
    Scan {
        position: usize,
        message: &'static str,
    },

    #[error("unexpected end of input while scanning a Steam config file")]
    Truncated,
}

#[derive(Debug)]
pub(crate) struct SpannedObject {
    pub open: usize,
    pub close: usize,
    pub pairs: Vec<SpannedPair>,
}

#[derive(Debug)]
pub(crate) struct SpannedPair {
    pub key: String,
    pub key_span: Span,
    pub value: SpannedValue,
}

#[derive(Debug)]
pub(crate) enum SpannedValue {
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

    /// The quoted-value span when this is a string, for a value-only splice.
    pub(crate) fn string_span(&self) -> Option<Span> {
        match self {
            Self::String { span, .. } => Some(*span),
            Self::Object(_) => None,
        }
    }
}

impl SpannedObject {
    /// Last matching key wins, mirroring [`crate::vdf::Object::get`].
    pub(crate) fn get(&self, key: &str) -> Option<&SpannedPair> {
        self.pairs
            .iter()
            .rev()
            .find(|pair| pair.key.eq_ignore_ascii_case(key))
    }

    pub(crate) fn get_object(&self, key: &str) -> Option<&SpannedObject> {
        match &self.get(key)?.value {
            SpannedValue::Object(object) => Some(object),
            SpannedValue::String { .. } => None,
        }
    }

    pub(crate) fn get_string(&self, key: &str) -> Option<&str> {
        match &self.get(key)?.value {
            SpannedValue::String { text, .. } => Some(text),
            SpannedValue::Object(_) => None,
        }
    }
}

/// Scan a document's root object, which has no braces of its own.
pub(crate) fn document(input: &str) -> Result<SpannedObject, Error> {
    Scanner::new(input).document()
}

pub(crate) fn navigate<'a>(root: &'a SpannedObject, path: &[&str]) -> Option<&'a SpannedObject> {
    let mut current = root;
    for key in path {
        current = current.get_object(key)?;
    }
    Some(current)
}

/// Escape a value the way Valve's serializer does.
pub(crate) fn quote(value: &str) -> String {
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

pub(crate) fn splice(input: &str, start: usize, end: usize, replacement: &str) -> String {
    let mut out = String::with_capacity(input.len() + replacement.len());
    out.push_str(&input[..start]);
    out.push_str(replacement);
    out.push_str(&input[end..]);
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
pub(crate) fn insert_pair(input: &str, object: &SpannedObject, key: &str, value: &str) -> String {
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

/// Insert an object holding `pairs` as the last entry of `parent`.
///
/// The layout mirrors the surrounding object: a multiline parent gets the
/// nested, tab-indented form Steam writes, an inline parent a single line.
pub(crate) fn insert_object(
    input: &str,
    parent: &SpannedObject,
    key: &str,
    pairs: &[(&str, &str)],
) -> String {
    match layout(input, parent.close) {
        Layout::Multiline { indent } => {
            let child = format!("{indent}\t");
            let inner = format!("{child}\t");
            let mut body = format!("{child}\"{key}\"\n{child}{{\n");
            for (pair_key, pair_value) in pairs {
                body.push_str(&format!("{inner}\"{pair_key}\"\t\t{}\n", quote(pair_value)));
            }
            body.push_str(&format!("{child}}}\n"));
            let line_start = line_start(input, parent.close);
            splice(input, line_start, line_start, &body)
        }
        Layout::Inline => {
            let needs_space = !input[..parent.close]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
            let lead = if needs_space { " " } else { "" };
            let mut body = format!("\"{key}\" {{");
            for (pair_key, pair_value) in pairs {
                body.push_str(&format!(" \"{pair_key}\"\t\t{}", quote(pair_value)));
            }
            body.push_str(" }");
            splice(input, parent.close, parent.close, &format!("{lead}{body} "))
        }
    }
}

/// Remove a pair, taking the whitespace with it when it owns its line.
pub(crate) fn remove_pair(input: &str, pair: &SpannedPair) -> String {
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

pub(crate) fn line_start(input: &str, position: usize) -> usize {
    input[..position].rfind('\n').map_or(0, |index| index + 1)
}

fn is_horizontal_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
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
