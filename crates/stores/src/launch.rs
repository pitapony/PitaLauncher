//! Starting an installed game through the store's own client.
//!
//! Two backends cannot answer the question the umu/Proton hand-off asks — *which
//! executable do I run?* `nile`'s library listing names none, and `gogdl` has no
//! install listing at all, so both start a game **by id**: `nile launch <id>`,
//! `gogdl launch <dir> <id> --platform windows`. Their store rows therefore keep
//! no `exe`, and a Play button that needed one could never light up.
//!
//! This module is the bridge, and it is deliberately tiny: the argv already
//! lives in each backend's `CliSpec` (`install`/`verify`/`launch` templates are
//! the same argv-only contract), so [`client_spec`] only resolves the right
//! spec, substitutes `{id}`/`{dir}`, and packages the result as the
//! [`LaunchSpec`] the hand-off takes. Nothing here runs a process: the caller
//! (`platform::launch::prepare`, then `start`) owns supervision, which is what
//! keeps a store client launch supervised exactly like a direct one.

use std::path::PathBuf;

use pitagames_core::Game;
use pitagames_core::launch::LaunchSpec;

use crate::cli::{CliCommand, CliError};
use crate::sync::Backends;

/// Whether this store's client starts a game by id rather than by executable.
///
/// This is the one predicate the GUI and the CLI ask before offering Play to a
/// row that names no `exe`: Epic reports one, and a Steam row launches through
/// Steam itself, so only GOG and Amazon need the client shape.
#[must_use]
pub const fn launches_by_id(store: pitagames_core::Store) -> bool {
    matches!(
        store,
        pitagames_core::Store::Gog | pitagames_core::Store::Amazon
    )
}

/// Why a store-client launch could not be composed.
#[derive(Debug, thiserror::Error)]
pub enum ClientLaunchError {
    #[error("{0} does not launch games by id; use the executable its listing names")]
    NotById(pitagames_core::Store),

    #[error(transparent)]
    Cli(#[from] CliError),
}

/// The argv that starts `game` through its store client.
///
/// The template owns the placeholders: `gogdl` wants the install directory for
/// `launch`, `nile` does not, and a template that needs `{dir}` fails with
/// [`CliError::Template`] when the row carries none rather than dropping the
/// argument.
pub fn client_spec(backends: &Backends, game: &Game) -> Result<LaunchSpec, ClientLaunchError> {
    let spec = match game.id.store {
        pitagames_core::Store::Gog => backends.gog.game_spec(),
        pitagames_core::Store::Amazon => backends.amazon.spec(),
        other => return Err(ClientLaunchError::NotById(other)),
    };

    let command = CliCommand::Launch {
        id: game.id.store_id.clone(),
        install_dir: game.install_dir.clone(),
    };
    let args = spec.argv(&command)?;

    Ok(LaunchSpec {
        program: PathBuf::from(&spec.program),
        args,
        cwd: game.install_dir.clone(),
        // The client's own configuration is argv, not environment; a profile's
        // environment is applied later, by `platform::launch::prepare`.
        env: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{Game, GameId, Store};
    use std::path::PathBuf;

    use crate::amazon::Amazon;
    use crate::epic::Epic;
    use crate::gog::Gog;

    fn backends() -> Backends {
        Backends {
            epic: Epic::new(),
            amazon: Amazon::new(),
            gog: Gog::new(),
        }
    }

    fn installed_game(store: Store, id: &str, dir: Option<&str>) -> Game {
        let mut game = Game::new(GameId::new(store, id), "A Game");
        game.installed = true;
        game.install_dir = dir.map(PathBuf::from);
        game
    }

    #[test]
    fn only_gog_and_amazon_launch_by_id() {
        assert!(launches_by_id(Store::Gog));
        assert!(launches_by_id(Store::Amazon));
        assert!(!launches_by_id(Store::Epic));
        assert!(!launches_by_id(Store::Steam));
        assert!(!launches_by_id(Store::Local));
    }

    #[test]
    fn gog_passes_the_install_directory_to_gogdl() {
        let game = installed_game(Store::Gog, "1207658930", Some("/games/gog/A Game"));
        let spec = client_spec(&backends(), &game).unwrap();

        assert_eq!(spec.program, PathBuf::from("gogdl"));
        assert!(
            spec.args.contains(&"launch".to_string()),
            "gogdl launch: {:?}",
            spec.args
        );
        // `gogdl launch <dir> <id> --platform windows`, after the auth path.
        let launch_at = spec.args.iter().position(|arg| arg == "launch").unwrap();
        assert_eq!(spec.args[launch_at + 1], "/games/gog/A Game");
        assert_eq!(spec.args[launch_at + 2], "1207658930");
        assert_eq!(spec.args[launch_at + 3], "--platform");
        assert_eq!(spec.args[launch_at + 4], "windows");
        assert_eq!(
            spec.cwd.as_deref(),
            Some(std::path::Path::new("/games/gog/A Game"))
        );
    }

    #[test]
    fn gog_without_an_install_directory_fails_before_running() {
        let game = installed_game(Store::Gog, "1207658930", None);
        let error = client_spec(&backends(), &game).unwrap_err();
        assert!(
            matches!(error, ClientLaunchError::Cli(CliError::Template { .. })),
            "{error}"
        );
    }

    #[test]
    fn amazon_launches_by_id_alone() {
        let game = installed_game(Store::Amazon, "amzn1.example", None);
        let spec = client_spec(&backends(), &game).unwrap();

        assert_eq!(spec.program, PathBuf::from("nile"));
        assert_eq!(
            spec.args,
            vec!["launch".to_string(), "amzn1.example".to_string()]
        );
        assert!(spec.cwd.is_none());
        assert!(spec.env.is_empty());
    }

    #[test]
    fn a_store_that_launches_by_executable_is_refused() {
        let game = installed_game(Store::Epic, "Gizmo", Some("/games/epic/Gizmo"));
        let error = client_spec(&backends(), &game).unwrap_err();
        assert!(
            matches!(error, ClientLaunchError::NotById(Store::Epic)),
            "{error}"
        );
    }
}
