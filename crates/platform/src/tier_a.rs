//! Tier A: Proton's own, env-only OptiScaler hook.
//!
//! Proton-CachyOS (and umu-protonfixes) can deploy OptiScaler itself: the
//! launcher sets two environment variables and Proton downloads, verifies and
//! links the DLLs into the game's prefix. Nothing is written to the game
//! directory, so Tier A is fully reversible and has nothing to uninstall
//! (ADR-0023).
//!
//! ```text
//! PROTON_USE_OPTISCALER=1 PROTON_OPTISCALER_NAME=<name.dll> %command%
//! ```
//!
//! Two things are ours:
//!
//! - **The gate.** Tier A needs a Proton-CachyOS build at the hook version
//!   (`11.0-20260506`) or newer. An older build — or one whose version cannot be
//!   read — hides Tier A, and the Graphics screen offers Tier B instead
//!   ([`crate::capability`] turns [`TierAGate`] into the user-facing reason). The
//!   gate reads the *resolved* build's metadata through [`crate::proton`], never
//!   the profile's `ProtonRef`: only the concrete build knows its version.
//! - **The DLL name.** Proton injects one DLL, so the game's [`CompatEntry`]
//!   (`upscaling.preferred_dll`) picks it, with an ordered fallback to the names
//!   Tier A supports (`dxgi → d3d12 → dbghelp`, `dxgi.dll` by default).
//!
//! The module is framework-free and pure: a resolved [`ProtonInstall`] and a
//! [`CompatEntry`] in, an env vector or a verdict out. No path is ever written,
//! which is what makes "nothing is installed to disk on the Tier A path" true by
//! construction rather than by discipline.

use crate::compat::{CompatEntry, PreferredDll};
use crate::proton::{ProtonInstall, ProtonKind};

/// The env var that enables Proton-CachyOS's OptiScaler hook.
pub const USE_OPTISCALER: &str = "PROTON_USE_OPTISCALER";

/// The env var that selects the DLL Proton injects.
pub const OPTISCALER_NAME: &str = "PROTON_OPTISCALER_NAME";

/// The `Proton-CachyOS` build that first shipped the OptiScaler hook.
///
/// Also the floor the capability matrix uses for every other CachyOS-only path
/// (the FFX upgrade hooks, neural rendering), so the number lives in one place.
pub const CACHYOS_HOOKS_MIN: &[u64] = &[11, 0, 20260506];

/// The DLL names Tier A can inject, in preference order.
///
/// Proton-CachyOS accepts `dxgi.dll`, `d3d12.dll` and `dbghelp.dll`. The
/// `winmm`/`version` names the compat DB also offers are Tier B only, so a
/// per-game preference outside this set falls back to the ordered head.
pub const TIER_A_DLLS: [PreferredDll; 3] = [
    PreferredDll::Dxgi,
    PreferredDll::D3d12,
    PreferredDll::Dbghelp,
];

/// Why the Tier A hook may or may not be used for a resolved build.
///
/// [`crate::capability`] maps each verdict to one reason string, so the
/// Graphics screen and the compiler cannot disagree about whether Tier A is
/// offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierAGate {
    /// A Proton-CachyOS build at the hook version or newer.
    Supported,
    /// A Proton-CachyOS build older than the hook version.
    TooOld,
    /// A Proton-CachyOS build whose version could not be read.
    ///
    /// Never assumed to be new enough (plan §6): an unreadable version hides
    /// Tier A exactly like an old one.
    Unreadable,
    /// A non-CachyOS build (GE, Valve, umu, …): Tier A is Proton-CachyOS only.
    OtherBuild,
    /// Steam decides the runtime; there is no build to gate.
    SteamDefault,
}

impl TierAGate {
    /// Whether Tier A may be used for this build.
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

/// Gate a resolved Proton build for the Tier A hook.
pub fn gate(proton: Option<&ProtonInstall>) -> TierAGate {
    let Some(install) = proton else {
        return TierAGate::SteamDefault;
    };
    if install.kind != ProtonKind::CachyOs {
        return TierAGate::OtherBuild;
    }
    if install.version.is_none() {
        return TierAGate::Unreadable;
    }
    if install.version_key().as_slice() >= CACHYOS_HOOKS_MIN {
        TierAGate::Supported
    } else {
        TierAGate::TooOld
    }
}

/// The per-game DLL name Tier A injects.
///
/// The game's [`CompatEntry`] picks the DLL (`upscaling.preferred_dll`); a
/// preference Tier A cannot inject — `winmm.dll`/`version.dll`, which only Tier B
/// supports — or no preference at all falls back to the ordered list head,
/// `dxgi.dll`.
pub fn dll_name(compat: Option<&CompatEntry>) -> &'static str {
    compat
        .and_then(|entry| entry.upscaling.preferred_dll)
        .filter(|dll| TIER_A_DLLS.contains(dll))
        .unwrap_or(TIER_A_DLLS[0])
        .file_name()
}

/// The Tier A env for one game, as `(key, value)` pairs.
///
/// This is the whole of Tier A's contribution to a launch: Proton does the
/// deploying, we set the hook and the DLL name. The caller gates the build first
/// ([`gate`] through [`crate::capability::availability`]), so this function is
/// the emission only — it never touches the filesystem.
#[must_use]
pub fn env(compat: Option<&CompatEntry>) -> Vec<(String, String)> {
    vec![
        (USE_OPTISCALER.to_string(), "1".to_string()),
        (OPTISCALER_NAME.to_string(), dll_name(compat).to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::PreferredDll;
    use crate::proton::ProtonSource;
    use std::path::PathBuf;

    fn cachyos(version: Option<&str>) -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/usr/share/steam/compatibilitytools.d/proton-cachyos"),
            name: "proton-cachyos".to_string(),
            kind: ProtonKind::CachyOs,
            version: version.map(str::to_string),
            source: ProtonSource::SystemCompatibilityTools,
        }
    }

    fn other_build(version: Option<&str>) -> ProtonInstall {
        ProtonInstall {
            path: PathBuf::from("/steam/compatibilitytools.d/GE-Proton"),
            name: "GE-Proton".to_string(),
            kind: ProtonKind::Ge,
            version: version.map(str::to_string),
            source: ProtonSource::UserCompatibilityTools,
        }
    }

    /// A minimal compat entry with an optional preferred DLL.
    fn compat(dll: Option<PreferredDll>) -> CompatEntry {
        let preferred = match dll {
            Some(dll) => format!("preferred_dll = \"{}\"\n", dll.file_name()),
            None => String::new(),
        };
        CompatEntry::from_toml(&format!(
            "[game]\nstore = \"steam\"\nstore_id = \"1091500\"\ntitle = \"x\"\n\n\
             [upscaling]\n{preferred}"
        ))
        .unwrap()
    }

    // -- The gate -----------------------------------------------------------

    #[test]
    fn the_gate_accepts_the_hook_build_and_newer() {
        assert_eq!(
            gate(Some(&cachyos(Some("proton-cachyos 11.0-20260506")))),
            TierAGate::Supported
        );
        assert_eq!(
            gate(Some(&cachyos(Some("proton-cachyos 11.0-20260516")))),
            TierAGate::Supported
        );
        assert_eq!(
            gate(Some(&cachyos(Some("proton-cachyos 12.0-20270101")))),
            TierAGate::Supported
        );
        assert!(gate(Some(&cachyos(Some("proton-cachyos 11.0-20260506")))).is_supported());
    }

    #[test]
    fn the_gate_refuses_an_older_build() {
        assert_eq!(
            gate(Some(&cachyos(Some("proton-cachyos 10.0-20251101")))),
            TierAGate::TooOld
        );
        assert_eq!(
            gate(Some(&cachyos(Some("proton-cachyos 11.0-20260505")))),
            TierAGate::TooOld
        );
    }

    #[test]
    fn the_gate_refuses_an_unreadable_version() {
        // The `version` file is absent or unreadable: never assumed new enough.
        assert_eq!(gate(Some(&cachyos(None))), TierAGate::Unreadable);
    }

    #[test]
    fn the_gate_refuses_a_non_cachyos_build_and_steam_deciding() {
        assert_eq!(
            gate(Some(&other_build(Some("GE-Proton10-5")))),
            TierAGate::OtherBuild
        );
        // A non-CachyOS build with no version is the wrong build, not an
        // unreadable CachyOS one.
        assert_eq!(gate(Some(&other_build(None))), TierAGate::OtherBuild);
        assert_eq!(gate(None), TierAGate::SteamDefault);
    }

    // -- The DLL name -------------------------------------------------------

    #[test]
    fn the_dll_name_comes_from_the_compat_entry() {
        assert_eq!(
            dll_name(Some(&compat(Some(PreferredDll::Dbghelp)))),
            "dbghelp.dll"
        );
        assert_eq!(
            dll_name(Some(&compat(Some(PreferredDll::D3d12)))),
            "d3d12.dll"
        );
        assert_eq!(
            dll_name(Some(&compat(Some(PreferredDll::Dxgi)))),
            "dxgi.dll"
        );
    }

    #[test]
    fn a_dll_tier_a_cannot_inject_falls_back_to_the_ordered_head() {
        // `winmm`/`version` are Tier B names; Tier A injects `dxgi.dll` instead.
        assert_eq!(
            dll_name(Some(&compat(Some(PreferredDll::Winmm)))),
            "dxgi.dll"
        );
        assert_eq!(
            dll_name(Some(&compat(Some(PreferredDll::Version)))),
            "dxgi.dll"
        );
    }

    #[test]
    fn no_preference_or_no_entry_falls_back_to_dxgi() {
        assert_eq!(dll_name(Some(&compat(None))), "dxgi.dll");
        assert_eq!(dll_name(None), "dxgi.dll");
    }

    // -- The env ------------------------------------------------------------

    #[test]
    fn the_env_is_the_hook_and_the_dll_name() {
        assert_eq!(
            env(Some(&compat(Some(PreferredDll::Dbghelp)))),
            vec![
                ("PROTON_USE_OPTISCALER".to_string(), "1".to_string()),
                (
                    "PROTON_OPTISCALER_NAME".to_string(),
                    "dbghelp.dll".to_string()
                ),
            ]
        );
        assert_eq!(
            env(None),
            vec![
                ("PROTON_USE_OPTISCALER".to_string(), "1".to_string()),
                ("PROTON_OPTISCALER_NAME".to_string(), "dxgi.dll".to_string()),
            ]
        );
    }

    #[test]
    fn tier_a_sets_no_override_because_it_deploys_nothing() {
        // `WINEDLLOVERRIDES` is Tier B's mechanism: it only exists because Tier B
        // writes a DLL into the game directory. Tier A's env must not imply one.
        let env = env(Some(&compat(Some(PreferredDll::Dbghelp))));
        assert!(
            env.iter().all(|(key, _)| key != "WINEDLLOVERRIDES"),
            "{env:?}"
        );
    }
}
