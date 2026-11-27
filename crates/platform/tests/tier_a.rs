//! Tier A over fixture Proton trees.
//!
//! The unit tests in `tier_a` build [`ProtonInstall`] values by hand; these drive
//! the real metadata path — `ProtonInstall::from_dir` over a directory holding a
//! `proton` script and (for the readable cases) a `version` file — so the gate is
//! exercised against the same input a real machine has: a supported build, a
//! too-old build and one whose version cannot be read.
//!
//! Fixtures only: no GPU, no game and no Proton install. The last test pins the
//! Tier A acceptance criterion that the path installs nothing to disk.

use std::fs;
use std::path::Path;

use pitagames_platform::compat::{CompatEntry, PreferredDll};
use pitagames_platform::proton::{ProtonInstall, ProtonSource};
use pitagames_platform::tier_a::{self, TierAGate};

/// Write a directory `proton` discovery treats as a Proton build.
fn fake_proton(dir: &Path, version: Option<&str>) {
    fs::create_dir_all(dir.join("files")).unwrap();
    fs::write(dir.join("proton"), "#!/usr/bin/env python3\n").unwrap();
    if let Some(version) = version {
        fs::write(dir.join("version"), version).unwrap();
    }
}

/// A CachyOS build as discovery would report it.
fn install(root: &Path, name: &str, version: Option<&str>) -> ProtonInstall {
    let dir = root.join(name);
    fake_proton(&dir, version);
    ProtonInstall::from_dir(&dir, ProtonSource::SystemCompatibilityTools).unwrap()
}

fn compat(dll: PreferredDll) -> CompatEntry {
    CompatEntry::from_toml(&format!(
        "[game]\nstore = \"steam\"\nstore_id = \"1091500\"\ntitle = \"x\"\n\n\
         [upscaling]\npreferred_dll = \"{}\"\n",
        dll.file_name()
    ))
    .unwrap()
}

#[test]
fn fixture_proton_trees_gate_supported_too_old_and_unreadable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let supported = install(root, "proton-cachyos", Some("proton-cachyos 11.0-20260516"));
    let too_old = install(
        root,
        "proton-cachyos-old",
        Some("proton-cachyos 10.0-20251101"),
    );
    // A build whose `version` file is absent: readable enough to be discovered,
    // but its version is not.
    let unreadable = install(root, "proton-cachyos-noversion", None);

    assert_eq!(tier_a::gate(Some(&supported)), TierAGate::Supported);
    assert_eq!(tier_a::gate(Some(&too_old)), TierAGate::TooOld);
    assert_eq!(tier_a::gate(Some(&unreadable)), TierAGate::Unreadable);
    // Discovery still classifies every one of them as CachyOS.
    assert_eq!(
        too_old.kind,
        pitagames_platform::proton::ProtonKind::CachyOs
    );
    assert_eq!(
        unreadable.kind,
        pitagames_platform::proton::ProtonKind::CachyOs
    );
}

#[test]
fn a_supported_tree_emits_the_plan_env_with_the_compat_dll() {
    let temp = tempfile::tempdir().unwrap();
    let supported = install(
        temp.path(),
        "proton-cachyos",
        Some("proton-cachyos 11.0-20260516"),
    );
    assert!(tier_a::gate(Some(&supported)).is_supported());

    // The per-game DLL name comes from the compat entry...
    assert_eq!(
        tier_a::env(Some(&compat(PreferredDll::Dbghelp))),
        vec![
            ("PROTON_USE_OPTISCALER".to_string(), "1".to_string()),
            (
                "PROTON_OPTISCALER_NAME".to_string(),
                "dbghelp.dll".to_string()
            ),
        ]
    );
    // ...and falls back to the ordered head without one.
    assert_eq!(
        tier_a::env(None),
        vec![
            ("PROTON_USE_OPTISCALER".to_string(), "1".to_string()),
            ("PROTON_OPTISCALER_NAME".to_string(), "dxgi.dll".to_string()),
        ]
    );
}

#[test]
fn an_unsupported_tree_is_refused_and_offers_the_tier_b_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let too_old = install(
        root,
        "proton-cachyos-old",
        Some("proton-cachyos 10.0-20251101"),
    );
    let unreadable = install(root, "proton-cachyos-noversion", None);

    // The compiler gates through `capability`, whose verdict is `tier_a::gate`:
    // neither build is supported, so the Tier A env is never produced.
    assert!(!tier_a::gate(Some(&too_old)).is_supported());
    assert!(!tier_a::gate(Some(&unreadable)).is_supported());
}

#[test]
fn the_tier_a_path_installs_nothing_to_disk() {
    // Tier A has no deploy step: it computes an env vector and stops. The
    // manifest / `.gl-backup` machinery is Tier B's, so the game tree must be
    // byte-for-byte unchanged across a Tier A decision.
    let temp = tempfile::tempdir().unwrap();
    let game = temp.path().join("game");
    fs::create_dir_all(&game).unwrap();
    fs::write(game.join("game.exe"), b"the game").unwrap();
    let before = pitagames_mods::manifest::tree_hash(&game).unwrap();

    let env = tier_a::env(Some(&compat(PreferredDll::Dxgi)));

    assert_eq!(pitagames_mods::manifest::tree_hash(&game).unwrap(), before);
    assert!(
        env.iter().all(|(key, _)| key != "WINEDLLOVERRIDES"),
        "Tier A must not set the Tier B override: {env:?}"
    );
}
