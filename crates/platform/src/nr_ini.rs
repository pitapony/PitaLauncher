//! The safe `dlssnr-amd.ini` surface for a neural-rendering selection.
//!
//! DLSSNR-AMD replaces `dlssnr-amd.ini` with a fresh default file at install and
//! its in-game **NR Control** owns every setting, so the launcher may only touch
//! a few keys (ADR-0024, plan §7). The rule is data: [`surface`] is the whole
//! allowlist (`[Network] Model`/`model_scale`, `[Int4Mixed] Enabled`,
//! `[Preprocess] Enabled`/`Exposure`/`Curve`, `[NRControl] MenuKey`) and
//! [`settings`] composes *only* the keys the launcher actually has a selection
//! for, so a key the in-game menu owns is never clobbered:
//!
//! | Key | Written from |
//! |---|---|
//! | `[Network] Model` | the network selection ([`network_name`]) |
//! | `[Network] model_scale` | the `model_scale` selection (25..=100) |
//! | `[Int4Mixed] Enabled` | whether the network is int4 mixed |
//! | `[NRControl] MenuKey` | [`MENU_KEY`], the key that opens NR Control |
//!
//! `[Preprocess]` is on the surface (an advanced editor may set it) but is not
//! written by a plain NR install: it changes the picture NR is shown, so it
//! stays in the in-game menu unless the user changes it there.
//!
//! [`patch`] applies the settings through the shared line-preserving patcher, so
//! comments, ordering and CRLF survive and a key the surface does not allow is
//! refused by name. Framework-free and pure: a string goes in, a string comes
//! out.

use pitagames_core::profile::{NeuralRendering, NrModel};
use pitagames_mods::ini::{IniError, IniSetting, IniSurface};

/// The ini file these settings belong to.
pub const INI_FILE: &str = "dlssnr-amd.ini";

/// The in-game key that opens NR Control, upstream's default.
///
/// OptiScaler's own upscaler/frame-gen menu is `Insert`; NR Control is `End`
/// (plan §7 / upstream's README).
pub const MENU_KEY: &str = "End";

/// The shipped DLSSNR ini surface: the only sections and keys the launcher may
/// write.
pub fn surface() -> IniSurface {
    IniSurface::dlssnr()
}

/// Upstream's network name for a model variant, as `[Network] Model` spells it.
///
/// It is the name NVIDIA's DLL uses, not the installer's `--model` argument
/// (`CC_SMALL_FP8` vs `cc_small_fp8`); the installer accepts either.
pub const fn network_name(model: NrModel) -> &'static str {
    match model {
        NrModel::CcSmallFp8 => "CC_SMALL_FP8",
        NrModel::CcSilverAardwold => "CC_SILVER_AARDWOLD",
        NrModel::Int4Mixed => "CC_SILVER_AARDWOLD_INT4_MIXED",
        NrModel::SmallFp8Experimental => "CC_SMALL_FP8_EXPERIMENTAL",
    }
}

/// Whether a network variant is the int4-mixed network (`[Int4Mixed] Enabled`).
pub const fn int4_enabled(model: NrModel) -> bool {
    matches!(model, NrModel::Int4Mixed)
}

/// The allowlisted `dlssnr-amd.ini` settings for a network selection.
///
/// Only the keys the launcher has a selection for are produced; everything else
/// stays the in-game menu's.
pub fn settings(model: NrModel, model_scale: u8) -> Vec<IniSetting> {
    vec![
        IniSetting::new("Network", "Model", network_name(model)),
        IniSetting::new("Network", "model_scale", model_scale.to_string()),
        IniSetting::new("Int4Mixed", "Enabled", int4_enabled(model).to_string()),
        IniSetting::new("NRControl", "MenuKey", MENU_KEY),
    ]
}

/// The settings for a profile's neural-rendering value, or none when it is off.
pub fn for_profile(nr: &NeuralRendering) -> Vec<IniSetting> {
    match nr {
        NeuralRendering::DlssNrAmd {
            model, model_scale, ..
        } => settings(*model, *model_scale),
        NeuralRendering::Off => Vec::new(),
    }
}

/// Validate every setting against the shipped surface, refusing anything else.
pub fn validate(settings: &[IniSetting]) -> Result<(), IniError> {
    let surface = surface();
    for setting in settings {
        surface.validate(setting)?;
    }
    Ok(())
}

/// Apply the settings to an ini text, line-preserving, or refuse the whole patch.
///
/// Every setting is validated against [`surface`] first, so a non-allowlisted
/// key can never be written.
pub fn patch(text: &str, settings: &[IniSetting]) -> Result<String, IniError> {
    surface().patch(text, settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_fp8_selection_writes_only_the_allowlisted_keys() {
        let settings = settings(NrModel::CcSmallFp8, 75);
        let rendered: Vec<String> = settings
            .iter()
            .map(|s| format!("{}.{}={}", s.section, s.key, s.value))
            .collect();
        assert_eq!(
            rendered,
            [
                "Network.Model=CC_SMALL_FP8",
                "Network.model_scale=75",
                "Int4Mixed.Enabled=false",
                "NRControl.MenuKey=End",
            ]
        );
        // Everything the composition writes is on the surface.
        validate(&settings).expect("the composition is allowlisted");
    }

    #[test]
    fn the_int4_network_turns_the_int4_section_on() {
        let settings = settings(NrModel::Int4Mixed, 50);
        assert_eq!(
            network_name(NrModel::Int4Mixed),
            "CC_SILVER_AARDWOLD_INT4_MIXED"
        );
        assert!(int4_enabled(NrModel::Int4Mixed));
        let int4 = settings
            .iter()
            .find(|s| s.section == "Int4Mixed" && s.key == "Enabled")
            .expect("the int4 key");
        assert_eq!(int4.value, "true");
        validate(&settings).unwrap();
    }

    #[test]
    fn every_model_maps_to_its_upstream_network_name() {
        assert_eq!(network_name(NrModel::CcSmallFp8), "CC_SMALL_FP8");
        assert_eq!(
            network_name(NrModel::CcSilverAardwold),
            "CC_SILVER_AARDWOLD"
        );
        assert_eq!(
            network_name(NrModel::SmallFp8Experimental),
            "CC_SMALL_FP8_EXPERIMENTAL"
        );
        for model in [
            NrModel::CcSmallFp8,
            NrModel::CcSilverAardwold,
            NrModel::Int4Mixed,
            NrModel::SmallFp8Experimental,
        ] {
            validate(&settings(model, 100))
                .unwrap_or_else(|error| panic!("{model:?} composes a valid patch: {error}"));
        }
    }

    #[test]
    fn an_off_profile_writes_nothing() {
        assert!(for_profile(&NeuralRendering::Off).is_empty());
    }

    #[test]
    fn a_non_allowlisted_key_is_refused() {
        // The surface is the guarantee: a key it does not expose can never be
        // written, even if a caller builds the setting by hand.
        let smuggled = IniSetting::new("Network", "Secret", "x");
        assert!(validate(std::slice::from_ref(&smuggled)).is_err());
        assert!(patch("[Network]\nModel=CC_SMALL_FP8\n", &[smuggled]).is_err());
        // And a section the surface does not have.
        assert!(validate(&[IniSetting::new("Preprocess", "Inverse", "on")]).is_err());
    }

    #[test]
    fn patch_is_line_preserving() {
        let text = "; upstream defaults\r\n[Network]\r\nModel=CC_SMALL_FP8  ; the network\r\n";
        let patched = patch(text, &settings(NrModel::CcSilverAardwold, 50)).unwrap();
        assert!(
            patched.contains("Model=CC_SILVER_AARDWOLD  ; the network\r\n"),
            "{patched:?}"
        );
        assert!(patched.contains("model_scale=50"), "{patched:?}");
        assert!(patched.contains("[Int4Mixed]"), "{patched:?}");
    }

    #[test]
    fn the_surface_lists_the_documented_sections() {
        let surface = surface();
        for (section, key) in [
            ("Network", "Model"),
            ("Network", "model_scale"),
            ("Int4Mixed", "Enabled"),
            ("Preprocess", "Enabled"),
            ("Preprocess", "Exposure"),
            ("Preprocess", "Curve"),
            ("NRControl", "MenuKey"),
        ] {
            assert!(
                surface
                    .allowed_section(section)
                    .and_then(|s| s.key(key))
                    .is_some(),
                "the surface exposes [{section}] {key}"
            );
        }
    }
}
