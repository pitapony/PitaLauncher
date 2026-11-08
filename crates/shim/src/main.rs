//! `pitagames-shim` — the executable Steam runs instead of the game.
//!
//! A Steam game's launch options are set to:
//!
//! ```text
//! pitagames-shim <appid> %command%
//! ```
//!
//! Steam substitutes `%command%` with the game's command line. This shim
//! tokenizes it back into an argv (quotes intact), adds the launcher's
//! environment (`DXVK_HUD=fps` in M0, unless the user already set it), and
//! runs the game, forwarding its exit code.
//!
//! M0 scope: tokenization, env, exec, exit-code forwarding. Signal
//! forwarding, the graphics-profile env compiler and the `PITAGAMES_SESSION`
//! gamescope rules arrive in later milestones.

use std::env;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitCode};

use pitagames_core::{LaunchSpec, TokenizeError, tokenize_command};

/// Why the shim could not turn its arguments into a launch.
#[derive(Debug, thiserror::Error)]
pub enum ShimError {
    #[error("first argument must be the numeric Steam appid, found `{0}`")]
    BadAppId(String),

    #[error("no game command given: %command% was empty")]
    EmptyCommand,

    #[error("could not parse the game command: {0}")]
    Tokenize(#[from] TokenizeError),

    #[error("failed to start {}: {}", .0.display(), .1)]
    Spawn(std::path::PathBuf, #[source] std::io::Error),

    #[error("failed to wait for the game: {source}")]
    Wait {
        #[source]
        source: std::io::Error,
    },
}

/// Turn `pitagames-shim <appid> <command line>` into a [`LaunchSpec`].
pub fn build_launch(app_id: u32, command: &str) -> Result<LaunchSpec, ShimError> {
    let argv = tokenize_command(command)?;
    let Some(program) = argv.first().cloned() else {
        return Err(ShimError::EmptyCommand);
    };

    let mut spec = LaunchSpec::new(program);
    spec.args = argv[1..].to_vec();
    spec.env
        .push(("PITAGAMES_APPID".to_string(), app_id.to_string()));
    Ok(spec)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    match run(&args) {
        Ok(code) => ExitCode::from(code as u8),
        Err(error) => {
            eprintln!("pitagames-shim: {error}");
            eprintln!();
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<i32, ShimError> {
    if args.len() > 1 && matches!(args[1].as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(0);
    }

    let app_arg = args
        .get(1)
        .ok_or_else(|| ShimError::BadAppId("<missing>".to_string()))?;
    let app_id = app_arg
        .parse::<u32>()
        .map_err(|_| ShimError::BadAppId(app_arg.clone()))?;

    // `%command%` arrives as one argument when Steam keeps the quotes and as
    // several words when it does not; joining on a space reproduces the line
    // either way, because the tokenizer only splits on unquoted spaces.
    let command = args[2..].join(" ");
    let mut spec = build_launch(app_id, &command)?;

    // M0: show the DXVK FPS HUD. A value the user already set is never
    // clobbered, whether in the environment or in the game's own options.
    if spec.env_get("DXVK_HUD").is_none() && env::var_os("DXVK_HUD").is_none() {
        spec.env.push(("DXVK_HUD".to_string(), "fps".to_string()));
    }

    let mut child = Command::new(&spec.program)
        .args(&spec.args)
        .envs(
            spec.env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .spawn()
        .map_err(|source| ShimError::Spawn(spec.program.clone(), source))?;

    let status = child.wait().map_err(|source| ShimError::Wait { source })?;
    if status.success() {
        return Ok(0);
    }

    #[cfg(unix)]
    if let Some(signal) = status.signal() {
        return Ok(128 + signal);
    }

    Ok(status.code().unwrap_or(1))
}

const USAGE: &str = "pitagames-shim — PitaGames launch shim for Steam

USAGE:
    pitagames-shim <appid> <game command line>

Steam's launch options should be:

    pitagames-shim <appid> %command%

The shim tokenizes the substituted command line, applies the launcher
environment (currently DXVK_HUD=fps unless already set) and runs the game,
forwarding its exit code.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_spec_from_a_quoted_command() {
        let spec = build_launch(
            570,
            r#""/games/steamapps/common/dota 2/linuxsteam.so" -dx12"#,
        )
        .unwrap();

        assert_eq!(
            spec.program.to_str().unwrap(),
            "/games/steamapps/common/dota 2/linuxsteam.so"
        );
        assert_eq!(spec.args, vec!["-dx12"]);
        assert_eq!(spec.env_get("PITAGAMES_APPID"), Some("570"));
    }

    #[test]
    fn rejects_an_empty_command() {
        assert!(matches!(
            build_launch(570, "   "),
            Err(ShimError::EmptyCommand)
        ));
    }

    #[test]
    fn forwards_tokenizer_errors() {
        assert!(matches!(
            build_launch(570, r#""unterminated"#),
            Err(ShimError::Tokenize(_))
        ));
    }
}
