//! `pitagames-shim` — the executable Steam runs instead of the game.
//!
//! A Steam game's launch options are set to:
//!
//! ```text
//! pitagames-shim <appid> %command%
//! ```
//!
//! Steam substitutes `%command%` with the game's command line. This shim
//! tokenizes it back into an argv (quotes intact), applies the per-game profile
//! compiled into `profiles/steam-<appid>.json` (environment and wrapper chain),
//! runs the game, and forwards its exit code.
//!
//! A missing or corrupt profile is never fatal: the shim logs a warning and
//! starts the game with the M0 default environment, exactly as before profiles
//! existed. `--explain` prints the same composition as a copyable command for
//! the GUI's transparency panel.
//!
//! No shell is involved anywhere: wrappers, the game and the environment are
//! argv and key/value vectors.

use std::env;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use pitagames_core::{
    GameId, LaunchSpec, ShimProfile, Store, TokenizeError, flatten_wrappers, load_shim_profile,
    tokenize_command,
};

/// Environment value proving the shim ran; kept from M0.
const APPID_ENV: &str = "PITAGAMES_APPID";

/// The profile name, so a log or `dxvk.conf` can tell which profile launched.
const PROFILE_ENV: &str = "PITAGAMES_PROFILE";

/// M0's proof-of-life HUD, applied only when no profile file exists.
const FALLBACK_HUD: (&str, &str) = ("DXVK_HUD", "fps");

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
    Spawn(PathBuf, #[source] std::io::Error),

    #[error("failed to wait for the game: {source}")]
    Wait {
        #[source]
        source: std::io::Error,
    },
}

/// Turn `pitagames-shim <appid> <command line>` into a [`LaunchSpec`].
///
/// The profile (when present) supplies the environment; `app_id` is always
/// exported so the game and our logs can identify themselves.
pub fn build_launch(
    app_id: u32,
    command: &str,
    profile: Option<&ShimProfile>,
) -> Result<LaunchSpec, ShimError> {
    let argv = tokenize_command(command)?;
    let Some(program) = argv.first().cloned() else {
        return Err(ShimError::EmptyCommand);
    };

    let mut spec = LaunchSpec::new(program);
    spec.args = argv[1..].to_vec();
    spec.env = launch_env(app_id, profile);
    Ok(spec)
}

/// The environment additions, in application order (later wins).
///
/// With a profile, this is exactly what the compiler produced. Without one it is
/// M0's behaviour: identify the appid and show the FPS HUD.
fn launch_env(app_id: u32, profile: Option<&ShimProfile>) -> Vec<(String, String)> {
    let mut env = vec![(APPID_ENV.to_string(), app_id.to_string())];

    match profile {
        Some(profile) => {
            env.push((PROFILE_ENV.to_string(), profile.profile.clone()));
            env.extend(profile.env.iter().cloned());
        }
        None => {
            if env::var_os(FALLBACK_HUD.0).is_none() {
                env.push((FALLBACK_HUD.0.to_string(), FALLBACK_HUD.1.to_string()));
            }
        }
    }

    env
}

/// Flatten the wrapper chain and the game command into one `program + args`.
///
/// The composition itself lives in `core::launch` so the GUI preview and the
/// shim exec cannot disagree.
fn flatten(wrappers: &[pitagames_core::WrapperStep], spec: &LaunchSpec) -> (PathBuf, Vec<String>) {
    let (program, args) = flatten_wrappers(wrappers, &spec.program.to_string_lossy(), &spec.args);
    (PathBuf::from(program), args)
}

/// Load the compiled profile for a Steam appid, warning (never failing) on error.
fn load_profile(app_id: u32) -> Option<ShimProfile> {
    let dir = pitagames_core::paths::profiles_dir()?;
    let game = GameId::new(Store::Steam, app_id.to_string());
    match load_shim_profile(&dir, &game) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("pitagames-shim: ignoring profile for {app_id}: {error}");
            None
        }
    }
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

    let (explain, rest) = match args.get(1).map(String::as_str) {
        Some("--explain") => (true, &args[2..]),
        _ => (false, &args[1..]),
    };

    let app_arg = rest
        .first()
        .ok_or_else(|| ShimError::BadAppId("<missing>".to_string()))?;
    let app_id = app_arg
        .parse::<u32>()
        .map_err(|_| ShimError::BadAppId(app_arg.clone()))?;

    // `%command%` arrives as one argument when Steam keeps the quotes and as
    // several words when it does not; joining on a space reproduces the line
    // either way, because the tokenizer only splits on unquoted spaces.
    let command = rest[1..].join(" ");
    let profile = load_profile(app_id);
    let spec = build_launch(app_id, &command, profile.as_ref())?;

    if explain {
        explain_launch(app_id, profile.as_ref(), &spec);
        return Ok(0);
    }

    let (program, args) = flatten(&profile.map(|p| p.wrappers).unwrap_or_default(), &spec);
    log_launch(app_id, &program, &args, &spec.env);

    let mut child = Command::new(&program)
        .args(&args)
        .envs(
            spec.env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .spawn()
        .map_err(|source| ShimError::Spawn(program.clone(), source))?;

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

/// Print the composition as a copyable command plus the environment.
fn explain_launch(app_id: u32, profile: Option<&ShimProfile>, spec: &LaunchSpec) {
    println!("PitaGames launch for appid {app_id}");
    match profile {
        Some(profile) => {
            println!("Profile:      {}", profile.profile);
            match &profile.proton {
                Some(proton) => match &proton.install {
                    Some(install) => println!(
                        "Proton:       {} ({}) {}",
                        install.name,
                        install.source,
                        install.path.display()
                    ),
                    None => println!("Proton:       Steam decides ({:?})", proton.reference),
                },
                None => println!("Proton:       Steam decides"),
            }
        }
        None => println!("Profile:      none written (M0 defaults)"),
    }

    println!("Environment:");
    if spec.env.is_empty() {
        println!("  (none)");
    }
    for (key, value) in &spec.env {
        println!("  {key}={value}");
    }

    let wrappers = profile.map(|p| p.wrappers.as_slice()).unwrap_or(&[]);
    let (program, args) = flatten(wrappers, spec);
    let command =
        pitagames_core::render_shell_command(&spec.env, &program.to_string_lossy(), &args);

    println!();
    println!("Command:");
    println!("  {command}");
}

/// Append one launch line to the log directory; never fails the launch.
fn log_launch(app_id: u32, program: &Path, args: &[String], env: &[(String, String)]) {
    let Some(dir) = pitagames_core::paths::logs_dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    let mut line = format!(
        "{} appid={app_id} argv=[{}]",
        time_stamp(),
        std::iter::once(program.to_string_lossy().into_owned())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    );
    for (key, value) in env {
        line.push(' ');
        line.push_str(&format!("{key}={value}"));
    }
    line.push('\n');

    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("shim.log"))
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// A minimal UTC timestamp, avoiding a `time` dependency in the shim.
fn time_stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

const USAGE: &str = "pitagames-shim — PitaGames launch shim for Steam

USAGE:
    pitagames-shim <appid> <game command line>
    pitagames-shim --explain <appid> [game command line]

Steam's launch options should be:

    pitagames-shim <appid> %command%

The shim tokenizes the substituted command line, applies
profiles/steam-<appid>.json (environment and wrapper chain) if it exists, runs
the game and forwards its exit code. Without a profile it falls back to
DXVK_HUD=fps, M0's proof-of-life environment. A missing or corrupt profile is a
warning, never a failed launch.

--explain prints the resolved environment, Proton choice and command without
running anything.";

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{ProtonRef, ShimProton, ShimProtonInstall};

    fn profile() -> ShimProfile {
        let mut profile = ShimProfile::new(GameId::new(Store::Steam, "620"), "Default");
        profile.env = vec![
            ("DXVK_HUD".to_string(), "fps".to_string()),
            ("PROTON_LOG".to_string(), "1".to_string()),
        ];
        profile
    }

    #[test]
    fn builds_spec_from_a_quoted_command() {
        let spec = build_launch(
            570,
            r#""/games/steamapps/common/dota 2/linuxsteam.so" -dx12"#,
            None,
        )
        .unwrap();

        assert_eq!(
            spec.program.to_str().unwrap(),
            "/games/steamapps/common/dota 2/linuxsteam.so"
        );
        assert_eq!(spec.args, vec!["-dx12"]);
        assert_eq!(spec.env_get(APPID_ENV), Some("570"));
    }

    #[test]
    fn a_profile_replaces_the_m0_fallback_env() {
        let profile = profile();
        let spec = build_launch(620, "game", Some(&profile)).unwrap();
        assert_eq!(spec.env_get(APPID_ENV), Some("620"));
        assert_eq!(spec.env_get(PROFILE_ENV), Some("Default"));
        assert_eq!(spec.env_get("DXVK_HUD"), Some("fps"));
        assert_eq!(spec.env_get("PROTON_LOG"), Some("1"));
    }

    #[test]
    fn without_a_profile_the_m0_hud_is_kept() {
        let spec = build_launch(620, "game", None).unwrap();
        assert_eq!(spec.env_get(APPID_ENV), Some("620"));
        assert_eq!(spec.env_get("DXVK_HUD"), Some("fps"));
        assert_eq!(spec.env_get(PROFILE_ENV), None);
    }

    #[test]
    fn rejects_an_empty_command() {
        assert!(matches!(
            build_launch(570, "   ", None),
            Err(ShimError::EmptyCommand)
        ));
    }

    #[test]
    fn forwards_tokenizer_errors() {
        assert!(matches!(
            build_launch(570, r#""unterminated"#, None),
            Err(ShimError::Tokenize(_))
        ));
    }

    #[test]
    fn wrappers_flatten_outermost_first() {
        use pitagames_core::WrapperStep;

        let wrappers = vec![
            WrapperStep {
                program: PathBuf::from("gamescope"),
                args: vec!["--".to_string()],
            },
            WrapperStep {
                program: PathBuf::from("gamemoderun"),
                args: vec![],
            },
        ];
        let mut spec = LaunchSpec::new("/games/Game/game.exe");
        spec.args.push("-dx12".to_string());

        let (program, args) = flatten(&wrappers, &spec);
        assert_eq!(program, PathBuf::from("gamescope"));
        assert_eq!(
            args,
            vec![
                "--".to_string(),
                "gamemoderun".to_string(),
                "/games/Game/game.exe".to_string(),
                "-dx12".to_string(),
            ]
        );
    }

    #[test]
    fn without_wrappers_the_game_runs_directly() {
        let mut spec = LaunchSpec::new("/games/Game/game.exe");
        spec.args.push("-dx12".to_string());
        let (program, args) = flatten(&[], &spec);
        assert_eq!(program, PathBuf::from("/games/Game/game.exe"));
        assert_eq!(args, vec!["-dx12".to_string()]);
    }

    #[test]
    fn the_proton_record_is_optional_and_renders() {
        let mut profile = profile();
        profile.proton = Some(ShimProton {
            reference: ProtonRef::CachyOs,
            install: Some(ShimProtonInstall {
                name: "proton-cachyos".into(),
                path: PathBuf::from("/p/proton-cachyos"),
                kind: "proton-cachyos".into(),
                version: None,
                source: "system".into(),
            }),
        });
        let spec = build_launch(620, "game", Some(&profile)).unwrap();
        // Renders without panicking; the text itself is cosmetic.
        explain_launch(620, Some(&profile), &spec);
    }
}
