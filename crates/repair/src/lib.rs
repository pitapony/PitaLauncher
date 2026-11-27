//! One repair, three answers: mod trees, Steam config, database health.
//!
//! When a launcher misbehaves the user wants a single action that says what is
//! wrong and puts right what can be put right — not three diagnostics in three
//! places. This crate is that action, framework-free so the CLI's `repair`
//! command and the GUI's Settings button run the same code:
//!
//! - **mods** — [`check_roots`] re-hashes every mod manifest under the game
//!   roots and reports a tampered tree; [`reapply`] restores and re-installs it.
//! - **Steam config** — [`stores::steam_config`](pitagames_stores::steam_config)
//!   lists the restore points the launcher wrote and applies them, byte-for-byte
//!   where Steam has not changed the file and merged where it has.
//! - **database** — opening the database takes a pre-migration backup when one
//!   is owed, and [`Db::integrity_check`](pitagames_db::Db::integrity_check)
//!   runs SQLite's own `PRAGMA integrity_check`.
//!
//! [`export`] is the sibling: the no-endpoint local settings export, which
//! renders the GPU, Proton and resolved environment for the user to copy or
//! save. It never touches the network and never prints a credential.

pub mod export;
pub mod mods;

use std::path::PathBuf;

use pitagames_core::paths;
use pitagames_db::{Db, IntegrityReport};
use pitagames_stores::steam_config::{
    self, PendingRestore, RestoreOutcome, SteamProcessState, WriteOptions,
};

pub use export::{ExportInput, SettingsExport};
pub use mods::{
    ModFinding, ModState, ReapplyReport, check_dir, check_roots, find_manifests, reapply, restore,
};

/// Why a repair could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    /// The database could not be opened, backed up or checked.
    #[error(transparent)]
    Db(#[from] pitagames_db::Error),

    /// A Steam config restore failed.
    #[error(transparent)]
    Steam(#[from] steam_config::Error),
}

/// The Steam half of a report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SteamRepair {
    /// No backup directory was configured.
    NotConfigured,
    /// Restore points were found but not applied (`apply` was false).
    Pending(Vec<PendingRestore>),
    /// Restore points were applied.
    Restored(Vec<RestoreOutcome>),
}

impl SteamRepair {
    /// The restore points this section is about, applied or not.
    #[must_use]
    pub fn count(&self) -> usize {
        match self {
            Self::NotConfigured => 0,
            Self::Pending(rows) => rows.len(),
            Self::Restored(rows) => rows.len(),
        }
    }
}

/// The database half of a report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbRepair {
    /// The database that was checked.
    pub path: PathBuf,
    /// SQLite's verdict.
    pub integrity: IntegrityReport,
    /// The newest pre-migration backup beside the database, if one exists.
    pub backup: Option<PathBuf>,
}

/// What one repair found and did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RepairReport {
    /// Every mod manifest found, with its verdict.
    pub mods: Vec<ModFinding>,
    /// The Steam config restore points.
    pub steam: SteamRepair,
    /// The database check, when a database path was given.
    pub db: Option<DbRepair>,
}

impl RepairReport {
    /// The mods a re-apply is offered for.
    pub fn tampered_mods(&self) -> impl Iterator<Item = &ModFinding> {
        self.mods.iter().filter(|finding| finding.needs_reapply())
    }

    /// Whether nothing needed attention.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.mods.iter().all(ModFinding::ok)
            && self.steam.count() == 0
            && self.db.as_ref().is_none_or(|db| db.integrity.ok)
    }
}

/// What to repair and where to look.
#[derive(Debug, Clone)]
pub struct RepairRequest {
    /// Roots to scan for mod manifests.
    pub mod_roots: Vec<PathBuf>,
    /// The Steam config backup directory holding the restore journals.
    pub steam_backup: Option<PathBuf>,
    /// Whether Steam is running, so a restore can be refused rather than lost.
    pub steam_state: SteamProcessState,
    /// Restore Steam config even while Steam is running (the CLI's `--force`).
    pub allow_running: bool,
    /// The database to check; `None` skips the database section.
    pub db_path: Option<PathBuf>,
    /// Apply the repairs rather than only reporting them.
    pub apply: bool,
}

impl RepairRequest {
    /// The request a GUI screen wants: this machine's XDG game, backup and
    /// database paths, and the Steam state detected from the default roots.
    ///
    /// The one entry point a Settings screen needs — `run(&RepairRequest::
    /// for_this_machine(apply))` — with the CLI overriding the fields its flags
    /// name.
    #[must_use]
    pub fn for_this_machine(apply: bool) -> Self {
        let steam_state = paths::steam_roots()
            .into_iter()
            .find(|root| root.is_dir())
            .map_or(SteamProcessState::Unknown, |root| {
                steam_config::detect_steam_process(&root)
            });
        Self {
            mod_roots: paths::games_dir().into_iter().collect(),
            steam_backup: paths::steam_backup_dir(),
            steam_state,
            allow_running: false,
            db_path: paths::data_dir().map(|base| base.join("pitagames.db")),
            apply,
        }
    }
}

/// Run one repair: check the mod trees, list or apply the Steam restores, and
/// open the database (backing it up before any pending migration) and check it.
///
/// Detection always runs. `apply` controls only whether the Steam restore
/// points are written; mods are never changed here — a tampered tree is offered
/// a re-apply through [`reapply`], which the caller performs with its sources.
pub async fn run(request: &RepairRequest) -> Result<RepairReport, RepairError> {
    let mods = check_roots(&request.mod_roots);

    let steam = match &request.steam_backup {
        None => SteamRepair::NotConfigured,
        Some(dir) => {
            let pending = steam_config::pending_restores(dir)?;
            if request.apply && !pending.is_empty() {
                let options = WriteOptions {
                    backup_dir: dir.clone(),
                    allow_running: request.allow_running,
                };
                let mut restored = Vec::with_capacity(pending.len());
                for record in &pending {
                    restored.push(steam_config::restore_pending(
                        record,
                        request.steam_state,
                        &options,
                    )?);
                }
                SteamRepair::Restored(restored)
            } else {
                SteamRepair::Pending(pending)
            }
        }
    };

    let db = match &request.db_path {
        None => None,
        Some(path) => {
            // Opening the database migrates it — and takes a pre-migration
            // backup first when the schema is behind.
            let db = Db::open(path).await?;
            let integrity = db.integrity_check().await?;
            Some(DbRepair {
                path: path.clone(),
                integrity,
                backup: Db::latest_backup(path),
            })
        }
    };

    Ok(RepairReport { mods, steam, db })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use pitagames_core::{Game, GameId, Store};
    use pitagames_db::GameRepository;
    use pitagames_mods::{ModManifest, ModSource};

    const LOCALCONFIG_EMPTY: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/steam/localconfig_empty.vdf"
    );

    fn write(path: &std::path::Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// A temp tree with a tampered mod, a pending Steam restore and a database.
    async fn fixture(dir: &std::path::Path) -> RepairRequest {
        // A game directory with a mod whose created DLL was reverted.
        let game = dir.join("games/epic/Gizmo");
        write(&game.join("game.exe"), b"the game");
        let staging = dir.join("staging");
        let dll = staging.join("OptiScaler.dll");
        let ini = staging.join("OptiScaler.ini");
        write(&dll, b"the mod dll");
        write(&ini, b"[OptiScaler]\n");
        ModManifest::install(
            &game,
            "optiscaler",
            &[
                ModSource::new("d3d12.dll", dll),
                ModSource::new("OptiScaler.ini", ini),
            ],
        )
        .unwrap();
        write(&game.join("d3d12.dll"), b"reverted by a verify");

        // A Steam config we changed, with its restore journal.
        let localconfig = dir.join("Steam/userdata/123/config/localconfig.vdf");
        fs::create_dir_all(localconfig.parent().unwrap()).unwrap();
        fs::copy(LOCALCONFIG_EMPTY, &localconfig).unwrap();
        let backup = dir.join("backups/steam");
        steam_config::apply_launch_options(
            &localconfig,
            "620",
            Some("pitagames-shim 620 %command%"),
            SteamProcessState::Stopped,
            &WriteOptions::new(&backup),
        )
        .unwrap();

        // A database with one game row.
        let db_path = dir.join("pitagames.db");
        let db = Db::open(&db_path).await.unwrap();
        db.upsert_game(&Game::new(GameId::new(Store::Steam, "620"), "Portal 2"))
            .await
            .unwrap();

        RepairRequest {
            mod_roots: vec![dir.join("games")],
            steam_backup: Some(backup),
            steam_state: SteamProcessState::Stopped,
            allow_running: false,
            db_path: Some(db_path),
            apply: false,
        }
    }

    #[test]
    fn for_this_machine_fills_the_defaults_and_carries_apply() {
        // The entry point a Settings screen calls: defaults are best-effort
        // (they read the environment), but `apply` must survive verbatim.
        assert!(RepairRequest::for_this_machine(true).apply);
        assert!(!RepairRequest::for_this_machine(false).apply);
    }

    #[tokio::test]
    async fn one_repair_reports_the_mod_tree_steam_restores_and_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let request = fixture(dir.path()).await;

        let report = run(&request).await.unwrap();

        assert_eq!(report.mods.len(), 1, "{report:?}");
        assert!(report.mods[0].needs_reapply(), "{report:?}");
        assert_eq!(report.steam.count(), 1, "{report:?}");
        assert!(matches!(report.steam, SteamRepair::Pending(_)));
        let db = report.db.as_ref().expect("a database section");
        assert!(db.integrity.ok, "{db:?}");
        assert!(!report.is_clean());
    }

    #[tokio::test]
    async fn apply_restores_the_steam_config_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let mut request = fixture(dir.path()).await;
        let localconfig = dir.path().join("Steam/userdata/123/config/localconfig.vdf");
        let original = fs::read(LOCALCONFIG_EMPTY).unwrap();

        request.apply = true;
        let report = run(&request).await.unwrap();

        let SteamRepair::Restored(rows) = &report.steam else {
            panic!("{report:?}");
        };
        assert_eq!(rows.len(), 1);
        assert!(rows[0].exact, "{rows:?}");
        assert_eq!(fs::read(&localconfig).unwrap(), original);
        // The mod tree is untouched by a repair: a re-apply is the caller's job.
        assert!(check_dir(&dir.path().join("games/epic/Gizmo")).needs_reapply());
    }

    #[tokio::test]
    async fn a_tampered_mod_stays_re_appliable_through_the_repair() {
        let dir = tempfile::tempdir().unwrap();
        let request = fixture(dir.path()).await;
        let game = dir.path().join("games/epic/Gizmo");

        let report = run(&request).await.unwrap();
        let finding = report.tampered_mods().next().unwrap().clone();
        assert_eq!(finding.mod_name.as_deref(), Some("optiscaler"));

        // The repair pointed at the tree; the sources re-apply it cleanly.
        let staging = dir.path().join("staging");
        reapply(
            &finding.dir,
            "optiscaler",
            &[
                ModSource::new("d3d12.dll", staging.join("OptiScaler.dll")),
                ModSource::new("OptiScaler.ini", staging.join("OptiScaler.ini")),
            ],
        )
        .unwrap();
        assert!(check_dir(&game).ok());
    }
}
