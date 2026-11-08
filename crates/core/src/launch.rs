//! Launch composition: argv-only.
//!
//! `LaunchSpec` is the single artifact shared by the GUI, the CLI and
//! `pitagames-shim`. A spec is always an argument vector plus ordered
//! environment variables; nothing downstream may turn it into a shell string
//! or run it through a shell.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Everything needed to start a game process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LaunchSpec {
    /// The program to run (argv[0]).
    pub program: PathBuf,
    /// Arguments after argv[0].
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory, or the launcher's current directory when `None`.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Ordered environment additions; later entries override earlier ones.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

impl LaunchSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        }
    }

    /// The full argument vector, argv[0] first.
    pub fn argv(&self) -> Vec<&str> {
        let mut out = Vec::with_capacity(self.args.len() + 1);
        out.push(self.program.to_str().unwrap_or_default());
        out.extend(self.args.iter().map(String::as_str));
        out
    }

    /// Look up an environment value; the last definition wins.
    pub fn env_get(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

/// A `LaunchSpec` that asks the running Steam client to start a game.
///
/// This is the v1 way to run a Steam game: Steam builds the real command
/// line itself (optionally through `pitagames-shim <appid> %command%`), so
/// the launcher never needs to know the execution path.
#[must_use]
pub fn steam_run(app_id: &str) -> LaunchSpec {
    let mut spec = LaunchSpec::new("steam");
    spec.args.push(format!("steam://rungameid/{app_id}"));
    spec
}

/// A `LaunchSpec` that asks Steam to install (or update) an app.
///
/// Steam owns the download: the launcher hands the request over and then polls
/// the appmanifest, per the plan's "no native Steam downloader" rule.
#[must_use]
pub fn steam_install(app_id: &str) -> LaunchSpec {
    let mut spec = LaunchSpec::new("steam");
    spec.args.push(format!("steam://install/{app_id}"));
    spec
}

/// The executable name Steam runs when a game is routed through the shim.
pub const SHIM_PROGRAM: &str = "pitagames-shim";

/// Compose the launch-options string that routes a Steam game through the shim.
///
/// Steam expands `%command%` to the game's command line. Environment
/// assignments and other options stay in front of it, so the shim goes
/// immediately before `%command%`:
///
/// ```text
/// DXVK_HUD=fps %command% -novid   ->   DXVK_HUD=fps pitagames-shim 620 %command% -novid
/// ```
///
/// Options without `%command%` are treated as arguments after it. An existing
/// shim invocation is replaced, never nested, so applying this twice is a
/// no-op.
#[must_use]
pub fn shim_launch_options(app_id: &str, existing: &str) -> String {
    let shim = format!("{SHIM_PROGRAM} {app_id}");
    let cleaned = strip_shim_launch_options(existing);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return format!("{shim} %command%");
    }

    let Some(position) = cleaned.find("%command%") else {
        return format!("{shim} %command% {cleaned}");
    };

    let prefix = cleaned[..position].trim_end();
    let suffix = &cleaned[position..];
    if prefix.is_empty() {
        format!("{shim} {suffix}")
    } else {
        format!("{prefix} {shim} {suffix}")
    }
}

/// Remove a `pitagames-shim <appid>` invocation from a launch-options string.
///
/// Used by the uninstall path and by [`shim_launch_options`] to avoid nesting
/// two shims.
#[must_use]
pub fn strip_shim_launch_options(existing: &str) -> String {
    let mut out = String::with_capacity(existing.len());
    let mut rest = existing;

    while let Some((start, end)) = find_shim_token(rest) {
        out.push_str(&rest[..start]);
        if out.ends_with(|character: char| character.is_whitespace()) {
            rest = rest[end..].trim_start_matches([' ', '\t']);
        } else {
            rest = &rest[end..];
        }
    }

    out.push_str(rest);
    out.trim().to_string()
}

fn find_shim_token(text: &str) -> Option<(usize, usize)> {
    let mut search_from = 0;

    while let Some(offset) = text[search_from..].find(SHIM_PROGRAM) {
        let start = search_from + offset;
        let after = start + SHIM_PROGRAM.len();

        let boundary_before = start == 0
            || text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let boundary_after = after == text.len()
            || text[after..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace);
        if !(boundary_before && boundary_after) {
            search_from = after;
            continue;
        }

        let digits_start = skip_blanks(text, after);
        let mut cursor = digits_start;
        while text.as_bytes().get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == digits_start {
            search_from = after;
            continue;
        }

        return Some((start, skip_blanks(text, cursor)));
    }

    None
}

fn skip_blanks(text: &str, mut cursor: usize) -> usize {
    while matches!(text.as_bytes().get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    cursor
}

/// A syntax problem in a `%command%` string, with a byte position.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} (byte {position})")]
pub struct TokenizeError {
    pub position: usize,
    message: String,
}

impl TokenizeError {
    fn new(position: usize, message: impl Into<String>) -> Self {
        Self {
            position,
            message: message.into(),
        }
    }
}

/// Split a Steam `%command%` string into an argv.
///
/// Quote rules (a superset of what Steam writes on Linux):
///
/// - double quotes group one token; `\"` and `\\` escape inside them, and
///   the Valve `""` idiom means one literal `"`;
/// - single quotes group one literal token; `''` means one literal `'`;
/// - outside quotes, a backslash quotes the next character.
///
/// Empty or whitespace-only input yields zero tokens; callers decide what
/// that means (the shim treats it as "Steam gave us no command").
pub fn tokenize_command(line: &str) -> Result<Vec<String>, TokenizeError> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut state: QuoteState = QuoteState::Bare;

    let mut i = 0;
    while i < chars.len() {
        let open = i;
        let c = chars[i];

        match state {
            QuoteState::Bare => match c {
                ' ' | '\t' | '\n' | '\r' => {
                    if started {
                        tokens.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                '"' => {
                    state = QuoteState::Double { open };
                    started = true;
                }
                '\'' => {
                    state = QuoteState::Single { open };
                    started = true;
                }
                '\\' => {
                    let next = chars
                        .get(i + 1)
                        .ok_or_else(|| TokenizeError::new(open, "trailing backslash"))?;
                    current.push(*next);
                    started = true;
                    i += 1;
                }
                other => {
                    current.push(other);
                    started = true;
                }
            },
            QuoteState::Double { open } => match c {
                '\\' => {
                    let next = chars.get(i + 1).ok_or_else(|| {
                        TokenizeError::new(open, "unterminated escape in double quotes")
                    })?;
                    if *next == '"' || *next == '\\' {
                        // `\"` and `\\` are the escapes Valve writes.
                        current.push(*next);
                        i += 1;
                    } else {
                        // POSIX sh: a backslash before a plain character stays
                        // literal, so `C:\Games` survives.
                        current.push('\\');
                    }
                }
                '"' => {
                    if chars.get(i + 1).copied() == Some('"') {
                        current.push('"');
                        i += 1;
                    } else {
                        state = QuoteState::Bare;
                    }
                }
                other => current.push(other),
            },
            QuoteState::Single { open: _ } => match c {
                '\'' => {
                    if chars.get(i + 1).copied() == Some('\'') {
                        current.push('\'');
                        i += 1;
                    } else {
                        state = QuoteState::Bare;
                    }
                }
                other => current.push(other),
            },
        }

        i += 1;
    }

    if let QuoteState::Double { open } | QuoteState::Single { open } = state {
        return Err(TokenizeError::new(open, "unterminated quote"));
    }

    if started {
        tokens.push(current);
    }

    Ok(tokens)
}

#[derive(Clone, Copy)]
enum QuoteState {
    Bare,
    Double { open: usize },
    Single { open: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_words_split_on_whitespace() {
        assert_eq!(
            tokenize_command("a b\tc\nd").unwrap(),
            vec!["a", "b", "c", "d"]
        );
    }

    #[test]
    fn empty_and_whitespace_only_yield_no_tokens() {
        assert_eq!(tokenize_command("").unwrap(), Vec::<String>::new());
        assert_eq!(tokenize_command("  \t \n").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn double_quotes_group_a_token() {
        assert_eq!(
            tokenize_command(r#"dota2.exe -dx12 "with space" flag"#).unwrap(),
            vec!["dota2.exe", "-dx12", "with space", "flag"]
        );
    }

    #[test]
    fn backslash_survives_before_plain_characters_in_double_quotes() {
        // POSIX sh semantics: only \" and \\ are escapes inside double quotes.
        assert_eq!(
            tokenize_command(r#""C:\games\dota2.exe" "#).unwrap(),
            vec!["C:\\games\\dota2.exe"]
        );
        assert_eq!(
            tokenize_command(r#""say \"hi\"""#).unwrap(),
            vec!["say \"hi\""]
        );
    }

    #[test]
    fn valve_double_quote_idiom() {
        // The launch-options idiom: "" is one literal quote.
        assert_eq!(
            tokenize_command(r#""C:\"Games""#).unwrap(),
            vec!["C:\"Games"]
        );
    }

    #[test]
    fn single_quotes_are_literal() {
        // POSIX sh: outside quotes a backslash quotes the next character and
        // is consumed, so Steam's `\'` becomes one literal quote.
        assert_eq!(tokenize_command("'a b' c\\'d").unwrap(), vec!["a b", "c'd"]);
        assert_eq!(tokenize_command("'it''s'").unwrap(), vec!["it's"]);
    }

    #[test]
    fn backslash_outside_quotes_quoting_next_char() {
        assert_eq!(tokenize_command("a\\ b").unwrap(), vec!["a b"]);
    }

    #[test]
    fn unterminated_quotes_report_the_opening_position() {
        let error = tokenize_command(r#"ok "abc"#).unwrap_err();
        assert_eq!(error.position, 3);
        assert!(error.to_string().contains("unterminated quote"));

        let error = tokenize_command(r#"ok 'abc"#).unwrap_err();
        assert_eq!(error.position, 3);
    }

    #[test]
    fn trailing_backslash_is_an_error() {
        let error = tokenize_command("a\\").unwrap_err();
        assert!(error.to_string().contains("trailing backslash"));
    }

    #[test]
    fn launch_spec_lists_the_full_argv() {
        let mut spec =
            LaunchSpec::new("/home/u/.local/share/Steam/steamapps/common/Dota 2/linuxsteam.so");
        spec.args.push("--dx12".into());
        spec.env.push(("DXVK_HUD".into(), "fps".into()));

        assert_eq!(
            spec.argv()[0],
            "/home/u/.local/share/Steam/steamapps/common/Dota 2/linuxsteam.so"
        );
        assert_eq!(spec.argv().len(), 2);
        assert_eq!(spec.env_get("DXVK_HUD"), Some("fps"));
        assert_eq!(spec.env_get("NOPE"), None);
    }

    #[test]
    fn launch_spec_round_trips_through_json() {
        let mut spec = LaunchSpec::new("/bin/game");
        spec.args.push("-v".into());
        spec.cwd = Some("/tmp".into());
        spec.env.push(("A".into(), "1".into()));
        spec.env.push(("A".into(), "2".into()));

        let json = serde_json::to_string(&spec).unwrap();
        let parsed: LaunchSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, spec);
        assert_eq!(parsed.env_get("A"), Some("2"));
    }

    #[test]
    fn steam_run_is_argv_only() {
        let spec = steam_run("440");
        assert_eq!(spec.program.to_str(), Some("steam"));
        assert_eq!(spec.argv(), ["steam", "steam://rungameid/440"]);
        assert!(spec.env.is_empty() && spec.cwd.is_none());
    }

    #[test]
    fn steam_install_uses_the_install_url_and_no_shell() {
        let spec = steam_install("620");
        assert_eq!(spec.program.to_str(), Some("steam"));
        assert_eq!(spec.argv(), ["steam", "steam://install/620"]);
        assert!(spec.env.is_empty() && spec.cwd.is_none());
    }

    #[test]
    fn shim_options_are_added_before_the_command() {
        assert_eq!(
            shim_launch_options("620", ""),
            "pitagames-shim 620 %command%"
        );
        assert_eq!(
            shim_launch_options("620", "   "),
            "pitagames-shim 620 %command%"
        );
        assert_eq!(
            shim_launch_options("620", "%command% -novid"),
            "pitagames-shim 620 %command% -novid"
        );
        assert_eq!(
            shim_launch_options("620", "DXVK_HUD=fps %command% -novid"),
            "DXVK_HUD=fps pitagames-shim 620 %command% -novid"
        );
        // Without %command% the existing words become game arguments.
        assert_eq!(
            shim_launch_options("620", "-novid"),
            "pitagames-shim 620 %command% -novid"
        );
    }

    #[test]
    fn composing_the_shim_twice_is_a_no_op() {
        let once = shim_launch_options("620", "DXVK_HUD=fps %command% -novid");
        assert_eq!(shim_launch_options("620", &once), once);
    }

    #[test]
    fn an_existing_shim_is_replaced_not_nested() {
        assert_eq!(
            shim_launch_options("620", "pitagames-shim 440 %command% -novid"),
            "pitagames-shim 620 %command% -novid"
        );
        assert_eq!(
            shim_launch_options("620", "DXVK_HUD=fps pitagames-shim 440 %command%"),
            "DXVK_HUD=fps pitagames-shim 620 %command%"
        );
    }

    #[test]
    fn stripping_the_shim_leaves_the_original_options() {
        assert_eq!(
            strip_shim_launch_options("pitagames-shim 620 %command% -novid"),
            "%command% -novid"
        );
        assert_eq!(
            strip_shim_launch_options("DXVK_HUD=fps pitagames-shim 620 %command%"),
            "DXVK_HUD=fps %command%"
        );
        assert_eq!(strip_shim_launch_options("%command%"), "%command%");
        // A lookalike program name is not a shim invocation.
        assert_eq!(
            strip_shim_launch_options("/usr/bin/pitagames-shim 620 %command%"),
            "/usr/bin/pitagames-shim 620 %command%"
        );
    }
}
