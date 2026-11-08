//! HTML entity decoding for store text.
//!
//! Steam's `short_description` is plain text but entity-escaped (`&quot;`,
//! `&amp;`). We fetch no HTML fields at all, so a full parser would be
//! overkill; this covers the entities Steam actually emits.

/// Decode the common named entities plus `&#NN;` / `&#xNN;` numeric forms.
pub fn unescape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let after = &rest[index..];

        let Some(end) = after.find(';') else {
            // No terminator: the ampersand is literal text.
            out.push_str(after);
            return out;
        };

        match decode_entity(&after[1..end]) {
            Some(decoded) => {
                out.push(decoded);
                rest = &after[end + 1..];
            }
            None => {
                // Not an entity we know; keep the ampersand and move on.
                out.push('&');
                rest = &after[1..];
            }
        }
    }

    out.push_str(rest);
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let digits = entity.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<u32>().ok()?,
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_named_entities() {
        assert_eq!(
            unescape_html("The &quot;Perpetual Testing Initiative&quot; &amp; more"),
            "The \"Perpetual Testing Initiative\" & more"
        );
        assert_eq!(unescape_html("a &lt;b&gt; c&apos;d"), "a <b> c'd");
        assert_eq!(unescape_html("two&nbsp;spaces"), "two spaces");
    }

    #[test]
    fn decodes_numeric_entities() {
        assert_eq!(unescape_html("caf&#233;"), "café");
        assert_eq!(unescape_html("star &#x2605;"), "star ★");
        assert_eq!(
            unescape_html("bad &#x110000; code"),
            "bad &#x110000; code",
            "out-of-range code points stay literal"
        );
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(unescape_html("Portal 2"), "Portal 2");
        assert_eq!(unescape_html("Rock & Roll"), "Rock & Roll");
        assert_eq!(unescape_html(""), "");
        assert_eq!(unescape_html("&"), "&");
        assert_eq!(unescape_html("&unknown; entity"), "&unknown; entity");
    }

    #[test]
    fn handles_ampersands_without_an_entity() {
        assert_eq!(
            unescape_html("Tom & Jerry; &quot;best&quot;"),
            "Tom & Jerry; \"best\""
        );
    }
}
