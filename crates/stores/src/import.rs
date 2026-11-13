//! Read-only import of the games other launchers have installed.
//!
//! A user who already plays GOG/Epic/Amazon games through
//! [Heroic](https://heroicgameslauncher.com), [Lutris](https://lutris.net) or
//! [Bottles](https://usebottles.com) has those games on disk but no PitaGames
//! entry for them, and the store CLIs (`legendary`/`nile`/`lgogdownloader`) are
//! often not installed at all. The plan's answer is an **installed-only import**:
//! read the other launcher's own records and show what it already knows, with
//! nothing written back.
//!
//! Nothing here runs a program, writes a file or touches Steam. Every source is
//! a directory the caller passes in (so a fixture tree can be scanned without a
//! real install), and a source that is missing, malformed or half-written
//! degrades to a warning for that source alone — an import must never take the
//! library down with it.
//!
//! ```text
//!   Importer::default_roots() / with_roots(heroic, lutris, bottles)
//!       │
//!       ├─ heroic   <config>/heroic/legendaryConfig/legendary/installed.json  → Store::Epic
//!       │            <config>/heroic/gog_store/installed.json                 → Store::Gog
//!       │            <config>/heroic/nile_config/nile/installed.json          → Store::Amazon
//!       │            <config>/heroic/sideload_apps/library.json               → Store::Local
//!       ├─ lutris   <data>/lutris/pga.db                                      → Store::Local
//!       └─ bottles  <data>/bottles/library.yml                                → Store::Local
//! ```
//!
//! The three launchers are read where they actually keep the data, which is not
//! always where the plan said (see `docs/m2-importers.md`): Lutris 0.5 moved the
//! game list out of `~/.local/share/lutris/games/*.yml` into the `pga.db` SQLite
//! database, so its titles and install directories are only in there. Heroic's
//! install records are JSON, Bottles' library is YAML.
//!
//! Imported games keep the identity they have in the other launcher — an Epic
//! `app_name` from Heroic is the same [`GameId`] as one `stores::epic` lists, so
//! [`merge`] does not duplicate it — and are tagged `imported:<launcher>` so the
//! UI can say where the row came from.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use pitagames_core::paths;
use pitagames_core::{Game, GameId, Store};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Heroic's Flatpak application id.
const HEROIC_FLATPAK: &str = "com.heroicgameslauncher.hgl";
/// Lutris' Flatpak application id.
const LUTRIS_FLATPAK: &str = "net.lutris.Lutris";
/// Bottles' Flatpak application id.
const BOTTLES_FLATPAK: &str = "com.usebottles.bottles";

/// The tag every imported game carries, so a row can say where it came from.
pub fn imported_tag(origin: Origin) -> String {
    format!("imported:{}", origin.as_str())
}

/// Which launcher a set of records was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Heroic,
    Lutris,
    Bottles,
}

impl Origin {
    pub const ALL: [Self; 3] = [Self::Heroic, Self::Lutris, Self::Bottles];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Heroic => "heroic",
            Self::Lutris => "lutris",
            Self::Bottles => "bottles",
        }
    }

    /// The name to show a user.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Heroic => "Heroic",
            Self::Lutris => "Lutris",
            Self::Bottles => "Bottles",
        }
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one source file contributed.
///
/// A source that does not exist is not an error: most users have none of these
/// launchers, and `found` records the difference between "nothing there" and
/// "there, and it had no games".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceReport {
    /// The launcher the file belongs to.
    pub origin: Origin,
    /// Which of that launcher's files this is (`legendary`, `gog`, `pga.db`, …).
    pub kind: &'static str,
    /// The file that was read (or looked for).
    pub path: PathBuf,
    /// Games that came out of it.
    pub games: usize,
    /// Why the file could not be read, when it exists and does not parse.
    pub error: Option<String>,
}

impl SourceReport {
    /// Whether the file was there at all.
    pub fn found(&self) -> bool {
        self.path.exists()
    }
}

/// Everything one import pass found.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ImportReport {
    /// The imported games, in source order, one entry per [`GameId`].
    pub games: Vec<Game>,
    /// One row per file that was read or looked for.
    pub sources: Vec<SourceReport>,
    /// Problems with a file that only *enriches* another one (Heroic's cached
    /// libraries supply GOG and Amazon titles), which are not worth a source row.
    pub warnings: Vec<String>,
    /// Games dropped because an earlier source already had the same id.
    pub duplicates: usize,
}

impl ImportReport {
    /// The games found for one launcher.
    pub fn games_from(&self, origin: Origin) -> impl Iterator<Item = &Game> {
        let tag = imported_tag(origin);
        self.games
            .iter()
            .filter(move |game| game.tags.contains(&tag))
    }

    /// The sources that were read without an error.
    pub fn readable(&self) -> impl Iterator<Item = &SourceReport> {
        self.sources.iter().filter(|source| source.error.is_none())
    }

    /// The sources that exist and could not be read.
    pub fn failed(&self) -> impl Iterator<Item = &SourceReport> {
        self.sources.iter().filter(|source| source.error.is_some())
    }

    /// Drops a game whose [`GameId`] an earlier source already produced.
    ///
    /// Heroic can hold the same Steam/Epic/GOG game twice (a sideload entry for a
    /// game it also owns), and a user may have two launchers pointing at one
    /// library; a second row with the same id would be a duplicate tile.
    fn deduplicate(&mut self) {
        let before = self.games.len();
        let mut seen: BTreeSet<GameId> = BTreeSet::new();
        self.games.retain(|game| seen.insert(game.id.clone()));
        self.duplicates = before - self.games.len();
    }
}

/// The three roots an import reads from.
///
/// `heroic` is Heroic's config folder (the one that holds `legendaryConfig/`,
/// `gog_store/`, `nile_config/` and `sideload_apps/`), `lutris` is Lutris' data
/// folder (the one that holds `pga.db`) and `bottles` is Bottles' base folder
/// (the one that holds `library.yml` and `bottles/`).
#[derive(Debug, Clone)]
pub struct Importer {
    heroic: Vec<PathBuf>,
    lutris: Vec<PathBuf>,
    bottles: Vec<PathBuf>,
}

impl Default for Importer {
    fn default() -> Self {
        Self::default_roots()
    }
}

impl Importer {
    /// The locations every launcher uses, native and Flatpak, without checking
    /// that any of them exists.
    ///
    /// The Flatpak entries are listed next to the native ones because a user can
    /// have both installed, and because Heroic's Flatpak build is common: it
    /// keeps its whole config in `~/.var/app/<id>/config/heroic`.
    pub fn default_roots() -> Self {
        let mut importer = Self {
            heroic: Vec::new(),
            lutris: Vec::new(),
            bottles: Vec::new(),
        };
        if let Some(config) = paths::config_home() {
            importer.heroic.push(config.join("heroic"));
        }
        if let Some(data) = paths::data_home() {
            importer.lutris.push(data.join("lutris"));
            importer.bottles.push(data.join("bottles"));
        }
        if let Some(home) = paths::home_dir() {
            let flatpak = home.join(".var").join("app");
            importer
                .heroic
                .push(flatpak.join(HEROIC_FLATPAK).join("config").join("heroic"));
            importer
                .lutris
                .push(flatpak.join(LUTRIS_FLATPAK).join("data").join("lutris"));
            importer
                .bottles
                .push(flatpak.join(BOTTLES_FLATPAK).join("data").join("bottles"));
        }
        importer
    }

    /// An importer that reads exactly these roots.
    pub fn with_roots(heroic: Vec<PathBuf>, lutris: Vec<PathBuf>, bottles: Vec<PathBuf>) -> Self {
        Self {
            heroic,
            lutris,
            bottles,
        }
    }

    pub fn heroic_roots(&self) -> &[PathBuf] {
        &self.heroic
    }

    pub fn lutris_roots(&self) -> &[PathBuf] {
        &self.lutris
    }

    pub fn bottles_roots(&self) -> &[PathBuf] {
        &self.bottles
    }

    /// Reads every root and reports what was there.
    pub fn scan(&self) -> ImportReport {
        let mut report = ImportReport::default();
        for root in &self.heroic {
            self.scan_heroic(root, &mut report);
        }
        for root in &self.lutris {
            let database = root.join("pga.db");
            record(
                &mut report,
                Origin::Lutris,
                "pga.db",
                database.clone(),
                lutris_games(&database),
            );
        }
        for root in &self.bottles {
            let library = root.join("library.yml");
            record(
                &mut report,
                Origin::Bottles,
                "library.yml",
                library.clone(),
                bottles_library(&library),
            );
        }
        report.deduplicate();
        report
    }

    /// Heroic keeps one install record per store plus the sideloaded games.
    fn scan_heroic(&self, root: &Path, report: &mut ImportReport) {
        let legendary = root
            .join("legendaryConfig")
            .join("legendary")
            .join("installed.json");
        record(
            report,
            Origin::Heroic,
            "legendary",
            legendary.clone(),
            heroic_legendary(&legendary),
        );

        // GOG's install records carry the path but not the title; the cached
        // library next to them has the title. That cache only enriches, so a
        // broken one costs the titles, never the rows.
        let gog_library = root.join("store_cache").join("gog_library.json");
        let titles = match gog_titles(&gog_library) {
            Ok(titles) => titles,
            Err(error) => {
                report.warnings.push(error);
                BTreeMap::new()
            }
        };
        let gog = root.join("gog_store").join("installed.json");
        record(
            report,
            Origin::Heroic,
            "gog",
            gog.clone(),
            heroic_gog(&gog, &titles),
        );

        // Amazon is the same story: nile's own install record has the path, its
        // cached library has the title (`stores::amazon` parses it for the CLI
        // backend, so the two cannot drift).
        let nile = root.join("nile_config").join("nile");
        let nile_library = nile.join("library.json");
        let titles = match read_text(&nile_library) {
            Ok(Some(text)) => match crate::amazon::parse_owned(&text) {
                Ok(owned) => owned
                    .into_iter()
                    .map(|game| (game.id.store_id, game.title))
                    .collect(),
                Err(error) => {
                    report
                        .warnings
                        .push(format!("{}: {error}", nile_library.display()));
                    BTreeMap::new()
                }
            },
            Ok(None) => BTreeMap::new(),
            Err(error) => {
                report.warnings.push(error);
                BTreeMap::new()
            }
        };
        record(
            report,
            Origin::Heroic,
            "amazon",
            nile.join("installed.json"),
            heroic_amazon(&nile, &titles),
        );

        let sideload = root.join("sideload_apps").join("library.json");
        record(
            report,
            Origin::Heroic,
            "sideload",
            sideload.clone(),
            heroic_sideload(&sideload),
        );
    }
}

/// Adds a source's games to the report and records how it went.
fn record(
    report: &mut ImportReport,
    origin: Origin,
    kind: &'static str,
    path: PathBuf,
    result: Result<Vec<Game>, String>,
) -> usize {
    match result {
        Ok(games) => {
            let games_len = games.len();
            report.games.extend(games);
            report.sources.push(SourceReport {
                origin,
                kind,
                path,
                games: games_len,
                error: None,
            });
            games_len
        }
        Err(error) => {
            report.sources.push(SourceReport {
                origin,
                kind,
                path,
                games: 0,
                error: Some(error),
            });
            0
        }
    }
}

/// Adds the imported games a library does not already have.
///
/// A game that is already there wins: a real scan knows the installed state,
/// Steam's playtime and the update flags, which another launcher's record does
/// not. Returns how many games were added.
pub fn merge(library: &mut Vec<Game>, imported: Vec<Game>) -> usize {
    let mut added = 0;
    for game in imported {
        if library.iter().any(|existing| existing.id == game.id) {
            continue;
        }
        library.push(game);
        added += 1;
    }
    added
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Reads one JSON file, treating a missing file as "nothing to import".
fn parse_json_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    let Some(text) = read_text(path)? else {
        return Ok(None);
    };
    serde_json::from_str(&text).map(Some).map_err(|error| {
        format!(
            "{} is not the JSON shape {} writes: {error}",
            path.display(),
            "this launcher"
        )
    })
}

/// A byte size that may be a number or a string.
///
/// legendary writes `install_size` as a number, Heroic's GOG records as a
/// string, and both mean bytes.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Size {
    Bytes(u64),
    Text(String),
}

impl Size {
    fn bytes(&self) -> Option<u64> {
        match self {
            Self::Bytes(bytes) => Some(*bytes),
            Self::Text(text) => text.trim().parse().ok(),
        }
    }
}

/// The `platform:` tag for a record's own platform string.
///
/// The values differ per store (`Windows`/`Win32` from legendary, `windows`
/// from Heroic's GOG records and its sideload entries), so the tag is the value
/// lowercased — the runtime UI gates on `platform:windows` the same way Epic's
/// and Amazon's parsers already emit it.
fn push_platform_tag(tags: &mut Vec<String>, platform: &str) {
    let platform = platform.trim().to_ascii_lowercase();
    if !platform.is_empty() {
        tags.push(format!("platform:{platform}"));
    }
}

/// Joins an executable that the record stored relative to the install path.
///
/// legendary writes `executable` relative to `install_path`
/// (`FortniteGame/Binaries/Win64/FortniteLauncher.exe`), Heroic's GOG records
/// write an absolute path (a Windows one, which is left alone: it needs the
/// prefix, and that is the prefixes slice).
fn resolve_executable(install_path: &str, executable: &str) -> Option<PathBuf> {
    let executable = executable.trim();
    if executable.is_empty() {
        return None;
    }
    let path = Path::new(executable);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let install_path = install_path.trim();
    if install_path.is_empty() {
        return Some(path.to_path_buf());
    }
    Some(Path::new(install_path).join(path))
}

/// A path that is only recorded when it is not blank.
fn install_dir(install_path: &str) -> Option<PathBuf> {
    let install_path = install_path.trim();
    (!install_path.is_empty()).then(|| PathBuf::from(install_path))
}

/// One entry of legendary's own `installed.json`.
#[derive(Debug, Deserialize)]
struct LegendaryInstall {
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    install_path: String,
    #[serde(default)]
    executable: String,
    #[serde(default)]
    install_size: Option<Size>,
    #[serde(default)]
    is_dlc: bool,
    #[serde(default)]
    version: String,
    #[serde(default)]
    platform: String,
}

fn heroic_legendary(path: &Path) -> Result<Vec<Game>, String> {
    let entries: Option<Vec<LegendaryInstall>> = parse_json_file(path)?;
    Ok(entries
        .unwrap_or_default()
        .into_iter()
        .filter_map(legendary_game)
        .collect())
}

fn legendary_game(entry: LegendaryInstall) -> Option<Game> {
    if entry.is_dlc {
        return None;
    }
    let id = entry.app_name.trim();
    if id.is_empty() {
        return None;
    }
    let mut game = Game::new(GameId::new(Store::Epic, id), title_or(entry.title, id));
    game.installed = true;
    game.install_dir = install_dir(&entry.install_path);
    game.exe = resolve_executable(&entry.install_path, &entry.executable);
    game.version = non_empty(entry.version);
    game.size_bytes = entry.install_size.as_ref().and_then(Size::bytes);
    game.tags.push(imported_tag(Origin::Heroic));
    push_platform_tag(&mut game.tags, &entry.platform);
    Some(game)
}

/// Heroic's GOG install records, a `{"installed": [...]}` document.
#[derive(Debug, Default, Deserialize)]
struct GogStore {
    #[serde(default)]
    installed: Vec<GogInstall>,
}

#[derive(Debug, Deserialize)]
struct GogInstall {
    #[serde(default, rename = "appName", alias = "app_name")]
    app_name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    install_path: String,
    #[serde(default)]
    executable: String,
    #[serde(default)]
    install_size: Option<Size>,
    #[serde(default)]
    is_dlc: bool,
    #[serde(default)]
    version: String,
    #[serde(default)]
    platform: String,
}

/// Heroic's cached GOG library, which is where the titles are.
#[derive(Debug, Default, Deserialize)]
struct GogCache {
    #[serde(default)]
    games: Vec<GogLibraryGame>,
}

#[derive(Debug, Deserialize)]
struct GogLibraryGame {
    #[serde(default, rename = "app_name", alias = "appName")]
    app_name: String,
    #[serde(default)]
    title: String,
}

fn gog_titles(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let cache: Option<GogCache> = parse_json_file(path)?;
    Ok(cache
        .unwrap_or_default()
        .games
        .into_iter()
        .filter(|game| !game.app_name.trim().is_empty() && !game.title.trim().is_empty())
        .map(|game| {
            (
                game.app_name.trim().to_string(),
                game.title.trim().to_string(),
            )
        })
        .collect())
}

fn heroic_gog(path: &Path, titles: &BTreeMap<String, String>) -> Result<Vec<Game>, String> {
    let store: Option<GogStore> = parse_json_file(path)?;
    Ok(store
        .unwrap_or_default()
        .installed
        .into_iter()
        .filter_map(|entry| gog_game(entry, titles))
        .collect())
}

fn gog_game(entry: GogInstall, titles: &BTreeMap<String, String>) -> Option<Game> {
    if entry.is_dlc {
        return None;
    }
    let id = entry.app_name.trim();
    if id.is_empty() {
        return None;
    }
    let title = entry
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .or_else(|| titles.get(id).map(String::as_str))
        .unwrap_or(id);
    let mut game = Game::new(GameId::new(Store::Gog, id), title);
    game.installed = true;
    game.install_dir = install_dir(&entry.install_path);
    game.exe = resolve_executable(&entry.install_path, &entry.executable);
    game.version = non_empty(entry.version);
    game.size_bytes = entry.install_size.as_ref().and_then(Size::bytes);
    game.tags.push(imported_tag(Origin::Heroic));
    push_platform_tag(&mut game.tags, &entry.platform);
    Some(game)
}

/// Heroic's Amazon games, which are nile's records: Heroic copies nile's config
/// into `nile_config/nile`, so the same parser answers for both.
fn heroic_amazon(nile: &Path, titles: &BTreeMap<String, String>) -> Result<Vec<Game>, String> {
    let state = crate::amazon::installed_state(Some(nile)).map_err(|error| error.to_string())?;
    Ok(state
        .into_iter()
        .map(|entry| {
            let title = titles
                .get(&entry.id)
                .cloned()
                .unwrap_or_else(|| entry.id.clone());
            let mut game = Game::new(GameId::new(Store::Amazon, entry.id), title);
            game.installed = true;
            game.install_dir = (!entry.path.as_os_str().is_empty()).then(|| entry.path.clone());
            game.version = entry.version.clone();
            game.size_bytes = entry.size_bytes;
            game.tags.push(imported_tag(Origin::Heroic));
            // nile only ships Windows builds, exactly as `stores::amazon` tags them.
            game.tags.push("platform:windows".to_string());
            game
        })
        .collect())
}

/// Heroic's sideloaded games: a shortcut to an executable (or a URL) the user
/// added by hand. These have no store behind them, so they are [`Store::Local`].
#[derive(Debug, Default, Deserialize)]
struct SideloadLibrary {
    #[serde(default)]
    games: Vec<SideloadGame>,
}

#[derive(Debug, Deserialize)]
struct SideloadGame {
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    folder_name: String,
    #[serde(default)]
    is_installed: Option<bool>,
    #[serde(default)]
    install: SideloadInstall,
}

#[derive(Debug, Default, Deserialize)]
struct SideloadInstall {
    #[serde(default)]
    executable: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    is_dlc: bool,
}

fn heroic_sideload(path: &Path) -> Result<Vec<Game>, String> {
    let library: Option<SideloadLibrary> = parse_json_file(path)?;
    Ok(library
        .unwrap_or_default()
        .games
        .into_iter()
        .filter_map(sideload_game)
        .collect())
}

fn sideload_game(entry: SideloadGame) -> Option<Game> {
    if entry.install.is_dlc {
        return None;
    }
    let id = entry.app_name.trim();
    if id.is_empty() {
        return None;
    }
    let mut game = Game::new(GameId::new(Store::Local, id), title_or(entry.title, id));
    // A sideloaded entry is "installed" unless Heroic says otherwise; a URL
    // shortcut is not installed by definition.
    game.installed = entry.is_installed.unwrap_or(true);
    game.install_dir = install_dir(&entry.folder_name);
    game.exe = resolve_executable(&entry.folder_name, &entry.install.executable);
    game.tags.push(imported_tag(Origin::Heroic));
    push_platform_tag(&mut game.tags, &entry.install.platform);
    Some(game)
}

/// Opens Lutris' `pga.db` for reading.
///
/// A WAL database cannot always be opened read-only (the reader has to create
/// the `-shm` file), and Lutris is often running when a user imports, so a
/// refused read-only open falls back to a normal one. Nothing here writes: the
/// only statement is a `SELECT`.
fn open_lutris_db(path: &Path) -> Result<rusqlite::Connection, String> {
    use rusqlite::OpenFlags;

    let connection = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    // `SQLITE_OPEN_READ_WRITE` without `SQLITE_OPEN_CREATE`: a database that is
    // not there must stay absent.
    .or_else(|_| {
        rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    })
    .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(connection)
}

/// Reads Lutris' game list from its SQLite database.
///
/// Lutris 0.5 stores the game list in `pga.db` (`games` table); the per-game
/// YAML files next to it only hold the runner configuration, so a title and the
/// install directory are only in here. The query is `SELECT *` and every column
/// is read by name, so a Lutris release that adds, renames or drops a column
/// costs that one field — the rows still import.
fn lutris_games(path: &Path) -> Result<Vec<Game>, String> {
    use rusqlite::types::Value;

    // A missing database means Lutris is not installed, or has never run: not
    // an error, and never something to create (opening SQLite read-write would).
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection = open_lutris_db(path)?;
    let mut statement = connection
        .prepare("SELECT * FROM games")
        .map_err(|error| format!("{}: {error}", path.display()))?;
    // `SELECT *` on a schema we do not own, so a missing column is a field we
    // lose rather than an error.
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let at = |name: &str| columns.iter().position(|column| column == name);

    let (name_at, slug_at, runner_at, directory_at) =
        (at("name"), at("slug"), at("runner"), at("directory"));
    let (installed_at, platform_at, service_at) = (at("installed"), at("platform"), at("service"));

    let text = |row: &rusqlite::Row<'_>, index: Option<usize>| -> Option<String> {
        match row.get::<usize, Value>(index?).ok()? {
            Value::Text(text) => Some(text),
            Value::Integer(number) => Some(number.to_string()),
            Value::Real(number) => Some(number.to_string()),
            _ => None,
        }
    };

    let mut rows = statement
        .query([])
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let mut games = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("{}: {error}", path.display()))?
    {
        let name = text(row, name_at).unwrap_or_default();
        let slug = text(row, slug_at).unwrap_or_default();
        // The slug is the identity `lutris <slug>` uses and it survives a
        // reinstall; a row with neither a name nor a slug is not a game.
        let id = if slug.trim().is_empty() {
            name.trim().to_string()
        } else {
            slug.trim().to_string()
        };
        if id.is_empty() {
            continue;
        }
        let mut game = Game::new(GameId::new(Store::Local, id.clone()), title_or(name, &id));
        game.tags.push(imported_tag(Origin::Lutris));
        game.installed = match row.get::<usize, Value>(installed_at.unwrap_or(usize::MAX)) {
            Ok(Value::Integer(value)) => value != 0,
            Ok(Value::Text(value)) => value.trim() == "1" || value.trim() == "true",
            // No `installed` column: every row of `games` is a known game.
            _ => true,
        };
        game.install_dir = text(row, directory_at).and_then(|dir| install_dir(&dir));
        if let Some(runner) = text(row, runner_at).filter(|runner| !runner.trim().is_empty()) {
            game.tags.push(format!("runner:{}", runner.trim()));
        }
        if let Some(service) = text(row, service_at).filter(|service| !service.trim().is_empty()) {
            game.tags.push(format!("service:{}", service.trim()));
        }
        if let Some(platform) = text(row, platform_at) {
            push_platform_tag(&mut game.tags, &platform);
        }
        games.push(game);
    }
    Ok(games)
}

/// One entry of Bottles' `library.yml`.
///
/// Bottles' library is a map of uuid to entry, and it holds what the bottle
/// config does not: a display name.
#[derive(Debug, Deserialize)]
struct BottlesEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    store: String,
    #[serde(default)]
    bottle: Option<BottlesBottle>,
}

/// A Bottles bottle. Only its name is read: the prefix path lives in that
/// bottle's own `bottle.yml`, and a Windows program path there needs the prefix
/// to become a host path (the prefixes slice's job, not an import's).
#[derive(Debug, Deserialize)]
struct BottlesBottle {
    #[serde(default)]
    name: String,
}

fn bottles_library(path: &Path) -> Result<Vec<Game>, String> {
    let Some(text) = read_text(path)? else {
        return Ok(Vec::new());
    };
    let entries: BTreeMap<String, Option<BottlesEntry>> = serde_norway::from_str(&text)
        .map_err(|error| format!("{} is not the YAML Bottles writes: {error}", path.display()))?;
    Ok(entries
        .into_iter()
        .filter_map(|(uuid, entry)| entry.and_then(|entry| bottles_game(uuid, entry)))
        .collect())
}

fn bottles_game(uuid: String, entry: BottlesEntry) -> Option<Game> {
    let id = if entry.id.trim().is_empty() {
        uuid
    } else {
        entry.id.trim().to_string()
    };
    if id.is_empty() {
        return None;
    }
    let mut game = Game::new(
        GameId::new(Store::Local, id.clone()),
        title_or(entry.name, &id),
    );
    game.installed = true;
    game.tags.push(imported_tag(Origin::Bottles));
    // A bottle entry's program lives inside a Wine prefix (the path in the
    // bottle config is a Windows one), and an umu entry belongs to another
    // store's launcher: neither is an install directory this launcher owns, so
    // neither is guessed here.
    if let Some(bottle) = entry.bottle.filter(|bottle| !bottle.name.trim().is_empty()) {
        game.tags.push(format!("bottle:{}", bottle.name.trim()));
    }
    if !entry.source.trim().is_empty() {
        game.tags.push(format!("source:{}", entry.source.trim()));
    }
    if !entry.store.trim().is_empty() {
        game.tags.push(format!("store:{}", entry.store.trim()));
    }
    Some(game)
}

/// A trimmed string, or `None` when it is blank.
fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// A trimmed title, falling back to the id so a row is never blank.
fn title_or(title: String, fallback: &str) -> String {
    non_empty(title).unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_ENV_LOCK;
    use std::fs;

    const LEGENDARY: &str =
        include_str!("../../../fixtures/heroic/legendaryConfig/legendary/installed.json");
    const GOG_STORE: &str = include_str!("../../../fixtures/heroic/gog_store/installed.json");
    const GOG_CACHE: &str = include_str!("../../../fixtures/heroic/store_cache/gog_library.json");
    const NILE_LIBRARY: &str = include_str!("../../../fixtures/nile/library-list.json");
    const SIDELOAD: &str = include_str!("../../../fixtures/heroic/sideload_apps/library.json");
    const BOTTLES: &str = include_str!("../../../fixtures/bottles/library.yml");

    fn write(dir: &Path, relative: &str, contents: &str) -> PathBuf {
        let path = dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    /// A Heroic config folder holding every fixture.
    fn heroic_root(dir: &Path) -> PathBuf {
        let root = dir.join("heroic");
        write(&root, "legendaryConfig/legendary/installed.json", LEGENDARY);
        write(&root, "gog_store/installed.json", GOG_STORE);
        write(&root, "store_cache/gog_library.json", GOG_CACHE);
        write(
            &root,
            "nile_config/nile/installed.json",
            include_str!("../../../fixtures/nile/installed.json"),
        );
        write(&root, "nile_config/nile/library.json", NILE_LIBRARY);
        write(&root, "sideload_apps/library.json", SIDELOAD);
        root
    }

    #[test]
    fn heroic_legendary_records_become_epic_games_without_their_dlc() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "installed.json", LEGENDARY);
        let games = heroic_legendary(&path).unwrap();

        assert_eq!(games.len(), 2, "the DLC entry is not a second game");
        let fortnite = &games[0];
        assert_eq!(fortnite.id, GameId::new(Store::Epic, "Fortnite"));
        assert_eq!(fortnite.title, "Fortnite");
        assert!(fortnite.installed);
        assert_eq!(
            fortnite.install_dir.as_deref(),
            Some(Path::new("/home/player/Games/epic/Fortnite"))
        );
        // legendary stores the executable relative to the install path.
        assert_eq!(
            fortnite.exe.as_deref(),
            Some(Path::new(
                "/home/player/Games/epic/Fortnite/FortniteGame/Binaries/Win64/FortniteLauncher.exe"
            ))
        );
        assert_eq!(fortnite.size_bytes, Some(32_212_254_720));
        assert_eq!(fortnite.version.as_deref(), Some("1.2.3"));
        assert_eq!(fortnite.tags, ["imported:heroic", "platform:windows"]);

        assert_eq!(games[1].id.store_id, "SmoothCriminal");
        assert_eq!(games[1].tags[1], "platform:mac");
        assert!(!games.iter().any(|game| game.id.store_id.contains("DLC")));
    }

    #[test]
    fn a_legendary_document_that_is_not_json_is_one_source_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "installed.json", "{ not json");
        let error = heroic_legendary(&path).unwrap_err();
        assert!(error.contains("JSON"), "{error}");

        // A missing file is not an error: nothing is installed through it.
        assert_eq!(heroic_legendary(&dir.path().join("nope.json")).unwrap(), []);
    }

    #[test]
    fn heroic_gog_takes_its_titles_from_the_cached_library() {
        let dir = tempfile::tempdir().unwrap();
        let store = write(dir.path(), "gog_store/installed.json", GOG_STORE);
        let titles = gog_titles(&dir.path().join("gog_store/nope.json")).unwrap();
        assert!(titles.is_empty());
        let cache = write(dir.path(), "store_cache/gog_library.json", GOG_CACHE);
        let titles = gog_titles(&cache).unwrap();
        assert_eq!(
            titles.get("1207660623").map(String::as_str),
            Some("The Witcher 3: Wild Hunt")
        );

        let games = heroic_gog(&store, &titles).unwrap();
        assert_eq!(games.len(), 1, "the DLC record is dropped");
        assert_eq!(games[0].id, GameId::new(Store::Gog, "1207660623"));
        assert_eq!(games[0].title, "The Witcher 3: Wild Hunt");
        // Heroic's GOG records write the size as a string.
        assert_eq!(games[0].size_bytes, Some(42_949_672_960));
        assert_eq!(games[0].version.as_deref(), Some("1.32"));
        assert_eq!(games[0].tags, ["imported:heroic", "platform:windows"]);

        // Without the cache the row keeps the store id as a title.
        let untitled = heroic_gog(&store, &BTreeMap::new()).unwrap();
        assert_eq!(untitled[0].title, "1207660623");
    }

    #[test]
    fn heroic_amazon_games_reuse_niles_own_records() {
        let dir = tempfile::tempdir().unwrap();
        let nile = dir.path().join("nile_config/nile");
        fs::create_dir_all(&nile).unwrap();
        fs::write(
            nile.join("installed.json"),
            include_str!("../../../fixtures/nile/installed.json"),
        )
        .unwrap();
        let titles = {
            let owned = crate::amazon::parse_owned(NILE_LIBRARY).unwrap();
            owned
                .into_iter()
                .map(|game| (game.id.store_id, game.title))
                .collect::<BTreeMap<String, String>>()
        };

        let games = heroic_amazon(&nile, &titles).unwrap();
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].id.store, Store::Amazon);
        assert_eq!(games[0].title, "Adios");
        assert_eq!(
            games[0].install_dir.as_deref(),
            Some(Path::new("/home/player/Games/nile/Adios"))
        );
        assert_eq!(games[0].size_bytes, Some(12_345_678_901));
        assert_eq!(games[1].title, "790f3b07-fc9c", "no title in the cache");
        assert_eq!(games[0].tags, ["imported:heroic", "platform:windows"]);
    }

    #[test]
    fn sideloaded_shortcuts_become_local_games() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "library.json", SIDELOAD);
        let games = heroic_sideload(&path).unwrap();

        assert_eq!(games.len(), 2);
        let local = &games[0];
        assert_eq!(local.id.store, Store::Local);
        assert_eq!(local.title, "My Local Game");
        assert!(local.installed);
        assert_eq!(
            local.exe.as_deref(),
            Some(Path::new("/home/player/Games/local/game.exe"))
        );
        assert_eq!(local.tags, ["imported:heroic", "platform:windows"]);
        // A URL shortcut is recorded but not installed.
        assert!(!games[1].installed);
        assert_eq!(games[1].tags, ["imported:heroic"]);
    }

    #[test]
    fn one_broken_heroic_file_does_not_stop_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let root = heroic_root(dir.path());
        fs::write(
            root.join("legendaryConfig/legendary/installed.json"),
            "{ broken",
        )
        .unwrap();
        // The GOG cache is the file whose failure is only a warning.
        fs::write(root.join("store_cache/gog_library.json"), "{ broken").unwrap();

        let report = Importer::with_roots(vec![root], vec![], vec![]).scan();

        let failed: Vec<&str> = report.failed().map(|source| source.kind).collect();
        assert_eq!(failed, ["legendary"]);
        assert!(
            report
                .warnings
                .iter()
                .any(|line| line.contains("gog_library"))
        );
        // The other three sources still produced games.
        assert!(!report.games.is_empty());
        assert!(
            report
                .games
                .iter()
                .any(|game| game.id.store == Store::Amazon)
        );
        assert!(
            report
                .sources
                .iter()
                .any(|source| source.kind == "gog" && source.games == 1),
            "the GOG row survives a broken title cache"
        );
    }

    /// Builds the `games` table exactly as Lutris 0.5 does.
    fn lutris_db(dir: &Path, rows: &[&str]) -> PathBuf {
        let path = dir.join("pga.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE games (
                     id INTEGER PRIMARY KEY,
                     name TEXT,
                     sortname TEXT,
                     runner TEXT,
                     slug TEXT,
                     platform TEXT,
                     directory TEXT,
                     installed INTEGER,
                     year TEXT,
                     lastplayed TEXT,
                     configpath TEXT,
                     playtime INTEGER,
                     service TEXT,
                     service_id TEXT,
                     has_custom_banner INTEGER,
                     has_custom_icon INTEGER,
                     has_custom_coverart_big INTEGER
                 );",
            )
            .unwrap();
        for row in rows {
            connection.execute(row, []).unwrap();
        }
        drop(connection);
        path
    }

    #[test]
    fn lutris_games_come_from_pga_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = lutris_db(
            dir.path(),
            &[
                "INSERT INTO games (name, slug, runner, platform, directory, installed, configpath, service)
                 VALUES ('Hollow Knight', 'hollow-knight', 'wine', 'Windows',
                         '/home/player/Games/hollow-knight', 1, 'hollow-knight-1720000000', NULL)",
                "INSERT INTO games (name, slug, runner, platform, directory, installed, configpath, service)
                 VALUES ('Dota 2', 'dota-2', 'libretro', 'Linux', NULL, 0, 'dota-2-1720000001', 'steam')",
                // A row the user deleted the name of: nothing to show.
                "INSERT INTO games (name, slug, runner) VALUES ('', '', 'wine')",
            ],
        );

        let games = lutris_games(&path).unwrap();
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].id, GameId::new(Store::Local, "hollow-knight"));
        assert_eq!(games[0].title, "Hollow Knight");
        assert!(games[0].installed);
        assert_eq!(
            games[0].install_dir.as_deref(),
            Some(Path::new("/home/player/Games/hollow-knight"))
        );
        assert_eq!(
            games[0].tags,
            ["imported:lutris", "runner:wine", "platform:windows"]
        );
        assert!(!games[1].installed);
        assert_eq!(
            games[1].tags,
            [
                "imported:lutris",
                "runner:libretro",
                "service:steam",
                "platform:linux"
            ]
        );
        // The slug is the identity Lutris itself uses, not the row id.
        assert!(!games.iter().any(|game| game.id.store_id == "1"));
    }

    #[test]
    fn a_lutris_database_without_the_expected_columns_still_imports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pga.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE games (id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO games (name) VALUES ('Only A Name');",
            )
            .unwrap();
        drop(connection);

        let games = lutris_games(&path).unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].title, "Only A Name");
        assert_eq!(games[0].id, GameId::new(Store::Local, "Only A Name"));
        assert!(games[0].installed, "every row of `games` is a known game");
        assert_eq!(games[0].tags, ["imported:lutris"]);
    }

    #[test]
    fn a_missing_or_unreadable_lutris_database_is_a_source_error_only() {
        let dir = tempfile::tempdir().unwrap();
        // Missing: nothing to import, no error.
        assert_eq!(lutris_games(&dir.path().join("pga.db")).unwrap(), []);

        // A file that is not a database at all.
        fs::write(dir.path().join("pga.db"), "not a database").unwrap();
        let error = lutris_games(&dir.path().join("pga.db")).unwrap_err();
        assert!(error.contains("pga.db"), "{error}");
    }

    #[test]
    fn bottles_library_entries_become_local_games() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "library.yml", BOTTLES);
        let games = bottles_library(&path).unwrap();

        assert_eq!(games.len(), 2);
        assert_eq!(games[0].id.store, Store::Local);
        assert_eq!(games[0].title, "Notepad++");
        assert_eq!(
            games[0].tags,
            ["imported:bottles", "bottle:Gaming", "source:bottle"]
        );
        assert_eq!(games[1].title, "Hades");
        assert_eq!(
            games[1].tags,
            ["imported:bottles", "source:umu", "store:steam"]
        );
        // A bottle entry's program path is a Windows path inside the prefix, so
        // no install directory is invented from it.
        assert!(games.iter().all(|game| game.install_dir.is_none()));

        // Bottles' ids are the entry ids, not the map keys.
        assert_eq!(games[0].id.store_id, "6e1a3f5c-9b2d-4a6e-8f1c-2d3e4f5a6b7c");
    }

    #[test]
    fn a_missing_or_broken_bottles_library_is_a_source_error_only() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            bottles_library(&dir.path().join("library.yml")).unwrap(),
            []
        );

        let path = write(dir.path(), "library.yml", "not: [valid: yaml");
        let error = bottles_library(&path).unwrap_err();
        assert!(error.contains("YAML"), "{error}");
    }

    #[test]
    fn a_scan_reads_every_root_and_deduplicates_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let heroic = heroic_root(dir.path());

        // Two Heroic roots, the same config twice: the second must not double
        // every row.
        let report = Importer::with_roots(vec![heroic.clone(), heroic], vec![], vec![]).scan();

        assert_eq!(report.sources.len(), 8, "four files per root");
        assert_eq!(report.duplicates, 7, "every game of the second root");
        let ids: BTreeSet<GameId> = report.games.iter().map(|game| game.id.clone()).collect();
        assert_eq!(ids.len(), report.games.len());

        // Every store the fixtures cover, and the local rows.
        let stores: BTreeSet<Store> = report.games.iter().map(|game| game.id.store).collect();
        assert_eq!(
            stores,
            BTreeSet::from([Store::Epic, Store::Gog, Store::Amazon, Store::Local])
        );
        assert_eq!(
            report.games_from(Origin::Heroic).count(),
            report.games.len()
        );
        assert_eq!(report.games.len(), 7);
        assert!(report.failed().next().is_none());
    }

    #[test]
    fn a_scan_of_nothing_installed_is_empty_and_silent() {
        let dir = tempfile::tempdir().unwrap();
        let report = Importer::with_roots(
            vec![dir.path().join("heroic")],
            vec![dir.path().join("lutris")],
            vec![dir.path().join("bottles")],
        )
        .scan();

        assert!(report.games.is_empty());
        assert!(report.warnings.is_empty());
        assert!(report.sources.iter().all(|source| !source.found()));
        assert!(report.failed().next().is_none());
        assert_eq!(report.readable().count(), report.sources.len());
    }

    #[test]
    fn merge_keeps_the_games_a_library_already_has() {
        let mut library = vec![{
            let mut game = Game::new(GameId::new(Store::Epic, "Fortnite"), "Fortnite");
            game.installed = true;
            game.playtime = std::time::Duration::from_secs(60);
            game
        }];
        let imported = vec![
            Game::new(GameId::new(Store::Epic, "Fortnite"), "Fortnite"),
            Game::new(GameId::new(Store::Local, "hollow-knight"), "Hollow Knight"),
        ];

        let added = merge(&mut library, imported);
        assert_eq!(added, 1);
        assert_eq!(library.len(), 2);
        assert_eq!(library[0].playtime, std::time::Duration::from_secs(60));
    }

    #[test]
    fn the_default_roots_follow_the_xdg_directories_and_their_flatpaks() {
        let guard = TEST_ENV_LOCK.lock().unwrap();
        let previous = (
            std::env::var_os("HOME"),
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("XDG_DATA_HOME"),
        );
        // SAFETY: the env is restored before the lock is released.
        unsafe {
            std::env::set_var("HOME", "/home/player");
            std::env::set_var("XDG_CONFIG_HOME", "/home/player/.config");
            std::env::set_var("XDG_DATA_HOME", "/home/player/.local/share");
        }
        let importer = Importer::default_roots();
        let default_importer = Importer::default();
        unsafe {
            match &previous.0 {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &previous.1 {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match &previous.2 {
                Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
        drop(guard);

        assert_eq!(
            importer.heroic_roots(),
            [
                PathBuf::from("/home/player/.config/heroic"),
                PathBuf::from("/home/player/.var/app/com.heroicgameslauncher.hgl/config/heroic"),
            ]
        );
        assert_eq!(
            importer.lutris_roots(),
            [
                PathBuf::from("/home/player/.local/share/lutris"),
                PathBuf::from("/home/player/.var/app/net.lutris.Lutris/data/lutris"),
            ]
        );
        assert_eq!(
            importer.bottles_roots(),
            [
                PathBuf::from("/home/player/.local/share/bottles"),
                PathBuf::from("/home/player/.var/app/com.usebottles.bottles/data/bottles"),
            ]
        );
        assert_eq!(default_importer.heroic_roots(), importer.heroic_roots());
    }

    #[test]
    fn an_imported_game_keeps_the_identity_its_store_uses() {
        // The point of importing Heroic rather than inventing a local row: a
        // game `stores::epic` would list under this id must not double up.
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "installed.json", LEGENDARY);
        let imported = heroic_legendary(&path).unwrap();
        assert!(
            imported
                .iter()
                .all(|game| game.id == GameId::new(Store::Epic, game.id.store_id.clone()))
        );

        let mut library = vec![Game::new(GameId::new(Store::Epic, "Fortnite"), "Fortnite")];
        assert_eq!(merge(&mut library, imported), 1);
        assert_eq!(library.len(), 2);
    }
}
