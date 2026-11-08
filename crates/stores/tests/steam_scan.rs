//! End-to-end tests of the Steam scanner against fixture files and
//! synthetic library trees.

use std::fs;
use std::path::{Path, PathBuf};

use pitagames_stores::InstallPhase;
use pitagames_stores::parse_library_folders;
use pitagames_stores::steam::{self};

fn write(path: &Path, content: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn acf(app_id: u32, name: &str, install_dir: &str, state_flags: u32, size: u64) -> String {
    format!(
        r#""AppState"
{{
	"appid"		"{app_id}"
	"name"		"{name}"
	"StateFlags"	"{state_flags}"
	"installdir"	"{install_dir}"
	"LastUpdated"	"1735689600"
	"SizeOnDisk"	"{size}"
}}
"#
    )
}

fn modern_library_folders(libraries: &[&Path]) -> String {
    let entries: Vec<String> = libraries
        .iter()
        .enumerate()
        .map(|(index, library)| {
            format!(
                r#""{index}"
	{{
		"path"		"{}"
	}}
"#,
                library.display()
            )
        })
        .collect();

    format!(
        r#""libraryfolders"
{{
{}
}}
"#,
        entries.join("\n")
    )
}

#[test]
fn parses_the_modern_libraryfolders_fixture() {
    let text = include_str!("../../../fixtures/steam/libraryfolders_modern.vdf");
    let libraries = parse_library_folders(text).unwrap();

    assert_eq!(
        libraries,
        vec![
            PathBuf::from("/home/player/.local/share/Steam"),
            PathBuf::from("/mnt/games/SteamLibrary"),
            PathBuf::from("/media/My Games/Steam Library"),
        ]
    );
}

#[test]
fn parses_the_legacy_libraryfolders_fixture() {
    let text = include_str!("../../../fixtures/steam/libraryfolders_legacy.vdf");
    let libraries = parse_library_folders(text).unwrap();

    // The non-numeric bookkeeping keys must be skipped.
    assert_eq!(
        libraries,
        vec![
            PathBuf::from("/mnt/games/SteamLibrary"),
            PathBuf::from("/media/My Games/Steam Library"),
        ]
    );
}

#[test]
fn scan_finds_games_across_two_libraries() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");
    let second = root.path().join("SecondLibrary");

    write(
        &steam.join("steamapps/libraryfolders.vdf"),
        modern_library_folders(&[&steam, &second]),
    );

    write(
        &steam.join("steamapps/appmanifest_620.acf"),
        acf(620, "Portal 2", "Portal 2", 4, 12_884_901_888),
    );
    fs::create_dir_all(steam.join("steamapps/common/Portal 2")).unwrap();

    write(
        &steam.join("steamapps/appmanifest_570.acf"),
        acf(570, "Dota 2", "dota 2", 4, 48_318_382_080),
    );
    fs::create_dir_all(steam.join("steamapps/common/dota 2")).unwrap();

    write(
        &second.join("steamapps/appmanifest_400.acf"),
        acf(400, "Half-Life", "Half-Life", 4, 1_073_741_824),
    );
    fs::create_dir_all(second.join("steamapps/common/Half-Life")).unwrap();

    let report = steam::scan_steam(&steam, false);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    let titles: Vec<&str> = report
        .games
        .iter()
        .map(|game| game.title.as_str())
        .collect();
    assert_eq!(titles, vec!["Dota 2", "Half-Life", "Portal 2"]);

    let portal = &report.games[2];
    assert_eq!(portal.id.store_id, "620");
    assert_eq!(
        portal.install_dir.as_deref(),
        Some(steam.join("steamapps/common/Portal 2").as_path())
    );
    assert_eq!(portal.size_bytes, Some(12_884_901_888));
    assert!(portal.installed);
    assert!(portal.last_updated.is_some());
}

#[test]
fn scan_reports_install_phases_per_appid() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");

    // 4 = fully installed, 8 = update required, 262148 = installed + downloading.
    for (app_id, name, flags) in [
        (620, "Portal 2", 4u32),
        (570, "Dota 2", 8),
        (400, "Half-Life", 262_148),
    ] {
        write(
            &steam.join(format!("steamapps/appmanifest_{app_id}.acf")),
            acf(app_id, name, name, flags, 1024),
        );
        fs::create_dir_all(steam.join(format!("steamapps/common/{name}"))).unwrap();
    }

    let report = steam::scan_steam(&steam, false);
    assert_eq!(report.phases.get("620"), Some(&InstallPhase::Installed));
    assert_eq!(
        report.phases.get("570"),
        Some(&InstallPhase::UpdateAvailable)
    );
    assert_eq!(report.phases.get("400"), Some(&InstallPhase::Downloading));
}

#[test]
fn scan_excludes_runtime_tools_by_default() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");

    write(
        &steam.join("steamapps/appmanifest_1628350.acf"),
        acf(1628350, "Steam Linux Runtime 3.0 (sniper)", "slr", 4, 0),
    );
    fs::create_dir_all(steam.join("steamapps/common/slr")).unwrap();

    let report = steam::scan_steam(&steam, false);
    assert!(report.games.is_empty(), "{:?}", report.games);

    let report = steam::scan_steam(&steam, true);
    assert_eq!(report.games.len(), 1);
    assert_eq!(report.games[0].id.store_id, "1628350");
}

#[test]
fn scan_keeps_going_past_a_broken_manifest() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");

    write(
        &steam.join("steamapps/appmanifest_999.acf"),
        "\"AppState\"\n{\n\t\"appid\"\t\t\"999\"\n",
    );
    write(
        &steam.join("steamapps/appmanifest_620.acf"),
        acf(620, "Portal 2", "Portal 2", 4, 12_884_901_888),
    );
    fs::create_dir_all(steam.join("steamapps/common/Portal 2")).unwrap();

    let report = steam::scan_steam(&steam, false);

    assert_eq!(report.games.len(), 1);
    assert_eq!(report.games[0].id.store_id, "620");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("appmanifest_999.acf")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn scan_prefers_the_copy_whose_install_dir_exists() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");
    let second = root.path().join("SecondLibrary");

    write(
        &steam.join("steamapps/libraryfolders.vdf"),
        modern_library_folders(&[&steam, &second]),
    );

    // Root copy: manifest present, install directory missing (game moved out).
    write(
        &steam.join("steamapps/appmanifest_570.acf"),
        acf(570, "Dota 2", "dota 2", 4, 48_318_382_080),
    );
    // Second copy: manifest present and the install directory exists.
    write(
        &second.join("steamapps/appmanifest_570.acf"),
        acf(570, "Dota 2", "dota 2", 4, 48_318_382_080),
    );
    fs::create_dir_all(second.join("steamapps/common/dota 2")).unwrap();

    let report = steam::scan_steam(&steam, false);
    assert_eq!(report.games.len(), 1);
    assert_eq!(
        report.games[0].install_dir.as_deref(),
        Some(second.join("steamapps/common/dota 2").as_path())
    );
}

#[test]
fn scan_without_library_folders_scans_the_root_library() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");

    write(
        &steam.join("steamapps/appmanifest_620.acf"),
        acf(620, "Portal 2", "Portal 2", 4, 12_884_901_888),
    );
    fs::create_dir_all(steam.join("steamapps/common/Portal 2")).unwrap();

    let report = steam::scan_steam(&steam, false);
    assert_eq!(report.games.len(), 1);
    assert_eq!(report.games[0].id.store_id, "620");
}

#[test]
fn scan_reads_a_legacy_libraryfolders_file() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");
    let second = root.path().join("SecondLibrary");

    write(
        &steam.join("steamapps/libraryfolders.vdf"),
        format!(
            r#""LibraryFolders"
{{
	"TimeNextStatsReport"	"1234567890"
	"1"		"{}"
}}
"#,
            second.display()
        ),
    );

    write(
        &second.join("steamapps/appmanifest_400.acf"),
        acf(400, "Half-Life", "Half-Life", 4, 1_073_741_824),
    );
    fs::create_dir_all(second.join("steamapps/common/Half-Life")).unwrap();

    let report = steam::scan_steam(&steam, false);
    assert_eq!(report.games.len(), 1);
    assert_eq!(report.games[0].id.store_id, "400");
}

#[test]
fn scan_reads_playtime_and_last_played_from_localconfig() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");

    write(
        &steam.join("steamapps/appmanifest_620.acf"),
        acf(620, "Portal 2", "Portal 2", 4, 12_884_901_888),
    );
    fs::create_dir_all(steam.join("steamapps/common/Portal 2")).unwrap();
    write(
        &steam.join("steamapps/appmanifest_570.acf"),
        acf(570, "Dota 2", "dota 2", 4, 48_318_382_080),
    );
    fs::create_dir_all(steam.join("steamapps/common/dota 2")).unwrap();

    write(
        &steam.join("userdata/12345/config/localconfig.vdf"),
        include_str!("../../../fixtures/steam/localconfig.vdf"),
    );

    let report = steam::scan_steam(&steam, false);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    let portal = report
        .games
        .iter()
        .find(|game| game.id.store_id == "620")
        .expect("Portal 2 was scanned");
    assert_eq!(portal.playtime, std::time::Duration::from_secs(9000 * 60));
    assert_eq!(
        portal.last_played.map(|played| played.unix_timestamp()),
        Some(1_710_000_000)
    );

    // The fixture's appid 440 has no manifest here: it must be ignored.
    assert!(report.games.iter().all(|game| game.id.store_id != "440"));
}

#[test]
fn scan_without_userdata_is_not_a_warning() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");

    write(
        &steam.join("steamapps/appmanifest_620.acf"),
        acf(620, "Portal 2", "Portal 2", 4, 12_884_901_888),
    );
    fs::create_dir_all(steam.join("steamapps/common/Portal 2")).unwrap();

    let report = steam::scan_steam(&steam, false);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(report.games.len(), 1);
    assert_eq!(report.games[0].playtime, std::time::Duration::ZERO);
    assert_eq!(report.games[0].last_played, None);
}

#[test]
fn scan_warns_but_keeps_games_when_localconfig_is_broken() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");

    write(
        &steam.join("steamapps/appmanifest_620.acf"),
        acf(620, "Portal 2", "Portal 2", 4, 12_884_901_888),
    );
    fs::create_dir_all(steam.join("steamapps/common/Portal 2")).unwrap();
    write(
        &steam.join("userdata/12345/config/localconfig.vdf"),
        "\"localconfig\" { \"apps\" { \"620\" {",
    );

    let report = steam::scan_steam(&steam, false);
    assert_eq!(report.games.len(), 1);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("localconfig.vdf")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn scan_warns_when_the_manifest_appid_disagrees_with_the_filename() {
    let root = tempfile::tempdir().unwrap();
    let steam = root.path().join("Steam");

    // Filename says 570, the manifest says 620: the filename wins.
    write(
        &steam.join("steamapps/appmanifest_570.acf"),
        acf(620, "Portal 2", "Portal 2", 4, 12_884_901_888),
    );
    fs::create_dir_all(steam.join("steamapps/common/Portal 2")).unwrap();

    let report = steam::scan_steam(&steam, false);
    assert_eq!(report.games.len(), 1);
    assert_eq!(report.games[0].id.store_id, "570");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("does not match")),
        "{:?}",
        report.warnings
    );
}
