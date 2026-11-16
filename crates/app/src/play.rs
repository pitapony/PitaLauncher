//! The game the launcher is running.
//!
//! Steam launches itself (`library::spawn_steam`); everything else goes through
//! [`pitagames_platform::launch`] — the umu/Proton hand-off — on a worker thread
//! that owns the child process and reports back here.
//!
//! ```text
//!  card Play ──► Play::begin ──► play::spawn ──► prepare + start
//!                     ▲                                │
//!                     │                          PlayEvent::Started (pid)
//!                     └──────── Play::apply ◄── PlayEvent::Finished (exit + playtime)
//! ```
//!
//! One game at a time: two Proton trees on one prefix is exactly what the
//! per-game prefix layout exists to prevent, and the header's Stop button only
//! ever has one process group to signal.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::StreamExt as _;
use futures_util::lock::Mutex as AsyncMutex;
use pitagames_core::{Game, GameId, ProtonRef, ShimProfile};
use pitagames_platform::launch::{self as handoff, DEFAULT_GRACE, LaunchMode, Outcome, RunOptions};
use pitagames_platform::proton::{DiscoveryReport, ProtonInstall, ProtonSource};
use pitagames_stores::sync::Backends;

use crate::runtime;

/// One report from the thread running a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayEvent {
    /// The process is up: its pid is what Stop signals.
    Started {
        id: GameId,
        pid: u32,
        log: Option<PathBuf>,
    },
    /// The game ended; `Err` means it could not be started at all.
    Finished {
        id: GameId,
        title: String,
        outcome: Result<Outcome, String>,
    },
}

/// The window's end of the play event channel.
///
/// Same shape as [`crate::store_installs::InstallChannel`]: one drain future
/// owns the receiver, behind an async mutex because `use_hook` needs a `Clone`.
#[derive(Clone)]
pub struct PlayChannel {
    sender: UnboundedSender<PlayEvent>,
    receiver: Rc<AsyncMutex<Option<UnboundedReceiver<PlayEvent>>>>,
}

impl PlayChannel {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver: Rc::new(AsyncMutex::new(Some(receiver))),
        }
    }

    /// A sender for the Play buttons.
    pub fn sender(&self) -> UnboundedSender<PlayEvent> {
        self.sender.clone()
    }

    /// Wait for the next report; `None` once the channel closes.
    pub async fn next(&self) -> Option<PlayEvent> {
        let mut slot = self.receiver.lock().await;
        let receiver = slot.as_mut()?;
        receiver.next().await
    }
}

impl Default for PlayChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// What is starting, running or just finished.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Play {
    running: Option<Running>,
    last: Option<Last>,
}

/// The game holding the launcher's one play slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub id: GameId,
    pub title: String,
    /// `None` between the click and [`PlayEvent::Started`].
    pub pid: Option<u32>,
    pub log: Option<PathBuf>,
}

/// The last game that ended, for the header strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Last {
    pub title: String,
    pub summary: String,
    pub ok: bool,
}

impl Play {
    /// Claim the slot for a game before its thread starts.
    ///
    /// `Err` names the game already running: the grid disables Play for that
    /// row, but a second click from the detail page must not start a twin.
    pub fn begin(&mut self, game: &Game) -> Result<(), String> {
        if let Some(running) = &self.running {
            return Err(format!(
                "{} is still running; stop it before starting another game.",
                running.title
            ));
        }
        self.running = Some(Running {
            id: game.id.clone(),
            title: game.title.clone(),
            pid: None,
            log: None,
        });
        self.last = None;
        Ok(())
    }

    /// Fold a worker report in.
    pub fn apply(&mut self, event: &PlayEvent) {
        match event {
            PlayEvent::Started { id, pid, log } => {
                if let Some(running) = self.running.as_mut()
                    && running.id == *id
                {
                    running.pid = Some(*pid);
                    running.log = log.clone();
                }
            }
            PlayEvent::Finished { id, title, outcome } => {
                if self
                    .running
                    .as_ref()
                    .is_some_and(|running| running.id == *id)
                {
                    self.running = None;
                }
                let (summary, ok) = match outcome {
                    Ok(outcome) => (outcome.summary(), outcome.success()),
                    Err(message) => (message.clone(), false),
                };
                self.last = Some(Last {
                    title: title.clone(),
                    summary,
                    ok,
                });
            }
        }
    }

    pub fn running(&self) -> Option<&Running> {
        self.running.as_ref()
    }

    /// The last finished game, shown until the next Play.
    pub fn last(&self) -> Option<&Last> {
        self.last.as_ref()
    }

    /// Dismiss the header's "last game" strip.
    pub fn clear_last(&mut self) {
        self.last = None;
    }

    /// `"starting"`/`"playing"` for one card, `None` for every other row.
    pub fn label(&self, id: &GameId) -> Option<&'static str> {
        let running = self.running.as_ref().filter(|running| running.id == *id)?;
        Some(if running.pid.is_some() {
            "playing"
        } else {
            "starting"
        })
    }

    /// Ask the running game's process group to stop.
    ///
    /// `false` when nothing had a pid yet (the click was milliseconds ago); the
    /// thread will report `Finished` on its own either way.
    pub fn stop(&mut self) -> bool {
        let Some(running) = &self.running else {
            return false;
        };
        let Some(pid) = running.pid else {
            return false;
        };
        handoff::terminate(pid, DEFAULT_GRACE);
        true
    }
}

/// Run one game on its own thread.
///
/// The thread owns the [`handoff::Session`]; the window only keeps the pid, so a
/// closed window leaves the game running and the OS reaps it with the process
/// tree (`setsid`).
pub fn spawn(game: Game, sender: UnboundedSender<PlayEvent>) {
    let id = game.id.clone();
    let title = game.title.clone();
    let thread_name = format!("pitagames-play-{}", game.id.store_id);
    let reporter = sender.clone();
    // The closure takes the game, so the failure path keeps its own copy.
    let (fallback_id, fallback_title) = (id.clone(), title.clone());
    let spawned = thread::Builder::new().name(thread_name).spawn(move || {
        let outcome = start_game(&game, &sender);
        let _ = sender.unbounded_send(PlayEvent::Finished { id, title, outcome });
    });

    if let Err(error) = spawned {
        let _ = reporter.unbounded_send(PlayEvent::Finished {
            id: fallback_id,
            title: fallback_title,
            outcome: Err(format!("could not start the game thread: {error}")),
        });
    }
}

/// Compose and start one game; the worker's whole body.
fn start_game(game: &Game, sender: &UnboundedSender<PlayEvent>) -> Result<Outcome, String> {
    let runtime = load_runtime(&game.id);
    let report = runtime::discover_proton();
    // A native executable runs without Proton, and discovery failing on a
    // machine that has none must not stop it: only a Windows build needs the
    // answer, and `prepare` names that failure itself.
    let proton = match resolve_proton(runtime.as_ref(), &report) {
        Ok(install) => install,
        Err(error) if needs_proton(game) => return Err(error),
        Err(_) => None,
    };
    let root = pitagames_platform::prefix::default_root()
        .ok_or_else(|| "no data directory (HOME is unset)".to_string())?;
    // A store whose client starts games by id (gogdl, nile) names no
    // executable, so its argv is composed here and the hand-off only
    // supervises it.
    let client = client_launch(game)?;

    let prepared = handoff::prepare(&handoff::Handoff {
        game,
        runtime: runtime.as_ref(),
        proton: proton.as_ref(),
        prefix_root: &root,
        session: runtime::session_mode(),
        client: client.as_ref(),
    })
    .map_err(|error| error.to_string())?;

    let log = RunOptions::default_log_for(&game.id);
    let options = RunOptions {
        log_path: log.as_deref(),
        grace: DEFAULT_GRACE,
    };
    let mut session = handoff::start(&prepared.spec, &options).map_err(|error| {
        // The two shapes fail on different missing programs: a Windows build
        // wants `umu-run`, a store client wants its own CLI on PATH.
        let hint = if prepared.mode == LaunchMode::StoreClient {
            "is the store's CLI installed?"
        } else {
            "is umu-run installed?"
        };
        format!(
            "could not start {} ({error}); {hint}",
            prepared.spec.program.display()
        )
    })?;

    let _ = sender.unbounded_send(PlayEvent::Started {
        id: game.id.clone(),
        pid: session.pid(),
        log: session.log_path().map(Path::to_path_buf),
    });

    session.wait().map_err(|error| error.to_string())
}

/// The store client's argv, when this row launches by id instead of by path.
///
/// `None` for every other row, which keeps the umu/native hand-off untouched.
/// A template failure (GOG's `gogdl` needs the install directory) is returned
/// as the launch's own error, because it is exactly the case a Play button
/// cannot retry around.
fn client_launch(game: &Game) -> Result<Option<pitagames_core::LaunchSpec>, String> {
    if game.exe.is_some() || !pitagames_stores::launch::launches_by_id(game.id.store) {
        return Ok(None);
    }
    let backends = Backends::new();
    pitagames_stores::launch::client_spec(&backends, game)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// The compiled profile the hand-off should apply, if one was written.
///
/// Steam's shim profile and a non-Steam game's profile are the same file shape
/// (`profiles/<store>-<id>.json`); a corrupt one is ignored, never fatal, which
/// is the shim's own contract.
fn load_runtime(id: &GameId) -> Option<ShimProfile> {
    let dir = pitagames_core::paths::profiles_dir()?;
    match pitagames_core::load_shim_profile(&dir, id) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("[pitagames] ignoring {id:?}'s profile: {error}");
            None
        }
    }
}

/// The Proton a launch uses: the one the profile recorded, else discovery.
fn resolve_proton(
    runtime: Option<&ShimProfile>,
    report: &DiscoveryReport,
) -> Result<Option<ProtonInstall>, String> {
    let recorded = runtime
        .and_then(|runtime| runtime.proton.as_ref())
        .and_then(|proton| proton.install.as_ref());

    if let Some(recorded) = recorded
        && let Ok(install) = ProtonInstall::from_dir(&recorded.path, ProtonSource::Explicit)
    {
        return Ok(Some(install));
    }

    let reference = runtime
        .and_then(|runtime| runtime.proton.as_ref())
        .map(|proton| proton.reference.clone())
        .unwrap_or(ProtonRef::Auto);

    report
        .resolve(&reference)
        .map_err(|error| error.to_string())
}

/// Whether this row's launch needs a Proton build at all.
fn needs_proton(game: &Game) -> bool {
    game.exe
        .as_deref()
        .is_some_and(|exe| handoff::is_windows(exe, game))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitagames_core::{Game, Store};
    use std::time::Duration;

    fn game(id: &str) -> Game {
        let mut game = Game::new(GameId::new(Store::Epic, id), format!("Game {id}"));
        game.installed = true;
        game
    }

    fn outcome(code: i32, secs: u64) -> Outcome {
        Outcome {
            code: Some(code),
            signal: None,
            duration: Duration::from_secs(secs),
            log: None,
        }
    }

    #[test]
    fn one_game_holds_the_slot_at_a_time() {
        let mut play = Play::default();
        assert!(play.begin(&game("Gizmo")).is_ok());
        assert_eq!(
            play.label(&GameId::new(Store::Epic, "Gizmo")),
            Some("starting")
        );

        let error = play.begin(&game("Other")).unwrap_err();
        assert!(error.contains("Game Gizmo"), "{error}");
        assert_eq!(
            play.running().map(|running| running.id.clone()),
            Some(GameId::new(Store::Epic, "Gizmo"))
        );
    }

    #[test]
    fn the_started_report_fills_the_pid_and_turns_the_label_on() {
        let mut play = Play::default();
        play.begin(&game("Gizmo")).unwrap();
        let id = GameId::new(Store::Epic, "Gizmo");

        play.apply(&PlayEvent::Started {
            id: id.clone(),
            pid: 4242,
            log: Some(PathBuf::from("/logs/epic-Gizmo-1.log")),
        });
        assert_eq!(play.label(&id), Some("playing"));
        assert_eq!(play.running().and_then(|running| running.pid), Some(4242));
        assert_eq!(
            play.label(&GameId::new(Store::Epic, "Other")),
            None,
            "only the running game's card changes"
        );
    }

    #[test]
    fn finishing_frees_the_slot_and_records_the_verdict() {
        let mut play = Play::default();
        play.begin(&game("Gizmo")).unwrap();
        let id = GameId::new(Store::Epic, "Gizmo");

        play.apply(&PlayEvent::Finished {
            id: id.clone(),
            title: "Game Gizmo".to_string(),
            outcome: Ok(outcome(0, 3725)),
        });

        assert!(play.running().is_none());
        assert_eq!(play.label(&id), None);
        let last = play.last().expect("a finished game is remembered");
        assert_eq!(last.summary, "exited 0 after 1h 02m");
        assert!(last.ok);
    }

    #[test]
    fn a_failure_is_reported_and_also_frees_the_slot() {
        let mut play = Play::default();
        play.begin(&game("Gizmo")).unwrap();

        play.apply(&PlayEvent::Finished {
            id: GameId::new(Store::Epic, "Gizmo"),
            title: "Game Gizmo".to_string(),
            outcome: Err("no Proton build".to_string()),
        });
        let last = play.last().unwrap();
        assert_eq!(last.summary, "no Proton build");
        assert!(!last.ok);
        assert!(play.running().is_none());
    }

    #[test]
    fn a_late_report_for_another_game_does_not_touch_the_slot() {
        let mut play = Play::default();
        play.begin(&game("Gizmo")).unwrap();

        play.apply(&PlayEvent::Finished {
            id: GameId::new(Store::Epic, "Stale"),
            title: "Stale".to_string(),
            outcome: Ok(outcome(0, 1)),
        });
        assert_eq!(
            play.label(&GameId::new(Store::Epic, "Gizmo")),
            Some("starting"),
            "the stale report must not free the slot"
        );
    }

    #[test]
    fn only_a_windows_build_needs_proton() {
        let mut native = game("Native");
        native.exe = Some(PathBuf::from("/games/Native/native.sh"));
        assert!(!needs_proton(&native));

        let mut windows = game("Windows");
        windows.exe = Some(PathBuf::from("/games/Windows/Game.exe"));
        assert!(needs_proton(&windows));

        let mut tagged = game("Tagged");
        tagged.exe = Some(PathBuf::from("/games/Tagged/Game"));
        tagged
            .tags
            .push(pitagames_platform::launch::WINDOWS_TAG.to_string());
        assert!(needs_proton(&tagged));

        assert!(!needs_proton(&game("NoExe")), "prepare names that failure");
    }

    #[test]
    fn stopping_before_the_pid_arrives_is_a_no_op() {
        let mut play = Play::default();
        play.begin(&game("Gizmo")).unwrap();

        assert!(!play.stop(), "nothing to signal yet");
        assert_eq!(
            play.label(&GameId::new(Store::Epic, "Gizmo")),
            Some("starting"),
            "the worker still owns the game"
        );
    }

    #[test]
    fn a_store_that_launches_by_id_gets_a_client_spec() {
        let mut gog = Game::new(GameId::new(Store::Gog, "1207658930"), "GOG Game");
        gog.installed = true;
        gog.install_dir = Some(PathBuf::from("/games/gog/GOG Game"));

        let spec = client_launch(&gog).unwrap().expect("gogdl composes a spec");
        assert_eq!(spec.program, PathBuf::from("gogdl"));
        assert!(spec.args.contains(&"launch".to_string()), "{:?}", spec.args);
        assert_eq!(spec.cwd.as_deref(), Some(Path::new("/games/gog/GOG Game")));
    }

    #[test]
    fn client_launch_is_only_for_a_store_without_an_executable() {
        let mut epic = game("Gizmo");
        epic.exe = Some(PathBuf::from("/games/Gizmo/Gizmo.exe"));
        assert!(client_launch(&epic).unwrap().is_none());

        assert!(
            client_launch(&game("Gizmo")).unwrap().is_none(),
            "Epic launches through umu, not its client"
        );

        let mut gog = Game::new(GameId::new(Store::Gog, "1234"), "GOG Game");
        gog.installed = true;
        gog.exe = Some(PathBuf::from("/games/gog/game.exe"));
        assert!(
            client_launch(&gog).unwrap().is_none(),
            "a row with an executable goes through the hand-off directly"
        );
    }

    #[test]
    fn a_gog_row_without_a_directory_reports_the_template_error() {
        let mut gog = Game::new(GameId::new(Store::Gog, "1234"), "GOG Game");
        gog.installed = true;

        let error = client_launch(&gog).unwrap_err();
        assert!(error.contains("dir"), "{error}");
    }
}
