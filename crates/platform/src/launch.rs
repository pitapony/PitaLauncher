//! The umu/Proton launch hand-off and process supervision.
//!
//! Steam launches its own games through `steam://rungameid/<appid>`, so it never
//! asks this module for anything. Every other store — Epic, Amazon, GOG, an
//! imported Heroic/Lutris/Bottles entry — has a directory and (maybe) an
//! executable, and needs the same three answers: which runtime, which prefix,
//! and what argv. That is [`prepare`]:
//!
//! ```text
//! Windows game   umu-run <exe>          GAMEID/PROTONPATH/WINEPREFIX/STORE
//! native game    <exe>                  the diagnostic environment only
//! store client   gogdl launch …/nile …  the profile env minus umu's keys
//! ```
//!
//! The third shape exists because two backends cannot name an executable at all
//! — `nile` has none in its listing and `gogdl` has no install listing — so
//! their clients start the game by id. The argv for that shape is not composed
//! here: `stores` owns the templates and hands in a plain [`LaunchSpec`] through
//! [`Handoff::client`], while this module keeps the supervision identical to a
//! direct launch. A store client owns its own runtime and prefix, which is why
//! that shape carries no Proton and no `WINEPREFIX`; the wrapper chain and the
//! profile's environment still apply, because they are the launcher's
//! transparency promise either way.
//!
//! The environment is the compiled [`ShimProfile`] (the file
//! `pitagames-shim` reads for a Steam game) plus the umu values the per-game
//! prefix owns. Wrapper chains flatten in front of `umu-run`, exactly as they do
//! in front of `%command%`, so `gamescope -- gamemoderun umu-run …` is still
//! `program + args` and never a shell string.
//!
//! [`start`] is the supervision half: a new session (its own process group), the
//! raw output teed to one log file, exit code and wall-clock duration. A game
//! gets killed through [`Session::cancel`] / [`terminate`] — `SIGTERM` to the
//! whole group, then `SIGKILL` after a grace period — because a launcher that
//! crashes while a game runs would otherwise leave a `gamescope` tree behind.
//!
//! Nothing here reads a store's CLI: the caller (the GUI, the CLI, a test)
//! supplies the [`Game`] row, the resolved [`ProtonInstall`] and the prefix
//! root, which is what keeps this module framework-free and testable with a
//! `#!/bin/sh` fake instead of a real game.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use pitagames_core::launch::{LaunchSpec, flatten_wrappers, render_shell_command};
use pitagames_core::profile::WrapperStep;
use pitagames_core::store::{Game, GameId, Store};
use pitagames_core::{ShimProfile, paths};
use serde::Serialize;

use crate::prefix;
use crate::proton::ProtonInstall;

/// The launcher that runs a Windows game outside Steam.
pub const UMU_RUN: &str = "umu-run";

/// How long a cancelled game gets between `SIGTERM` and `SIGKILL`.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(10);

/// The tag a store backend uses for a Windows build (see `stores::epic`).
pub const WINDOWS_TAG: &str = "platform:windows";

/// Which runtime a prepared launch ended up with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    /// A Windows build through `umu-run` (Proton + a per-game prefix).
    Umu,
    /// A native Linux executable, run directly with the wrapper chain.
    Native,
    /// The store's own client starts the game by id (`gogdl launch`,
    /// `nile launch`), because its listing names no executable.
    StoreClient,
}

impl LaunchMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Umu => "umu",
            Self::Native => "native",
            Self::StoreClient => "client",
        }
    }

    /// Whether the launcher owns the prefix (the store client owns its own).
    #[must_use]
    pub const fn uses_prefix(self) -> bool {
        matches!(self, Self::Umu)
    }
}

/// Everything the hand-off needs that is not in the game row.
pub struct Handoff<'a> {
    /// The store's own row: id, title, install directory and executable.
    pub game: &'a Game,
    /// The compiled profile (`profiles/<store>-<id>.json`), when one exists.
    /// Its environment and wrappers are applied; the shim reads the same file.
    pub runtime: Option<&'a ShimProfile>,
    /// The build [`ProtonRef`](pitagames_core::ProtonRef) resolved to. Required
    /// for a Windows game, unused for a native one.
    pub proton: Option<&'a ProtonInstall>,
    /// Where the per-game prefix lives (see [`crate::prefix::default_root`]).
    pub prefix_root: &'a Path,
    /// `PITAGAMES_SESSION=1`: the console session already owns a compositor, so
    /// a gamescope wrapper in the profile is dropped.
    pub session: bool,
    /// How the store's own client starts a game whose row names no executable
    /// (`gogdl launch <dir> <id>`, `nile launch <id>`). Ignored when the row has
    /// one; [`prepare`] composes no argv of its own here, it only prepends the
    /// wrapper chain and applies the profile.
    pub client: Option<&'a LaunchSpec>,
}

/// One launch, composed and ready to exec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Prepared {
    pub mode: LaunchMode,
    /// The argv-only launch artifact shared with the GUI's transparency panel.
    pub spec: LaunchSpec,
    /// The per-game Wine prefix; `None` for a native or store-client launch.
    pub prefix: Option<PathBuf>,
    /// The umu/app key (`<store>-<id>`), which is also the `GAMEID`.
    pub game_key: String,
}

impl Prepared {
    /// The command as a POSIX-quoted string, for display and copy.
    #[must_use]
    pub fn render(&self) -> String {
        render_shell_command(
            &self.spec.env,
            self.spec.program.to_str().unwrap_or_default(),
            &self.spec.args,
        )
    }

    /// The environment additions as a map (later entries already won).
    #[must_use]
    pub fn env_map(&self) -> BTreeMap<&str, &str> {
        let mut map = BTreeMap::new();
        for (key, value) in &self.spec.env {
            map.insert(key.as_str(), value.as_str());
        }
        map
    }
}

/// Why a game cannot be handed off.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    #[error("Steam launches its own games; the hand-off is for the other stores")]
    SteamGame,

    #[error("{title} is not installed")]
    NotInstalled { title: String },

    #[error("{title} has no launch executable; sync the store's library to refresh its row")]
    NoExecutable { title: String },

    #[error("{title}'s executable {path} is not on disk; re-run the store's verify")]
    MissingExecutable { title: String, path: PathBuf },

    #[error("{title} is a Windows build and no Proton build was resolved for it")]
    NoProton { title: String },
}

/// Compose one non-Steam launch.
///
/// Pure apart from one `stat` on the executable: it asks whether the path the
/// store's listing named is still there, because a repaired or moved install is
/// the most common reason a launch fails, and "umu-run: no such file" is a much
/// worse error than naming the path.
///
/// A row with no executable is not necessarily a refusal: when the caller
/// supplies a [`Handoff::client`] spec, the store's own client starts the game
/// by id instead. A row with neither is [`LaunchError::NoExecutable`].
pub fn prepare(handoff: &Handoff<'_>) -> Result<Prepared, LaunchError> {
    let game = handoff.game;
    if game.id.store == Store::Steam {
        return Err(LaunchError::SteamGame);
    }
    if !game.installed {
        return Err(LaunchError::NotInstalled {
            title: game.title.clone(),
        });
    }

    let runtime_env: Vec<(String, String)> = handoff
        .runtime
        .map(|runtime| runtime.env.clone())
        .unwrap_or_default();
    let wrappers = runtime_wrappers(handoff);

    let (mode, program, args, env, cwd, prefix_dir) = if let Some(exe) = &game.exe {
        if !exe.is_file() {
            return Err(LaunchError::MissingExecutable {
                title: game.title.clone(),
                path: exe.clone(),
            });
        }
        if is_windows(exe, game) {
            let proton = handoff.proton.ok_or_else(|| LaunchError::NoProton {
                title: game.title.clone(),
            })?;
            let prefix_dir = prefix::dir_in(handoff.prefix_root, &game.id);

            // The profile's environment goes first and the umu values last: the
            // prefix and the runtime are the launcher's own, so a stale profile
            // key cannot point two games at one prefix.
            let mut env = runtime_env;
            for (key, value) in prefix::umu_env(&game.id, &proton.path, &prefix_dir) {
                push_env(&mut env, &key, &value);
            }
            push_env(&mut env, "STORE", store_env(game.id.store));

            let program = UMU_RUN.to_string();
            let args = vec![exe.display().to_string()];
            (
                LaunchMode::Umu,
                program,
                args,
                env,
                game.install_dir.clone(),
                Some(prefix_dir),
            )
        } else {
            // A native Linux game needs no Proton and no prefix: the exe runs
            // as it is, with the wrapper chain in front of it.
            let program = exe.display().to_string();
            (
                LaunchMode::Native,
                program,
                Vec::new(),
                runtime_env,
                game.install_dir.clone(),
                None,
            )
        }
    } else if let Some(client) = handoff.client {
        // The store's client owns the runtime and the prefix (`gogdl` picks
        // its own Wine, `nile` its own Bottles/wine), so no umu variable is
        // added here. The profile still applies: it is the launcher's promise
        // that a preview and a launch cannot disagree.
        (
            LaunchMode::StoreClient,
            client.program.to_string_lossy().into_owned(),
            client.args.clone(),
            runtime_env,
            client.cwd.clone().or_else(|| game.install_dir.clone()),
            None,
        )
    } else {
        return Err(LaunchError::NoExecutable {
            title: game.title.clone(),
        });
    };

    let (program, args) = flatten_wrappers(&wrappers, &program, &args);
    let spec = LaunchSpec {
        program: PathBuf::from(program),
        args,
        cwd,
        env,
    };

    Ok(Prepared {
        mode,
        spec,
        prefix: prefix_dir,
        game_key: prefix::game_key(&game.id),
    })
}

/// Whether a game is a Windows build.
///
/// The `.exe` suffix is the honest signal (a store may forget to tag a row),
/// and the `platform:windows` tag a backend sets covers the odd executable that
/// is not named `.exe`.
#[must_use]
pub fn is_windows(exe: &Path, game: &Game) -> bool {
    exe.extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("exe"))
        || game
            .tags
            .iter()
            .any(|tag| tag.eq_ignore_ascii_case(WINDOWS_TAG))
}

/// The umu `STORE` value for a store, which protonfixes matches on.
#[must_use]
pub const fn store_env(store: Store) -> &'static str {
    // `STORE` is a fixed vocabulary in umu; a value it does not know would be
    // worse than `none`, which simply skips the store-specific fixes.
    match store {
        Store::Steam => "steam",
        Store::Gog => "gog",
        Store::Epic => "epic",
        Store::Amazon => "amazon",
        Store::Local => "none",
    }
}

/// Append a variable, replacing an earlier value for the same key.
///
/// [`LaunchSpec`] lets later entries win, but the transparency panel renders the
/// list verbatim, so two `GAMEID=` prefixes would look like a bug. The umu
/// values are the launcher's own and always replace the profile's.
fn push_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(slot) = env
        .iter_mut()
        .find(|(existing, _)| existing.eq_ignore_ascii_case(key))
    {
        slot.1 = value.to_string();
        return;
    }
    env.push((key.to_string(), value.to_string()));
}

/// The wrapper steps a launch applies, with the session rule enforced.
fn runtime_wrappers(handoff: &Handoff<'_>) -> Vec<WrapperStep> {
    let steps = handoff
        .runtime
        .map(|runtime| runtime.wrappers.clone())
        .unwrap_or_default();
    if !handoff.session {
        return steps;
    }
    // Nested gamescope is a bug: the console session is the compositor.
    steps
        .into_iter()
        .filter(|step| !is_gamescope(step))
        .collect()
}

fn is_gamescope(step: &WrapperStep) -> bool {
    step.program
        .file_name()
        .is_some_and(|name| name.to_string_lossy() == "gamescope")
}

/// How to run a prepared launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions<'a> {
    /// Receive stdout and stderr here (created, with its parents). `None`
    /// discards both, which is only right for a throwaway probe.
    pub log_path: Option<&'a Path>,
    /// `SIGTERM` → `SIGKILL` grace for [`Session::cancel`].
    pub grace: Duration,
}

impl Default for RunOptions<'_> {
    fn default() -> Self {
        Self {
            log_path: None,
            grace: DEFAULT_GRACE,
        }
    }
}

impl RunOptions<'_> {
    /// The default log for one launch: `<logs>/<store>-<id>-<nanos>.log`.
    ///
    /// The store backends use the same shape for their own transcripts, so a
    /// bug report finds every run of a game in one directory.
    #[must_use]
    pub fn default_log_for(id: &GameId) -> Option<PathBuf> {
        let dir = paths::logs_dir()?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        Some(dir.join(format!("{}-{stamp}.log", prefix::game_key(id))))
    }
}

/// Start a prepared launch in its own session.
///
/// The child gets its own process group (`setsid`), so cancelling kills the
/// wrapper tree — `gamescope`, `gamemoderun`, `umu-run`, the game — and never
/// the launcher. Its stdout and stderr share one file.
pub fn start(spec: &LaunchSpec, options: &RunOptions<'_>) -> io::Result<Session> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    if let Some(cwd) = &spec.cwd
        && cwd.is_dir()
    {
        command.current_dir(cwd);
    }

    let log = match options.log_path {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            command.stdout(Stdio::from(file.try_clone()?));
            command.stderr(Stdio::from(file));
            Some(path.to_path_buf())
        }
        None => {
            command.stdout(Stdio::null());
            command.stderr(Stdio::null());
            None
        }
    };

    // SAFETY: `setsid` is async-signal-safe and the child runs nothing else
    // before `exec`, which is what `pre_exec` requires.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn()?;
    Ok(Session {
        child,
        log,
        grace: options.grace,
        started: Instant::now(),
    })
}

/// A running game.
#[derive(Debug)]
pub struct Session {
    child: Child,
    log: Option<PathBuf>,
    grace: Duration,
    started: Instant,
}

impl Session {
    /// The child's pid, which is also its process-group id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The file the child's output goes to, when one was requested.
    #[must_use]
    pub fn log_path(&self) -> Option<&Path> {
        self.log.as_deref()
    }

    /// Ask the whole process group to stop.
    ///
    /// `SIGTERM` first so Proton can flush its prefix; the caller keeps waiting.
    /// [`terminate`] runs the same escalation, which is what a "Stop" button
    /// uses when the session itself lives on a worker thread.
    pub fn cancel(&self) {
        terminate(self.pid(), self.grace);
    }

    /// Wait for the game to end.
    pub fn wait(&mut self) -> io::Result<Outcome> {
        let status = self.child.wait()?;
        Ok(self.outcome(status))
    }

    /// Ask whether the game already ended.
    pub fn try_wait(&mut self) -> io::Result<Option<Outcome>> {
        match self.child.try_wait()? {
            Some(status) => Ok(Some(self.outcome(status))),
            None => Ok(None),
        }
    }

    fn outcome(&self, status: ExitStatus) -> Outcome {
        Outcome {
            code: status.code(),
            signal: status.signal(),
            duration: self.started.elapsed(),
            log: self.log.clone(),
        }
    }
}

/// Stop a game's process group by pid.
///
/// `SIGTERM` now, `SIGKILL` after `grace` — a detached sleeper does the second
/// half so the caller never blocks. Killing a group that is already gone is a
/// no-op, so a game that exits during the grace period is unaffected.
pub fn terminate(pid: u32, grace: Duration) {
    let group = -(pid as i32);
    // SAFETY: `kill` is async-signal-safe and a best-effort signal; a missing
    // group is a `-1` return this launcher does not need to report.
    unsafe {
        libc::kill(group, libc::SIGTERM);
    }

    std::thread::spawn(move || {
        std::thread::sleep(grace);
        // SAFETY: same call, one grace period later.
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
    });
}

/// How a game ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// The exit code, or `None` when a signal ended the process.
    pub code: Option<i32>,
    /// The terminating signal, when one did.
    pub signal: Option<i32>,
    /// Wall clock from spawn to reap: a non-Steam game's playtime.
    #[serde(with = "secs")]
    pub duration: Duration,
    pub log: Option<PathBuf>,
}

impl Outcome {
    /// Whether the game exited with 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    #[must_use]
    pub fn duration_secs(&self) -> u64 {
        self.duration.as_secs()
    }

    /// A one-line verdict for a notice or the CLI.
    #[must_use]
    pub fn summary(&self) -> String {
        let total = self.duration.as_secs();
        let played = format_duration(total);
        match (self.code, self.signal) {
            (Some(0), _) => format!("exited 0 after {played}"),
            (Some(code), _) => format!("exited {code} after {played}"),
            (None, Some(signal)) => format!("stopped by signal {signal} after {played}"),
            (None, None) => format!("ended after {played}"),
        }
    }
}

/// `1h 05m`, `4m 12s`, `41s` — the CLI's own formatting, not the GUI's.
fn format_duration(secs: u64) -> String {
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

mod secs {
    use std::time::Duration;

    use serde::Serializer;

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::profile::ProtonRef;
    use std::os::unix::fs::PermissionsExt;

    fn game(store: Store, id: &str, exe: Option<PathBuf>) -> Game {
        let mut game = Game::new(GameId::new(store, id), id);
        game.installed = true;
        game.exe = exe;
        game
    }

    fn write_exe(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn proton() -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/usr/share/steam/compatibilitytools.d/proton-cachyos"),
            name: "proton-cachyos".to_string(),
            kind: crate::proton::ProtonKind::CachyOs,
            version: Some("proton-cachyos 10.0-20251101".to_string()),
            source: crate::proton::ProtonSource::SystemCompatibilityTools,
        }
    }

    fn runtime(id: GameId) -> ShimProfile {
        let mut profile = ShimProfile::new(id, "Default");
        profile
            .env
            .push(("DXVK_HUD".to_string(), "fps".to_string()));
        profile.proton = Some(crate::envc::shim_proton(&ProtonRef::Auto, None));
        profile
    }

    fn handoff<'a>(
        game: &'a Game,
        runtime: Option<&'a ShimProfile>,
        proton: Option<&'a ProtonInstall>,
        root: &'a Path,
    ) -> Handoff<'a> {
        Handoff {
            game,
            runtime,
            proton,
            prefix_root: root,
            session: false,
            client: None,
        }
    }

    #[test]
    fn steam_is_not_a_hand_off_game() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_exe(temp.path(), "game.sh", "#!/bin/sh\nexit 0\n");
        let game = game(Store::Steam, "620", Some(exe));
        let error = prepare(&handoff(&game, None, None, temp.path())).unwrap_err();
        assert_eq!(error, LaunchError::SteamGame);
    }

    #[test]
    fn an_uninstalled_game_is_refused_before_anything_else() {
        let temp = tempfile::tempdir().unwrap();
        let mut game = game(Store::Epic, "Gizmo", None);
        game.installed = false;
        let error = prepare(&handoff(&game, None, None, temp.path())).unwrap_err();
        assert!(matches!(error, LaunchError::NotInstalled { .. }), "{error}");
    }

    #[test]
    fn a_row_without_an_executable_says_to_sync() {
        let temp = tempfile::tempdir().unwrap();
        let game = game(Store::Gog, "1234", None);
        let error = prepare(&handoff(&game, None, None, temp.path())).unwrap_err();
        assert!(matches!(error, LaunchError::NoExecutable { .. }), "{error}");
        assert!(error.to_string().contains("sync the store's library"));
    }

    #[test]
    fn a_stale_executable_path_is_named() {
        let temp = tempfile::tempdir().unwrap();
        let game = game(
            Store::Epic,
            "Gizmo",
            Some(temp.path().join("Gizmo/Binaries/Win64/Gizmo.exe")),
        );
        let error = prepare(&handoff(&game, None, Some(&proton()), temp.path())).unwrap_err();
        match error {
            LaunchError::MissingExecutable { path, .. } => {
                assert!(path.ends_with("Gizmo.exe"), "{}", path.display());
            }
            other => panic!("unexpected {other}"),
        }
    }

    #[test]
    fn a_windows_game_goes_through_umu_with_its_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Gizmo");
        fs::create_dir_all(&install).unwrap();
        let exe = write_exe(&install, "Gizmo.exe", "");
        let mut game = game(Store::Epic, "Gizmo", Some(exe.clone()));
        game.install_dir = Some(install.clone());
        let mut profile = runtime(game.id.clone());
        // A stale value in an old profile must not point GAMEID somewhere else.
        profile
            .env
            .push(("GAMEID".to_string(), "umu-default".to_string()));
        let proton = proton();

        let prepared =
            prepare(&handoff(&game, Some(&profile), Some(&proton), temp.path())).unwrap();

        assert_eq!(prepared.mode, LaunchMode::Umu);
        assert_eq!(prepared.spec.program, PathBuf::from("umu-run"));
        assert_eq!(prepared.spec.args, vec![exe.display().to_string()]);
        assert_eq!(prepared.game_key, "epic-Gizmo");
        assert_eq!(
            prepared.prefix,
            Some(temp.path().join("epic-Gizmo")),
            "one prefix per game, under the root"
        );
        assert_eq!(prepared.spec.cwd.as_deref(), Some(install.as_path()));

        let env = prepared.env_map();
        let prefix = prepared
            .prefix
            .clone()
            .expect("a Windows launch has a prefix");
        assert_eq!(env.get("STORE"), Some(&"epic"));
        assert_eq!(env.get("PROTONPATH"), Some(&proton.path.to_str().unwrap()));
        assert_eq!(env.get("WINEPREFIX"), Some(&prefix.to_str().unwrap()));
        assert_eq!(env.get("DXVK_HUD"), Some(&"fps"), "the profile is applied");
        assert_eq!(
            env.get("GAMEID"),
            Some(&"epic-Gizmo"),
            "the prefix's GAMEID wins over a stale profile value"
        );
        assert_eq!(
            prepared
                .spec
                .env
                .iter()
                .filter(|(key, _)| key == "GAMEID")
                .count(),
            1,
            "the stale value is replaced, not duplicated: {}",
            prepared.render()
        );
    }

    #[test]
    fn a_windows_game_without_proton_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_exe(temp.path(), "Gizmo.exe", "");
        let game = game(Store::Epic, "Gizmo", Some(exe));
        let error = prepare(&handoff(&game, None, None, temp.path())).unwrap_err();
        assert!(matches!(error, LaunchError::NoProton { .. }), "{error}");
    }

    #[test]
    fn the_windows_tag_covers_an_exe_that_is_not_named_exe() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_exe(temp.path(), "Gizmo", "");
        let mut game = game(Store::Epic, "Gizmo", Some(exe));
        game.tags.push(WINDOWS_TAG.to_string());

        let prepared = prepare(&handoff(&game, None, Some(&proton()), temp.path())).unwrap();
        assert_eq!(prepared.mode, LaunchMode::Umu);
    }

    #[test]
    fn a_native_game_runs_directly_with_no_umu_env() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Native");
        fs::create_dir_all(&install).unwrap();
        let exe = write_exe(&install, "native", "#!/bin/sh\nexit 0\n");
        let game = game(Store::Local, "native", Some(exe.clone()));
        let profile = runtime(game.id.clone());

        let prepared = prepare(&handoff(&game, Some(&profile), None, temp.path())).unwrap();

        assert_eq!(prepared.mode, LaunchMode::Native);
        assert_eq!(prepared.spec.program, exe);
        assert!(prepared.spec.args.is_empty());
        assert_eq!(prepared.prefix, None);
        let env = prepared.env_map();
        assert_eq!(env.get("DXVK_HUD"), Some(&"fps"));
        for key in ["PROTONPATH", "WINEPREFIX", "STORE"] {
            assert_eq!(env.get(key), None, "{key} is umu-only");
        }
    }

    #[test]
    fn a_store_client_starts_a_row_with_no_executable() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Gog Game");
        fs::create_dir_all(&install).unwrap();
        let mut game = game(Store::Gog, "1207658930", None);
        game.install_dir = Some(install.clone());
        let profile = runtime(game.id.clone());
        let client = LaunchSpec {
            program: PathBuf::from("gogdl"),
            args: vec![
                "launch".to_string(),
                install.display().to_string(),
                "1207658930".to_string(),
                "--platform".to_string(),
                "windows".to_string(),
            ],
            cwd: None,
            env: Vec::new(),
        };
        let mut request = handoff(&game, Some(&profile), None, temp.path());
        request.client = Some(&client);

        let prepared = prepare(&request).unwrap();

        assert_eq!(prepared.mode, LaunchMode::StoreClient);
        assert_eq!(prepared.spec.program, PathBuf::from("gogdl"));
        assert_eq!(prepared.spec.args, client.args);
        assert_eq!(
            prepared.prefix, None,
            "the store client owns its own runtime and prefix"
        );
        assert_eq!(
            prepared.spec.cwd.as_deref(),
            Some(install.as_path()),
            "the row's install directory is the working directory"
        );
        let env = prepared.env_map();
        assert_eq!(
            env.get("DXVK_HUD"),
            Some(&"fps"),
            "the profile still applies"
        );
        for key in ["PROTONPATH", "WINEPREFIX", "STORE", "GAMEID"] {
            assert_eq!(env.get(key), None, "{key} is umu-only");
        }
    }

    #[test]
    fn the_client_specs_own_cwd_wins_and_wrappers_still_flatten() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("Gog Game");
        fs::create_dir_all(&install).unwrap();
        let mut game = game(Store::Gog, "1207658930", None);
        game.install_dir = Some(install.clone());
        let client = LaunchSpec {
            program: PathBuf::from("/usr/bin/gogdl"),
            args: vec!["launch".to_string(), "1207658930".to_string()],
            cwd: Some(temp.path().to_path_buf()),
            env: Vec::new(),
        };
        let mut profile = runtime(game.id.clone());
        profile.wrappers = vec![WrapperStep {
            program: PathBuf::from("gamescope"),
            args: vec!["--".to_string()],
        }];
        let mut request = handoff(&game, Some(&profile), None, temp.path());
        request.client = Some(&client);

        let prepared = prepare(&request).unwrap();
        assert_eq!(prepared.spec.program, PathBuf::from("gamescope"));
        assert_eq!(
            prepared.spec.args,
            vec![
                "--".to_string(),
                "/usr/bin/gogdl".to_string(),
                "launch".to_string(),
                "1207658930".to_string(),
            ]
        );
        assert_eq!(
            prepared.spec.cwd.as_deref(),
            Some(temp.path()),
            "an explicit client cwd beats the install directory"
        );
    }

    #[test]
    fn wrappers_flatten_in_front_of_umu_run() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_exe(temp.path(), "Gizmo.exe", "");
        let game = game(Store::Epic, "Gizmo", Some(exe.clone()));
        let mut profile = runtime(game.id.clone());
        profile.wrappers = vec![
            WrapperStep {
                program: PathBuf::from("gamescope"),
                args: vec!["--fullscreen".to_string(), "--".to_string()],
            },
            WrapperStep {
                program: PathBuf::from("gamemoderun"),
                args: Vec::new(),
            },
        ];
        let proton = proton();

        let prepared =
            prepare(&handoff(&game, Some(&profile), Some(&proton), temp.path())).unwrap();
        assert_eq!(prepared.spec.program, PathBuf::from("gamescope"));
        assert_eq!(
            prepared.spec.args,
            vec![
                "--fullscreen".to_string(),
                "--".to_string(),
                "gamemoderun".to_string(),
                "umu-run".to_string(),
                exe.display().to_string(),
            ]
        );
        let rendered = prepared.render();
        assert!(rendered.contains("GAMEID=epic-Gizmo"), "{rendered}");
        assert!(!rendered.contains("GAMEID=umu-default"), "{rendered}");
    }

    #[test]
    fn the_console_session_suppresses_gamescope_only() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_exe(temp.path(), "Gizmo.exe", "");
        let game = game(Store::Epic, "Gizmo", Some(exe));
        let mut profile = runtime(game.id.clone());
        profile.wrappers = vec![
            WrapperStep {
                program: PathBuf::from("/usr/bin/gamescope"),
                args: vec!["--".to_string()],
            },
            WrapperStep {
                program: PathBuf::from("mangohud"),
                args: Vec::new(),
            },
        ];
        let proton = proton();

        let mut request = handoff(&game, Some(&profile), Some(&proton), temp.path());
        request.session = true;
        let prepared = prepare(&request).unwrap();

        assert_eq!(prepared.spec.program, PathBuf::from("mangohud"));
        assert_eq!(
            prepared.spec.args.first().map(String::as_str),
            Some("umu-run")
        );
    }

    #[test]
    fn the_store_vocabulary_matches_umu() {
        assert_eq!(store_env(Store::Epic), "epic");
        assert_eq!(store_env(Store::Gog), "gog");
        assert_eq!(store_env(Store::Amazon), "amazon");
        assert_eq!(store_env(Store::Local), "none");
        assert_eq!(store_env(Store::Steam), "steam");
    }

    #[test]
    fn a_finished_game_reports_its_exit_code_and_playtime() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_exe(temp.path(), "ok.sh", "#!/bin/sh\necho hello\nexit 0\n");
        let spec = LaunchSpec::new(&exe);
        let log = temp.path().join("logs/run.log");

        let options = RunOptions {
            log_path: Some(&log),
            grace: Duration::from_millis(50),
        };
        let mut session = start(&spec, &options).unwrap();
        let outcome = session.wait().unwrap();

        assert!(outcome.success());
        assert_eq!(outcome.code, Some(0));
        assert_eq!(outcome.signal, None);
        assert_eq!(outcome.log.as_deref(), Some(log.as_path()));
        let output = fs::read_to_string(&log).unwrap();
        assert!(output.contains("hello"), "{output:?}");
        assert!(outcome.summary().starts_with("exited 0 after"));
    }

    #[test]
    fn a_failing_game_keeps_its_exit_code() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_exe(temp.path(), "fail.sh", "#!/bin/sh\nexit 3\n");
        let spec = LaunchSpec::new(&exe);

        let mut session = start(&spec, &RunOptions::default()).unwrap();
        let outcome = session.wait().unwrap();
        assert_eq!(outcome.code, Some(3));
        assert!(!outcome.success());
        assert!(outcome.summary().starts_with("exited 3 after"));
    }

    #[test]
    fn cancelling_stops_the_whole_process_tree() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("started");
        let body = format!("#!/bin/sh\ntouch '{}'\nsleep 30\n", marker.display());
        let exe = write_exe(temp.path(), "long.sh", &body);
        let spec = LaunchSpec::new(&exe);

        let options = RunOptions {
            log_path: None,
            grace: Duration::from_millis(50),
        };
        let mut session = start(&spec, &options).unwrap();
        // Wait for the child to be running before signalling it; the marker
        // keeps the test off a sleep-and-hope race. The budget is generous
        // because a saturated CI box can take seconds to schedule the child.
        let mut waited = 0;
        while !marker.exists() && waited < 10_000 {
            std::thread::sleep(Duration::from_millis(10));
            waited += 10;
        }
        assert!(marker.exists(), "the fake game never started");

        session.cancel();
        let outcome = session.wait().unwrap();
        assert_eq!(
            outcome.signal,
            Some(libc::SIGTERM),
            "SIGTERM reaches the game's process group: {outcome:?}"
        );
        assert!(outcome.duration < Duration::from_secs(5));
    }

    #[test]
    fn the_log_directory_is_created_and_the_parent_is_not_required() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_exe(temp.path(), "ok.sh", "#!/bin/sh\nexit 0\n");
        let spec = LaunchSpec::new(&exe);
        let log = temp.path().join("a/b/c/run.log");

        let options = RunOptions {
            log_path: Some(&log),
            grace: Duration::from_millis(50),
        };
        let mut session = start(&spec, &options).unwrap();
        session.wait().unwrap();
        assert!(log.is_file());
    }

    #[test]
    fn a_missing_program_is_an_io_error_not_a_panic() {
        let spec = LaunchSpec::new("/nonexistent/pitagames/game");
        let error = start(&spec, &RunOptions::default()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_spec_env_reaches_the_child() {
        let temp = tempfile::tempdir().unwrap();
        let exe = write_exe(
            temp.path(),
            "env.sh",
            "#!/bin/sh\nprintf '%s' \"$PITAGAMES_TEST\"\n",
        );
        let mut spec = LaunchSpec::new(&exe);
        spec.env
            .push(("PITAGAMES_TEST".to_string(), "seen".to_string()));
        let log = temp.path().join("run.log");

        let options = RunOptions {
            log_path: Some(&log),
            grace: Duration::from_millis(50),
        };
        let mut session = start(&spec, &options).unwrap();
        session.wait().unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "seen");
    }

    #[test]
    fn the_default_log_name_is_the_game_key() {
        let log = RunOptions::default_log_for(&GameId::new(Store::Epic, "Gizmo"));
        match paths::logs_dir() {
            Some(dir) => {
                let log = log.expect("a log dir means a log path");
                assert!(log.starts_with(&dir));
                assert!(
                    log.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("epic-Gizmo-")
                );
            }
            None => assert!(log.is_none()),
        }
    }
}
