//! Capture the launch options a mod installer prints.
//!
//! The HelixSR and DLSSNR-AMD installers tell the user to set Steam launch
//! options like `WINEDLLOVERRIDES="dxgi=n,b" %command%`. Copying that by hand is
//! the step that goes wrong, so [`capture`] finds every printed line containing
//! `%command%`, reads the `KEY=value` assignments ahead of it, and keeps only
//! the ones on a **fixed allowlist** (ADR-0023): `WINEDLLOVERRIDES`,
//! `VK_ADD_LAYER_PATH`, `VK_INSTANCE_LAYERS`, and any `DXVK_*` / `VKD3D_*` key.
//! Everything else is dropped and reported with a named reason, so a mod's
//! printed line can never smuggle arbitrary environment into a launch.
//!
//! Framework-free and pure: a transcript goes in, a [`CapturedEnv`] comes out,
//! so the same parser serves the fixture tests and the live installer terminal
//! (which feeds it the CLI driver's output lines).

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

/// The exact env keys a captured line may set.
pub const ALLOWED_KEYS: [&str; 3] = [
    "WINEDLLOVERRIDES",
    "VK_ADD_LAYER_PATH",
    "VK_INSTANCE_LAYERS",
];

/// The env key prefixes a captured line may set (`DXVK_HUD`, `VKD3D_DEBUG`, …).
pub const ALLOWED_PREFIXES: [&str; 2] = ["DXVK_", "VKD3D_"];

/// The token an installer prints to stand for the game's own command.
pub const COMMAND_TOKEN: &str = "%command%";

/// Whether a key may be captured.
pub fn is_allowed_key(key: &str) -> bool {
    ALLOWED_KEYS.contains(&key)
        || ALLOWED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

/// Why an assignment was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The key is not on the allowlist.
    NotAllowlisted,
}

impl RejectReason {
    /// The reason as a sentence, for the UI and a log line.
    pub const fn sentence(self) -> &'static str {
        match self {
            Self::NotAllowlisted => {
                "not on the captured-env allowlist \
                 (WINEDLLOVERRIDES, VK_ADD_LAYER_PATH, VK_INSTANCE_LAYERS, DXVK_*, VKD3D_*)"
            }
        }
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.sentence())
    }
}

/// One assignment that was captured off a `%command%` line but dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RejectedAssignment {
    /// The key that was refused.
    pub key: String,
    /// The value it carried.
    pub value: String,
    /// Why it was refused.
    pub reason: RejectReason,
}

impl fmt::Display for RejectedAssignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}=`{}` is {}", self.key, self.value, self.reason)
    }
}

/// What [`capture`] read: the allowlisted env, and the assignments it dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CapturedEnv {
    /// The allowlisted `KEY=value` pairs, keyed for a stable order.
    pub env: BTreeMap<String, String>,
    /// The assignments that were refused, in the order they were read.
    pub rejected: Vec<RejectedAssignment>,
}

impl CapturedEnv {
    /// Whether nothing was captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.env.is_empty()
    }

    /// How many keys were captured.
    #[must_use]
    pub fn len(&self) -> usize {
        self.env.len()
    }
}

/// Parse every `%command%` line in `text` into an allowlisted env map.
///
/// Only what appears *before* the `%command%` token is read: that is where the
/// env belongs, and anything after it is the game's own arguments. A token that
/// is not a `KEY=value` assignment (the prose an installer wraps around the
/// line, a bare flag) is ignored; a `KEY=value` whose key is off the allowlist
/// is recorded in [`CapturedEnv::rejected`].
pub fn capture(text: &str) -> CapturedEnv {
    let mut captured = CapturedEnv::default();
    for line in text.lines() {
        let Some(index) = line.find(COMMAND_TOKEN) else {
            continue;
        };
        for (key, value) in parse_assignments(&line[..index]) {
            if is_allowed_key(&key) {
                captured.env.insert(key, value);
            } else {
                captured.rejected.push(RejectedAssignment {
                    key,
                    value,
                    reason: RejectReason::NotAllowlisted,
                });
            }
        }
    }
    captured
}

/// Read `KEY=value` assignments from a fragment, honouring quoted values.
///
/// A value may be bare (`fps`) or double-quoted with spaces (`"dxgi=n,b"`);
/// inside quotes a backslash escapes the next character. A token without an `=`
/// is skipped, so the installer's prose around the line is harmless.
fn parse_assignments(text: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut assignments = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_whitespace() {
            index += 1;
            continue;
        }
        let key_start = index;
        while index < chars.len() && !chars[index].is_whitespace() && chars[index] != '=' {
            index += 1;
        }
        if index >= chars.len() || chars[index] != '=' {
            // A bare word, not an assignment; skip the whole token.
            continue;
        }
        let key: String = chars[key_start..index].iter().collect();
        index += 1; // the `=`
        let value = if index < chars.len() && chars[index] == '"' {
            index += 1;
            let mut value = String::new();
            while index < chars.len() {
                match chars[index] {
                    '\\' if index + 1 < chars.len() => {
                        value.push(chars[index + 1]);
                        index += 2;
                    }
                    '"' => {
                        index += 1;
                        break;
                    }
                    other => {
                        value.push(other);
                        index += 1;
                    }
                }
            }
            value
        } else {
            let value_start = index;
            while index < chars.len() && !chars[index].is_whitespace() {
                index += 1;
            }
            chars[value_start..index].iter().collect()
        };
        assignments.push((key, value));
    }
    assignments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_an_allowlisted_assignment_from_a_command_line() {
        let captured = capture("WINEDLLOVERRIDES=\"dxgi=n,b\" %command%");
        assert_eq!(
            captured.env.get("WINEDLLOVERRIDES").map(String::as_str),
            Some("dxgi=n,b")
        );
        assert!(captured.rejected.is_empty());
    }

    #[test]
    fn captures_multiple_assignments_including_the_prefixes() {
        let captured = capture(
            "VK_ADD_LAYER_PATH=\"/x/layers\" VK_INSTANCE_LAYERS=VK_LAYER_a \
             DXVK_HUD=fps VKD3D_DEBUG=warn %command%",
        );
        assert_eq!(captured.len(), 4);
        assert_eq!(captured.env["VK_ADD_LAYER_PATH"], "/x/layers");
        assert_eq!(captured.env["VK_INSTANCE_LAYERS"], "VK_LAYER_a");
        assert_eq!(captured.env["DXVK_HUD"], "fps");
        assert_eq!(captured.env["VKD3D_DEBUG"], "warn");
        assert!(captured.rejected.is_empty());
    }

    #[test]
    fn drops_an_off_list_key_with_a_named_reason() {
        let captured = capture("PROTON_USE_OPTISCALER=0 WINEDLLOVERRIDES=\"dxgi=n,b\" %command%");
        assert_eq!(
            captured.env.keys().collect::<Vec<_>>(),
            ["WINEDLLOVERRIDES"]
        );
        assert_eq!(captured.rejected.len(), 1);
        let rejected = &captured.rejected[0];
        assert_eq!(rejected.key, "PROTON_USE_OPTISCALER");
        assert_eq!(rejected.value, "0");
        assert_eq!(rejected.reason, RejectReason::NotAllowlisted);
        assert!(rejected.to_string().contains("allowlist"));
    }

    #[test]
    fn a_line_without_the_command_token_is_ignored() {
        let captured = capture("WINEDLLOVERRIDES=\"dxgi=n,b\"\njust a log line\n");
        assert!(captured.is_empty());
        assert!(captured.rejected.is_empty());
    }

    #[test]
    fn only_the_assignments_before_the_command_are_read() {
        // Game arguments after `%command%` are not environment.
        let captured = capture("%command% --windowed DXVK_HUD=fps");
        assert!(captured.is_empty());
    }

    #[test]
    fn prose_around_the_line_is_harmless() {
        let captured =
            capture("Set these Steam launch options: WINEDLLOVERRIDES=winmm=n,b %command%");
        assert_eq!(captured.env["WINEDLLOVERRIDES"], "winmm=n,b");
    }

    #[test]
    fn a_later_assignment_wins() {
        let captured = capture("DXVK_HUD=fps %command%\nDXVK_HUD=devinfo %command%");
        assert_eq!(captured.env["DXVK_HUD"], "devinfo");
    }

    #[test]
    fn an_escaped_quote_inside_a_value_is_kept() {
        let captured = capture("WINEDLLOVERRIDES=\"a=\\\"b\\\"\" %command%");
        assert_eq!(captured.env["WINEDLLOVERRIDES"], "a=\"b\"");
    }

    #[test]
    fn the_allowlist_is_exact_for_plain_keys_and_prefix_scoped_for_the_families() {
        assert!(is_allowed_key("WINEDLLOVERRIDES"));
        assert!(is_allowed_key("DXVK_HUD"));
        assert!(is_allowed_key("DXVK_"));
        assert!(is_allowed_key("VKD3D_CONFIG"));
        // A near-miss is refused.
        assert!(!is_allowed_key("DXVK"));
        assert!(!is_allowed_key("PROTON_USE_OPTISCALER"));
        assert!(!is_allowed_key("LD_PRELOAD"));
    }
}
