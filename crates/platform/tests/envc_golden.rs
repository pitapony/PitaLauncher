//! Golden files for the full M3 environment compiler (`platform::envc`).
//!
//! One snapshot per (profile × GPU × Proton) cell of the plan §6 matrix. Each
//! case builds a [`Profile`], a fixture [`GpuProfile`] and (where the gates need
//! it) a [`CompatEntry`] and a [`ProtonInstall`], compiles it, and snapshots the
//! env, wrappers, ini and warnings. The committed `.snap` files are the golden
//! record: a compiler change that alters any emitted variable fails the test.
//!
//! Fixtures only — no GPU, no game, no Proton install and no network. Regenerate
//! deliberately with `INSTA_UPDATE=always cargo test -p pitagames-platform
//! --test envc_golden` (then review the diff).

use std::path::{Path, PathBuf};

use pitagames_core::profile::{
    FrameGen, Injection, InjectionDll, ModelSource, NrBackend, NrModel, NrProcessing, NrRoute,
    PkgArch, Profile, Upscaler,
};
use pitagames_core::{GameId, Store};
use pitagames_platform::compat::{CompatEntry, Version};
use pitagames_platform::envc::{CompileRequest, CompiledRuntime, GraphicsContext, compile};
use pitagames_platform::gpu::{GpuArch, GpuProfile, HdrSupport};
use pitagames_platform::proton::{ProtonInstall, ProtonKind, ProtonSource};

// The compat entry every game-API route in the matrix needs: DLSS + XeSS and a
// HelixSR Scaler mode, plus a neural-rendering section.
const CYBERPUNK: &str = r#"
[game]
store = "steam"
store_id = "1091500"
title = "Cyberpunk 2077"
anticheat = "none"
verified = ["rdna4"]

[upscaling]
supported = ["dlss", "fsr3", "xess"]
preferred_dll = "dbghelp.dll"
helixsr = { modes = ["scaler", "fsr31_dropin"], preferred = "scaler" }

[neural_rendering]
backend = "auto"
route = "optiscaler"
min_mesa = "26.2"
model_scale_default = { rdna4 = 100, rdna3 = 50 }
requires_64bit = true
"#;

fn game() -> GameId {
    GameId::new(Store::Steam, "1091500")
}

fn profile() -> Profile {
    Profile::new(game(), "golden")
}

fn cyberpunk() -> CompatEntry {
    CompatEntry::from_toml(CYBERPUNK).unwrap()
}

fn gpu(arch: GpuArch) -> GpuProfile {
    GpuProfile {
        vendor: arch.vendor(),
        arch,
        device_id: None,
        name: Some("fixture".to_string()),
        mesa: Some(Version::parse("27.0.0").unwrap()),
        vram_bytes: Some(16 * 1024 * 1024 * 1024),
        ntsync: false,
        hdr: HdrSupport::Unknown,
        warnings: Vec::new(),
    }
}

fn cachyos(version: &str) -> ProtonInstall {
    ProtonInstall {
        path: PathBuf::from("/usr/share/steam/compatibilitytools.d/proton-cachyos"),
        name: "proton-cachyos".to_string(),
        kind: ProtonKind::CachyOs,
        version: Some(format!("proton-cachyos {version}")),
        source: ProtonSource::SystemCompatibilityTools,
    }
}

fn ge(version: &str) -> ProtonInstall {
    ProtonInstall {
        path: PathBuf::from("/steam/compatibilitytools.d/GE-Proton"),
        name: "GE-Proton".to_string(),
        kind: ProtonKind::Ge,
        version: Some(format!("GE-Proton{version}")),
        source: ProtonSource::UserCompatibilityTools,
    }
}

fn valve(version: &str) -> ProtonInstall {
    ProtonInstall {
        path: PathBuf::from("/steam/steamapps/common/Proton 9.0"),
        name: "Proton 9.0".to_string(),
        kind: ProtonKind::Valve,
        version: Some(format!("Proton {version}")),
        source: ProtonSource::SteamCommon,
    }
}

/// Compile with a fixture GPU and (optionally) a compat entry and Proton.
fn run(
    profile: &Profile,
    gpu: &GpuProfile,
    compat: Option<&CompatEntry>,
    proton: Option<&ProtonInstall>,
) -> CompiledRuntime {
    compile(
        CompileRequest {
            profile,
            proton,
            log_dir: Some(Path::new("/data/pitagames/logs")),
            session: false,
        },
        GraphicsContext::new(gpu, compat),
    )
    .unwrap_or_else(|error| panic!("compile failed: {error}"))
}

/// A stable textual rendering of the compiled runtime for the snapshot.
fn render(compiled: &CompiledRuntime) -> String {
    let mut out = String::new();
    out.push_str("env:\n");
    for (key, value) in &compiled.env {
        out.push_str(&format!("  {key}={value}\n"));
    }
    out.push_str("wrappers:\n");
    for step in &compiled.wrappers {
        out.push_str(&format!("  {} {:?}\n", step.program.display(), step.args));
    }
    out.push_str("ini:\n");
    for setting in &compiled.ini {
        out.push_str(&format!(
            "  {}.{}={}\n",
            setting.section, setting.key, setting.value
        ));
    }
    out.push_str("warnings:\n");
    for warning in &compiled.warnings {
        out.push_str(&format!("  {warning}\n"));
    }
    out
}

fn nr() -> pitagames_core::profile::NeuralRendering {
    use pitagames_core::profile::NeuralRendering;
    NeuralRendering::DlssNrAmd {
        backend: NrBackend::Rdna4Upstream,
        version: "0.0.8".into(),
        route: NrRoute::Optiscaler,
        model: NrModel::CcSmallFp8,
        model_scale: 75,
        processing: NrProcessing::RenoDx,
        pkg_arch: PkgArch::X86_64,
        captured_env: std::collections::BTreeMap::from([(
            "WINEDLLOVERRIDES".to_string(),
            "dxgi=n,b".to_string(),
        )]),
        model_source: ModelSource::CachedInPackage,
    }
}

#[test]
fn rdna4_cachyos_fsr4_upgrade() {
    let mut profile = profile();
    profile.upscaler = Upscaler::Fsr4Upgrade {
        version: Some("4.0.3".into()),
    };
    profile.diagnostics.dxvk_hud = true;
    let compiled = run(
        &profile,
        &gpu(GpuArch::Rdna4),
        None,
        Some(&cachyos("11.0-20260516")),
    );
    insta::assert_snapshot!("rdna4_cachyos_fsr4_upgrade", render(&compiled));
}

#[test]
fn rdna4_cachyos_dlss_to_fsr4_with_fsr3_frame_gen() {
    let mut profile = profile();
    profile.upscaler = Upscaler::DlssToFsr4;
    profile.frame_gen = FrameGen::Fsr3;
    let compiled = run(
        &profile,
        &gpu(GpuArch::Rdna4),
        Some(&cyberpunk()),
        Some(&cachyos("11.0-20260516")),
    );
    insta::assert_snapshot!(
        "rdna4_cachyos_dlss_to_fsr4_with_fsr3_frame_gen",
        render(&compiled)
    );
}

#[test]
fn rdna4_cachyos_dlss_to_xess() {
    let mut profile = profile();
    profile.upscaler = Upscaler::DlssToXess;
    let compiled = run(
        &profile,
        &gpu(GpuArch::Rdna4),
        Some(&cyberpunk()),
        Some(&cachyos("11.0-20260516")),
    );
    insta::assert_snapshot!("rdna4_cachyos_dlss_to_xess", render(&compiled));
}

#[test]
fn rdna3_cachyos_fsr4_rdna3_upgrade() {
    let mut profile = profile();
    profile.upscaler = Upscaler::Fsr4Rdna3 { version: None };
    let compiled = run(
        &profile,
        &gpu(GpuArch::Rdna3),
        None,
        Some(&cachyos("11.0-20260516")),
    );
    insta::assert_snapshot!("rdna3_cachyos_fsr4_rdna3_upgrade", render(&compiled));
}

#[test]
fn rdna2_cachyos_ffx3_upgrade() {
    let mut profile = profile();
    profile.upscaler = Upscaler::Ffx3Upgrade;
    let compiled = run(
        &profile,
        &gpu(GpuArch::Rdna2),
        None,
        Some(&cachyos("11.0-20260516")),
    );
    insta::assert_snapshot!("rdna2_cachyos_ffx3_upgrade", render(&compiled));
}

#[test]
fn rdna1_ge_helixsr() {
    let mut profile = profile();
    profile.upscaler = Upscaler::HlxDlss;
    let compiled = run(
        &profile,
        &gpu(GpuArch::Rdna1),
        Some(&cyberpunk()),
        Some(&ge("10-5")),
    );
    insta::assert_snapshot!("rdna1_ge_helixsr", render(&compiled));
}

#[test]
fn bmg_cachyos_xess_upgrade() {
    let mut profile = profile();
    profile.upscaler = Upscaler::XessUpgrade;
    let compiled = run(&profile, &gpu(GpuArch::Bmg), Some(&cyberpunk()), None);
    insta::assert_snapshot!("bmg_cachyos_xess_upgrade", render(&compiled));
}

#[test]
fn rdna4_cachyos_ml_frame_generation() {
    let mut profile = profile();
    profile.frame_gen = FrameGen::Fsr4Mlfg;
    let compiled = run(
        &profile,
        &gpu(GpuArch::Rdna4),
        None,
        Some(&cachyos("11.0-20260516")),
    );
    insta::assert_snapshot!("rdna4_cachyos_ml_frame_generation", render(&compiled));
}

#[test]
fn rdna4_ge_neural_rendering() {
    let mut profile = profile();
    profile.neural_rendering = nr();
    let compiled = run(
        &profile,
        &gpu(GpuArch::Rdna4),
        Some(&cyberpunk()),
        Some(&ge("10-5")),
    );
    insta::assert_snapshot!("rdna4_ge_neural_rendering", render(&compiled));
}

#[test]
fn rdna4_cachyos_neural_rendering_warns() {
    let mut profile = profile();
    profile.neural_rendering = nr();
    let compiled = run(
        &profile,
        &gpu(GpuArch::Rdna4),
        Some(&cyberpunk()),
        Some(&cachyos("11.0-20260516")),
    );
    insta::assert_snapshot!("rdna4_cachyos_neural_rendering_warns", render(&compiled));
}

#[test]
fn rdna4_valve_tier_b_manual_injection() {
    let mut profile = profile();
    profile.injection = Injection::Manual;
    profile.injection_dll = InjectionDll::Winmm;
    let compiled = run(&profile, &gpu(GpuArch::Rdna4), None, Some(&valve("9.0")));
    insta::assert_snapshot!("rdna4_valve_tier_b_manual_injection", render(&compiled));
}

#[test]
fn an_unknown_card_compiles_the_empty_default() {
    let profile = profile();
    let compiled = run(&profile, &gpu(GpuArch::Unknown), None, None);
    insta::assert_snapshot!("unknown_card_default", render(&compiled));
}

#[test]
fn compat_env_is_applied_and_the_profile_still_wins() {
    let entry = CompatEntry::from_toml(
        "[game]\nstore = \"steam\"\nstore_id = \"1091500\"\ntitle = \"x\"\n\n\
         [runtime]\nenv = { DXVK_HDR = \"1\", DXVK_FRAME_RATE = \"60\" }\n",
    )
    .unwrap();
    let mut profile = profile();
    // The advanced override is appended last and wins at launch.
    profile.env.insert("DXVK_HDR".into(), "0".into());
    let compiled = run(&profile, &gpu(GpuArch::Rdna4), Some(&entry), None);
    insta::assert_snapshot!("compat_env_and_profile_override", render(&compiled));
}
