//! The bounded accessibility check.
//!
//! The M4 accessibility pass is deliberately narrow (spec §M4): the focus graph
//! already proves every cell is keyboard/controller reachable
//! (`pitagames_input::focus::assert_trap_free`), the motion guard in
//! `crates/app/src/shell.rs` proves reduced motion holds by construction, and
//! [`crate::design`]'s contrast test proves the palette meets WCAG AA. What is
//! left is a **label on a control whose visible text is a glyph**: a screen
//! reader has nothing to announce for a button that reads `–` or `✕`.
//!
//! This module is that check, as a source scan in the same spirit as
//! [`crate::i18n`]'s `t("…")` guards — it reads `crates/app/src` rather than
//! compiling it, because the Dioxus crate cannot be built on a non-Linux host.
//! It is explicitly **not** a WCAG audit; it is the one invariant the spec asked
//! for.

/// Every `button` whose only visible content is a glyph but which does not
/// carry an `aria-label`, as the glyphs found in file order.
///
/// An "icon-only" control is a `button` RSX block whose text content is a
/// single string literal with no interpolation (`"–"`, `"✕"`) and no
/// alphanumeric characters. A word button (`"Play"`), an interpolated label
/// (`"{View::Home.label()}"`) and an attribute literal (`title: "Minimize"`)
/// are all ignored — the last because its preceding character is `:`, not a
/// content separator.
///
/// Test code (`#[cfg(test)]` and after) is skipped, so a test fixture may
/// deliberately spell an unlabelled glyph control.
pub fn unlabelled_icon_controls(source: &str) -> Vec<String> {
    let production = source.split("#[cfg(test)]").next().unwrap_or(source);
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(block) = next_button_block(production, &mut cursor) {
        if has_aria_label(&block) {
            continue;
        }
        if let Some(glyph) = content_literal(&block)
            && !glyph.is_empty()
            && !glyph.contains('{')
            && glyph.chars().all(|c| !c.is_alphanumeric())
        {
            found.push(glyph);
        }
    }
    found
}

/// Whether a block carries an `aria-label`, in either Dioxus spelling: the
/// built-in form (`aria-label: …`) or the custom-attribute form a hyphenated
/// name requires (`"aria-label": …`).
fn has_aria_label(block: &str) -> bool {
    block.contains("aria-label:") || block.contains("\"aria-label\":")
}

/// The inner text of the next `button { … }` RSX block at or after `*cursor`,
/// advancing `*cursor` past it. Braces inside string literals are ignored.
fn next_button_block(source: &str, cursor: &mut usize) -> Option<String> {
    let bytes = source.as_bytes();
    while let Some(found) = source[*cursor..].find("button") {
        let at = *cursor + found;
        let mut brace = at + "button".len();
        while brace < bytes.len() && bytes[brace].is_ascii_whitespace() {
            brace += 1;
        }
        if brace >= bytes.len() || bytes[brace] != b'{' {
            *cursor = brace.min(bytes.len());
            continue;
        }
        // Walk to the matching brace, ignoring braces inside a string literal
        // (`style: "…{…}…"` would otherwise unbalance the count).
        let mut depth = 0usize;
        let mut in_string = false;
        let mut index = brace;
        let mut end = None;
        while index < bytes.len() {
            let byte = bytes[index];
            if in_string {
                if byte == b'\\' {
                    index += 2;
                    continue;
                }
                if byte == b'"' {
                    in_string = false;
                }
            } else if byte == b'"' {
                in_string = true;
            } else if byte == b'{' {
                depth += 1;
            } else if byte == b'}' {
                depth -= 1;
                if depth == 0 {
                    end = Some(index);
                    break;
                }
            }
            index += 1;
        }
        let end = end?;
        *cursor = end + 1;
        return Some(source[brace + 1..end].to_string());
    }
    None
}

/// The single string literal a block shows as content, if it has exactly one.
///
/// A content literal is preceded (ignoring whitespace) by `,` or `{`; an
/// attribute value follows a `:`, and a `t("…")` argument follows `(`. A block
/// with zero or several content literals yields `None` — a word button or an
/// interpolated label is never icon-only.
fn content_literal(block: &str) -> Option<String> {
    let mut content = literal_spans(block)
        .into_iter()
        .filter(|(separator, _)| *separator == ',' || *separator == '{')
        .map(|(_, value)| value);
    let first = content.next()?;
    if content.next().is_some() {
        return None;
    }
    Some(first)
}

/// Every string literal in `block`, paired with the last non-whitespace
/// character before it (`','`/`'{'` for content, `':'`/`'('` for an
/// attribute). The scan has no dependency beyond `str`, like the i18n guard.
fn literal_spans(block: &str) -> Vec<(char, String)> {
    let bytes = block.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let start = index;
        let mut value = String::new();
        let mut end = index + 1;
        while end < bytes.len() {
            match bytes[end] {
                b'\\' => {
                    // Keep the escape pair verbatim; a glyph label has none.
                    if let Ok(pair) = std::str::from_utf8(&bytes[end..(end + 2).min(bytes.len())]) {
                        value.push_str(pair);
                    }
                    end += 2;
                }
                b'"' => break,
                _ => {
                    let ch = block[end..].chars().next().expect("a char");
                    value.push(ch);
                    end += ch.len_utf8();
                }
            }
        }
        // A literal that opens the block (only whitespace before it) is the
        // RSX child itself; treat it as `{`. Anything else follows its own
        // separator: `,`/`{` for content, `:`/`(` for an attribute.
        let prefix = &block[..start];
        let separator = if prefix.trim().is_empty() {
            '{'
        } else {
            prefix.trim_end().chars().next_back().unwrap_or(' ')
        };
        spans.push((separator, value));
        index = end + 1;
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// The app's production sources, test code removed — the same seam the i18n
    /// guard and the ADR-0028 source guards use.
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
            let production = text.split("#[cfg(test)]").next().unwrap_or(&text);
            sources.push((path, production.to_string()));
        }
        assert!(!sources.is_empty(), "the app sources were found");
        sources
    }

    #[test]
    fn an_unlabelled_glyph_button_is_found() {
        let source = r#"
            rsx! {
                button {
                    title: "Minimize",
                    onclick: move |_| window.set_minimized(true),
                    style: shell::window_control_style(),
                    "–"
                }
            }
        "#;
        assert_eq!(unlabelled_icon_controls(source), vec!["–".to_string()]);
    }

    #[test]
    fn a_labelled_glyph_button_is_ignored() {
        // The custom-attribute form Dioxus requires for a hyphenated name.
        let source = r#"
            button {
                "aria-label": t("action.close"),
                onclick: move |_| window.close(),
                style: shell::window_control_style(),
                "✕"
            }
        "#;
        assert!(unlabelled_icon_controls(source).is_empty());
        // The built-in form is accepted too.
        assert!(unlabelled_icon_controls(r#"button { aria-label: "Close", "✕" }"#).is_empty());
    }

    #[test]
    fn word_and_interpolated_buttons_are_ignored() {
        // A catalogued word, an interpolation, and an interpolation with a
        // glyph-ish suffix (`" ({n})"`) are all visible copy, not icons.
        let source = r#"
            button { onclick: move |_| go(), style: nav(), {t("action.play")} }
            button {
                onclick: move |_| go(),
                "{View::Home.label()}"
                if n > 0 { " ({n})" }
            }
        "#;
        assert!(unlabelled_icon_controls(source).is_empty());
    }

    #[test]
    fn an_attribute_literal_is_not_content() {
        // `title: "Close"` is a tooltip, not the visible text; the glyph is.
        let source = r#"
            button { title: "Close", "✕" }
        "#;
        assert_eq!(unlabelled_icon_controls(source), vec!["✕".to_string()]);
    }

    #[test]
    fn test_code_is_skipped() {
        let source = r#"
            button { "–" }
            #[cfg(test)]
            fn example() { button { "✕" } }
        "#;
        assert_eq!(unlabelled_icon_controls(source), vec!["–".to_string()]);
    }

    #[test]
    fn a_string_with_braces_does_not_unbalance_the_brace_count() {
        let source = r#"
            button {
                style: "background:url(x);content:'{'",
                "✕"
            }
        "#;
        assert_eq!(unlabelled_icon_controls(source), vec!["✕".to_string()]);
    }

    #[test]
    fn every_icon_only_control_carries_a_label() {
        // The bounded guard the accessibility ticket asks for: a screen reader
        // must have something to announce for the frameless window's chrome.
        let mut unlabelled = Vec::new();
        for (path, text) in app_sources() {
            for glyph in unlabelled_icon_controls(&text) {
                unlabelled.push(format!("{}: {glyph}", path.display()));
            }
        }
        assert!(
            unlabelled.is_empty(),
            "an icon-only control has no `aria-label`:\n{}",
            unlabelled.join("\n")
        );
    }
}
