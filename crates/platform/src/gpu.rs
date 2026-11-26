//! GPU detection for the graphics layer.
//!
//! The graphics work (M3) needs to know what card it is running on before it
//! can decide which upscalers, frame-generation paths and injection tiers are
//! available. This module is the detection half and *only* the detection half:
//! it reads the machine, it never decides availability (that is
//! `platform::capability`, built on `platform::compat`).
//!
//! Detection has two independent inputs and degrades gracefully between them:
//!
//! - **sysfs** (`/sys/class/drm/card*/device/`) gives the PCI vendor and device
//!   id and, on `amdgpu`, the VRAM size. The vendor/device pair is looked up in
//!   the [`const` table](arch_for) below, which works with no XDG directories
//!   and no subprocess.
//! - **`vulkaninfo --summary`** gives the marketing device name, the driver
//!   name and the Mesa version. It is optional: when the tool is missing the
//!   architecture still comes from the table and the VRAM still comes from
//!   sysfs, and only the Mesa version is reported as unknown rather than
//!   guessed.
//!
//! Everything host-specific is behind an injected [`GpuProbe`] (the sysfs root,
//! the captured `vulkaninfo` text, the `/dev/ntsync` and compositor hints), so
//! tests run against a fixture tree with no GPU and no `vulkaninfo` — the same
//! shape as [`crate::proton`]'s injected roots.
//!
//! Detection is read-only and side-effect free apart from [`GpuProbe::host`],
//! which runs `vulkaninfo` once and reads a couple of files.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::compat::{Arch, Version};

/// Where [`GpuProbe::host`] looks for DRM devices.
pub const SYS_ROOT: &str = "/sys";

/// The device node the ntfs-style synchronisation primitive registers.
pub const NTSYNC_DEVICE: &str = "/dev/ntsync";

/// The `$XDG_CURRENT_DESKTOP` value `host` reads for the HDR hint.
pub const DESKTOP_ENV: &str = "XDG_CURRENT_DESKTOP";

// ---------------------------------------------------------------------------
// Vendor and architecture
// ---------------------------------------------------------------------------

/// The PCI vendor behind a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuVendor {
    Amd,
    Intel,
    Nvidia,
    /// A vendor id we do not recognise (or no card at all).
    #[default]
    Unknown,
}

impl GpuVendor {
    /// Classify a raw PCI vendor id (`0x1002`, `0x8086`, `0x10de`).
    pub const fn from_id(vendor_id: u16) -> Self {
        match vendor_id {
            0x1002 => Self::Amd,
            0x8086 => Self::Intel,
            0x10de => Self::Nvidia,
            _ => Self::Unknown,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Amd => "amd",
            Self::Intel => "intel",
            Self::Nvidia => "nvidia",
            Self::Unknown => "unknown",
        }
    }

    /// The human label the graphics UI shows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Amd => "AMD",
            Self::Intel => "Intel",
            Self::Nvidia => "NVIDIA",
            Self::Unknown => "unknown vendor",
        }
    }
}

impl fmt::Display for GpuVendor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// A GPU architecture family, fine-grained enough to decide capability.
///
/// This is deliberately finer than [`crate::compat::Arch`]: the compatibility
/// database carries the *claim* and buckets a whole vendor family (`Rtx`,
/// `Arc`), while detection distinguishes the generations that decide which
/// frame-generation and neural-rendering paths exist. [`GpuArch::compat_arch`]
/// bridges the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuArch {
    Rdna4,
    Rdna3,
    Rdna2,
    Rdna1,
    /// Intel Arc "Alchemist" (DG2, A-series).
    Dg2,
    /// Intel Arc "Battlemage" (BMG, B-series).
    Bmg,
    /// NVIDIA Blackwell (RTX 50).
    Rtx50,
    /// NVIDIA Ada Lovelace (RTX 40).
    Rtx40,
    /// NVIDIA Ampere (RTX 30, and the GA107-based RTX 2050).
    Rtx30,
    /// NVIDIA Turing (RTX 20).
    Rtx20,
    /// NVIDIA Turing without tensor cores (GTX 16).
    Gtx16,
    /// NVIDIA Pascal (GTX 10).
    Gtx10,
    /// No card was found, or the device id is not in the table.
    #[default]
    Unknown,
}

impl GpuArch {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rdna4 => "rdna4",
            Self::Rdna3 => "rdna3",
            Self::Rdna2 => "rdna2",
            Self::Rdna1 => "rdna1",
            Self::Dg2 => "dg2",
            Self::Bmg => "bmg",
            Self::Rtx50 => "rtx50",
            Self::Rtx40 => "rtx40",
            Self::Rtx30 => "rtx30",
            Self::Rtx20 => "rtx20",
            Self::Gtx16 => "gtx16",
            Self::Gtx10 => "gtx10",
            Self::Unknown => "unknown",
        }
    }

    /// The human label the graphics UI shows.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Rdna4 => "RDNA 4 (RX 9000)",
            Self::Rdna3 => "RDNA 3 (RX 7000)",
            Self::Rdna2 => "RDNA 2 (RX 6000)",
            Self::Rdna1 => "RDNA 1 (RX 5000)",
            Self::Dg2 => "Intel Arc (Alchemist)",
            Self::Bmg => "Intel Arc (Battlemage)",
            Self::Rtx50 => "NVIDIA Blackwell (RTX 50)",
            Self::Rtx40 => "NVIDIA Ada (RTX 40)",
            Self::Rtx30 => "NVIDIA Ampere (RTX 30)",
            Self::Rtx20 => "NVIDIA Turing (RTX 20)",
            Self::Gtx16 => "NVIDIA Turing (GTX 16)",
            Self::Gtx10 => "NVIDIA Pascal (GTX 10)",
            Self::Unknown => "unknown architecture",
        }
    }

    /// The vendor that owns this architecture, or [`GpuVendor::Unknown`].
    pub const fn vendor(self) -> GpuVendor {
        match self {
            Self::Rdna1 | Self::Rdna2 | Self::Rdna3 | Self::Rdna4 => GpuVendor::Amd,
            Self::Dg2 | Self::Bmg => GpuVendor::Intel,
            Self::Rtx20 | Self::Rtx30 | Self::Rtx40 | Self::Rtx50 | Self::Gtx16 | Self::Gtx10 => {
                GpuVendor::Nvidia
            }
            Self::Unknown => GpuVendor::Unknown,
        }
    }

    /// The compatibility-database bucket this architecture maps to.
    ///
    /// `None` means the database has no bucket for it: the GTX cards predate
    /// tensor cores and the RTX line entirely, so no curated claim applies.
    pub const fn compat_arch(self) -> Option<Arch> {
        match self {
            Self::Rdna4 => Some(Arch::Rdna4),
            Self::Rdna3 => Some(Arch::Rdna3),
            Self::Rdna2 => Some(Arch::Rdna2),
            Self::Rdna1 => Some(Arch::Rdna1),
            Self::Dg2 | Self::Bmg => Some(Arch::Arc),
            Self::Rtx20 | Self::Rtx30 | Self::Rtx40 | Self::Rtx50 => Some(Arch::Rtx),
            Self::Gtx16 | Self::Gtx10 | Self::Unknown => None,
        }
    }
}

impl fmt::Display for GpuArch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// The vendor/device table
// ---------------------------------------------------------------------------
//
// Device ids are taken from the PCI ID database (pci.ids, 2026-11-25) and
// cover the shipping SKUs of each family. The table is intentionally data, not
// logic: a device id that is not listed degrades to `GpuArch::Unknown` with the
// vendor still known — it is never guessed from a neighbouring id.

/// AMD Navi 10/12/14 (RDNA 1).
const AMD_RDNA1: &[u16] = &[
    0x7310, 0x7312, 0x7319, 0x731b, 0x731f, 0x7340, 0x7341, 0x7347, 0x734f, 0x7360, 0x7362,
];

/// AMD Navi 21/22/23/24 (RDNA 2).
const AMD_RDNA2: &[u16] = &[
    0x73a1, 0x73a2, 0x73a3, 0x73a5, 0x73ab, 0x73ae, 0x73af, 0x73bf, 0x73c3, 0x73ce, 0x73df, 0x73e0,
    0x73e1, 0x73e3, 0x73ef, 0x73ff, 0x7421, 0x7422, 0x7423, 0x7424, 0x743f,
];

/// AMD Navi 31/32/33 (RDNA 3).
const AMD_RDNA3: &[u16] = &[
    0x73f0, 0x7448, 0x7449, 0x744a, 0x744b, 0x744c, 0x745e, 0x7460, 0x7461, 0x7470, 0x747e, 0x7480,
    0x7481, 0x7483, 0x7487, 0x7489, 0x748b, 0x7499, 0x749f,
];

/// AMD Navi 48/44 (RDNA 4).
const AMD_RDNA4: &[u16] = &[0x7550, 0x7551, 0x7590];

/// Intel Arc "Alchemist" (DG2).
const INTEL_DG2: &[u16] = &[
    0x4f80, 0x4f81, 0x4f82, 0x4f83, 0x4f84, 0x4f85, 0x4f86, 0x4f87, 0x4f88, 0x5690, 0x5691, 0x5692,
    0x5693, 0x5694, 0x5695, 0x5696, 0x5697, 0x5698, 0x56a0, 0x56a1, 0x56a2, 0x56a3, 0x56a4, 0x56a5,
    0x56a6, 0x56a7, 0x56a8, 0x56a9, 0x56b0, 0x56b1, 0x56b2, 0x56b3, 0x56ba, 0x56bb, 0x56bc, 0x56bd,
    0x56be, 0x56bf,
];

/// Intel Arc "Battlemage" (BMG).
const INTEL_BMG: &[u16] = &[
    0xb080, 0xb081, 0xb082, 0xb083, 0xe202, 0xe20b, 0xe20c, 0xe20d, 0xe210, 0xe211, 0xe212, 0xe215,
    0xe216, 0xe220, 0xe221, 0xe222, 0xe223,
];

/// NVIDIA Tesla/Volta consumer leftovers with no display path we care about are
/// intentionally absent; the RTX generations are grouped below by codename.
const NVIDIA_RTX50: &[u16] = &[
    0x22ce, 0x22d0, 0x2900, 0x2901, 0x2909, 0x290a, 0x2920, 0x2924, 0x2925, 0x293d, 0x2940, 0x2941,
    0x297e, 0x2980, 0x29bb, 0x29bc, 0x29c0, 0x29f1, 0x2b00, 0x2b85, 0x2b87, 0x2b8c, 0x2bb1, 0x2bb2,
    0x2bb3, 0x2bb4, 0x2bb5, 0x2bb9, 0x2bbc, 0x2bbd, 0x2c02, 0x2c05, 0x2c09, 0x2c18, 0x2c19, 0x2c2c,
    0x2c31, 0x2c33, 0x2c34, 0x2c38, 0x2c39, 0x2c3a, 0x2c58, 0x2c59, 0x2c77, 0x2c79, 0x2d04, 0x2d05,
    0x2d18, 0x2d19, 0x2d2c, 0x2d30, 0x2d39, 0x2d58, 0x2d59, 0x2d79, 0x2d83, 0x2d98, 0x2db8, 0x2db9,
    0x2dd8, 0x2df9, 0x2e02, 0x2e03, 0x2e06, 0x2e12, 0x2e13, 0x2e2a, 0x2f02, 0x2f04, 0x2f06, 0x2f18,
    0x2f38, 0x2f58, 0x3180, 0x3182, 0x3183, 0x3184, 0x31a1, 0x31c0, 0x31c2, 0x31c3, 0x31fe, 0x3200,
    0x3224, 0x323e, 0x3340, 0x3400,
];

const NVIDIA_RTX40: &[u16] = &[
    0x2681, 0x2682, 0x2684, 0x2685, 0x2687, 0x2689, 0x26af, 0x26b1, 0x26b2, 0x26b3, 0x26b5, 0x26b7,
    0x26b8, 0x26b9, 0x26ba, 0x26bb, 0x26f5, 0x2702, 0x2703, 0x2704, 0x2705, 0x2709, 0x2717, 0x2730,
    0x2757, 0x2770, 0x2782, 0x2783, 0x2785, 0x2786, 0x2788, 0x27a0, 0x27b0, 0x27b1, 0x27b2, 0x27b6,
    0x27b7, 0x27b8, 0x27ba, 0x27bb, 0x27e0, 0x27fa, 0x27fb, 0x2803, 0x2805, 0x2808, 0x2820, 0x2822,
    0x2838, 0x2860, 0x2878, 0x2882, 0x28a0, 0x28a1, 0x28a3, 0x28b0, 0x28b8, 0x28b9, 0x28ba, 0x28bb,
    0x28e0, 0x28e1, 0x28e3, 0x28f8,
];

const NVIDIA_RTX30: &[u16] = &[
    0x2080, 0x2081, 0x2082, 0x20b0, 0x20b1, 0x20b2, 0x20b3, 0x20b4, 0x20b5, 0x20b6, 0x20b7, 0x20b8,
    0x20b9, 0x20bb, 0x20bd, 0x20be, 0x20bf, 0x20c0, 0x20c2, 0x20f0, 0x20f1, 0x20f2, 0x20f3, 0x20f5,
    0x20f6, 0x20fd, 0x20fe, 0x20ff, 0x2200, 0x2203, 0x2204, 0x2205, 0x2206, 0x2207, 0x2208, 0x220a,
    0x220d, 0x2216, 0x222b, 0x222f, 0x2230, 0x2231, 0x2232, 0x2233, 0x2235, 0x2236, 0x2237, 0x2238,
    0x223f, 0x2414, 0x2420, 0x2438, 0x2460, 0x2480, 0x2482, 0x2483, 0x2484, 0x2486, 0x2487, 0x2488,
    0x2489, 0x248a, 0x248c, 0x248d, 0x248e, 0x249c, 0x249d, 0x249f, 0x24a0, 0x24a4, 0x24ac, 0x24ad,
    0x24af, 0x24b0, 0x24b1, 0x24b6, 0x24b7, 0x24b8, 0x24b9, 0x24ba, 0x24bb, 0x24bf, 0x24c0, 0x24c7,
    0x24c8, 0x24c9, 0x24dc, 0x24dd, 0x24df, 0x24e0, 0x24fa, 0x2501, 0x2503, 0x2504, 0x2505, 0x2507,
    0x2508, 0x2509, 0x2520, 0x2521, 0x2523, 0x252f, 0x2531, 0x2544, 0x2560, 0x2561, 0x2563, 0x2571,
    0x2582, 0x2583, 0x2584, 0x25a0, 0x25a2, 0x25a3, 0x25a4, 0x25a5, 0x25a6, 0x25a7, 0x25a9, 0x25aa,
    0x25ab, 0x25ac, 0x25ad, 0x25af, 0x25b0, 0x25b2, 0x25b5, 0x25b6, 0x25b8, 0x25b9, 0x25ba, 0x25bb,
    0x25bc, 0x25bd, 0x25e0, 0x25e2, 0x25e5, 0x25ec, 0x25ed, 0x25f9, 0x25fa, 0x25fb,
];

const NVIDIA_RTX20: &[u16] = &[
    0x1e02, 0x1e03, 0x1e04, 0x1e07, 0x1e09, 0x1e2d, 0x1e2e, 0x1e30, 0x1e35, 0x1e36, 0x1e37, 0x1e38,
    0x1e3c, 0x1e3d, 0x1e3e, 0x1e78, 0x1e81, 0x1e82, 0x1e84, 0x1e87, 0x1e89, 0x1e90, 0x1e91, 0x1e93,
    0x1eab, 0x1eb0, 0x1eb1, 0x1eb4, 0x1eb5, 0x1eb6, 0x1eb8, 0x1eb9, 0x1eba, 0x1ebe, 0x1ec2, 0x1ec7,
    0x1ed0, 0x1ed1, 0x1ed3, 0x1ef5, 0x1f02, 0x1f03, 0x1f04, 0x1f06, 0x1f07, 0x1f08, 0x1f0b, 0x1f10,
    0x1f11, 0x1f12, 0x1f14, 0x1f15, 0x1f2e, 0x1f36, 0x1f42, 0x1f47, 0x1f50, 0x1f51, 0x1f54, 0x1f55,
    0x1f76, 0x1f81, 0x1f97, 0x1f98, 0x1f9c, 0x1f9f, 0x1fa0, 0x1fa1, 0x1fae, 0x1fb0, 0x1fb1, 0x1fb2,
    0x1fb6, 0x1fb7, 0x1fb8, 0x1fb9, 0x1fba, 0x1fbb, 0x1fbc, 0x1fbf, 0x1ff0, 0x1ff2, 0x1ff9, 0x2183,
    0x2186, 0x2189, 0x21ae, 0x21bf, 0x21c2, 0x21c3,
];

const NVIDIA_GTX16: &[u16] = &[
    0x1eae, 0x1f09, 0x1f0a, 0x1f82, 0x1f83, 0x1f91, 0x1f92, 0x1f94, 0x1f95, 0x1f96, 0x1f99, 0x1f9d,
    0x1fd9, 0x1fdd, 0x2182, 0x2184, 0x2187, 0x2188, 0x2191, 0x2192, 0x21c4, 0x21d1,
];

const NVIDIA_GTX10: &[u16] = &[
    0x15c2, 0x15f0, 0x15f1, 0x15f7, 0x15f8, 0x15f9, 0x15fa, 0x15fb, 0x15fc, 0x15ff, 0x1725, 0x172e,
    0x172f, 0x1b00, 0x1b01, 0x1b02, 0x1b04, 0x1b06, 0x1b07, 0x1b30, 0x1b38, 0x1b39, 0x1b70, 0x1b78,
    0x1b80, 0x1b81, 0x1b82, 0x1b83, 0x1b84, 0x1b87, 0x1ba0, 0x1ba1, 0x1ba2, 0x1ba9, 0x1baa, 0x1bad,
    0x1bb0, 0x1bb1, 0x1bb3, 0x1bb4, 0x1bb5, 0x1bb6, 0x1bb7, 0x1bb8, 0x1bb9, 0x1bbb, 0x1bc7, 0x1be0,
    0x1be1, 0x1c00, 0x1c01, 0x1c02, 0x1c03, 0x1c04, 0x1c06, 0x1c07, 0x1c09, 0x1c20, 0x1c21, 0x1c22,
    0x1c23, 0x1c2d, 0x1c30, 0x1c31, 0x1c35, 0x1c36, 0x1c60, 0x1c61, 0x1c62, 0x1c70, 0x1c81, 0x1c82,
    0x1c83, 0x1c8c, 0x1c8d, 0x1c8e, 0x1c8f, 0x1c90, 0x1c91, 0x1c92, 0x1c94, 0x1c96, 0x1ca7, 0x1ca8,
    0x1caa, 0x1cb1, 0x1cb2, 0x1cb3, 0x1cb6, 0x1cba, 0x1cbb, 0x1cbc, 0x1cbd, 0x1ccc, 0x1ccd, 0x1cfa,
    0x1cfb, 0x1d01, 0x1d02, 0x1d10, 0x1d11, 0x1d12, 0x1d13, 0x1d16, 0x1d33, 0x1d34, 0x1d52, 0x1d56,
];

/// Look up a device id in the table for one vendor.
fn arch_for_device(ids: &[u16], device: u16, arch: GpuArch) -> Option<GpuArch> {
    ids.contains(&device).then_some(arch)
}

/// Map a PCI vendor/device pair to an architecture.
///
/// The lookup is vendor-scoped, so an AMD id can never be read as an NVIDIA one
/// (their id ranges overlap in places). An unknown pair yields
/// [`GpuArch::Unknown`] rather than a guess.
pub fn arch_for(vendor_id: u16, device_id: u16) -> GpuArch {
    let table: &[(&[u16], GpuArch)] = match vendor_id {
        0x1002 => &[
            (AMD_RDNA4, GpuArch::Rdna4),
            (AMD_RDNA3, GpuArch::Rdna3),
            (AMD_RDNA2, GpuArch::Rdna2),
            (AMD_RDNA1, GpuArch::Rdna1),
        ],
        0x8086 => &[(INTEL_BMG, GpuArch::Bmg), (INTEL_DG2, GpuArch::Dg2)],
        0x10de => &[
            (NVIDIA_RTX50, GpuArch::Rtx50),
            (NVIDIA_RTX40, GpuArch::Rtx40),
            (NVIDIA_RTX30, GpuArch::Rtx30),
            (NVIDIA_RTX20, GpuArch::Rtx20),
            (NVIDIA_GTX16, GpuArch::Gtx16),
            (NVIDIA_GTX10, GpuArch::Gtx10),
        ],
        _ => &[],
    };

    table
        .iter()
        .find_map(|(ids, arch)| arch_for_device(ids, device_id, *arch))
        .unwrap_or(GpuArch::Unknown)
}

// ---------------------------------------------------------------------------
// HDR hint
// ---------------------------------------------------------------------------

/// Whether the running compositor can present HDR.
///
/// This is a *hint* from `$XDG_CURRENT_DESKTOP`, not a capability claim: it
/// only says the compositor is one of the known HDR-capable desktops, which is
/// enough to offer `DXVK_HDR=1`. The display and the game still have to agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HdrSupport {
    Supported,
    /// A desktop is running and it is not one of the known HDR-capable ones.
    Unsupported,
    /// No desktop was detected (a bare TTY, a container, or no env var).
    #[default]
    Unknown,
}

impl HdrSupport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }

    /// Classify `$XDG_CURRENT_DESKTOP`.
    pub fn from_desktop(desktop: Option<&str>) -> Self {
        let Some(desktop) = desktop.map(str::trim).filter(|value| !value.is_empty()) else {
            return Self::Unknown;
        };
        let desktop = desktop.to_ascii_lowercase();
        if desktop.contains("kde")
            || desktop.contains("kwin")
            || desktop.contains("gnome")
            || desktop.contains("mango")
        {
            Self::Supported
        } else {
            Self::Unsupported
        }
    }
}

// ---------------------------------------------------------------------------
// The injected probe
// ---------------------------------------------------------------------------

/// Everything [`detect`] reads, injected so tests need no hardware.
///
/// [`GpuProbe::host`] fills this from the running machine; a test (or the CLI's
/// `--root`) builds it from a fixture tree instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuProbe {
    /// The sysfs root; `/sys` on a real machine, a fixture directory in tests.
    /// DRM devices are read from `<sys_root>/class/drm/card*`.
    pub sys_root: PathBuf,
    /// Raw `vulkaninfo --summary` output, or `None` when the tool is absent.
    pub vulkaninfo: Option<String>,
    /// Whether `/dev/ntsync` exists.
    pub ntsync: bool,
    /// The current `$XDG_CURRENT_DESKTOP`, for the HDR hint.
    pub desktop: Option<String>,
}

impl GpuProbe {
    /// A probe with an explicit sysfs root and no optional inputs.
    ///
    /// This is the fixture constructor: no `vulkaninfo`, no `ntsync`, no
    /// desktop. Add them with the `with_*` builders.
    pub fn at(sys_root: impl Into<PathBuf>) -> Self {
        Self {
            sys_root: sys_root.into(),
            vulkaninfo: None,
            ntsync: false,
            desktop: None,
        }
    }

    /// Probe the running machine.
    ///
    /// Reads `/sys`, checks for `/dev/ntsync`, reads `$XDG_CURRENT_DESKTOP` and
    /// runs `vulkaninfo --summary` once. A missing or failing `vulkaninfo` is
    /// not an error: the field is left `None` and detection degrades.
    pub fn host() -> Self {
        Self {
            sys_root: PathBuf::from(SYS_ROOT),
            vulkaninfo: run_vulkaninfo(),
            ntsync: Path::new(NTSYNC_DEVICE).exists(),
            desktop: std::env::var(DESKTOP_ENV).ok(),
        }
    }

    /// Attach captured `vulkaninfo --summary` text.
    #[must_use]
    pub fn with_vulkaninfo(mut self, text: impl Into<String>) -> Self {
        self.vulkaninfo = Some(text.into());
        self
    }

    /// Set the `/dev/ntsync` hint.
    #[must_use]
    pub fn with_ntsync(mut self, present: bool) -> Self {
        self.ntsync = present;
        self
    }

    /// Set the `$XDG_CURRENT_DESKTOP` hint.
    #[must_use]
    pub fn with_desktop(mut self, desktop: impl Into<String>) -> Self {
        self.desktop = Some(desktop.into());
        self
    }
}

/// Run `vulkaninfo --summary`, returning its stdout when it succeeds.
fn run_vulkaninfo() -> Option<String> {
    let output = Command::new("vulkaninfo").arg("--summary").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    (!text.trim().is_empty()).then_some(text)
}

// ---------------------------------------------------------------------------
// The detected profile
// ---------------------------------------------------------------------------

/// What the launcher knows about the machine's GPU.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GpuProfile {
    pub vendor: GpuVendor,
    pub arch: GpuArch,
    /// The PCI device id, when a DRM card or `vulkaninfo` supplied one.
    pub device_id: Option<u16>,
    /// The marketing name, only ever from `vulkaninfo`.
    pub name: Option<String>,
    /// The Mesa version, only ever from `vulkaninfo` (never guessed).
    pub mesa: Option<Version>,
    /// Total VRAM in bytes, from `amdgpu` sysfs.
    pub vram_bytes: Option<u64>,
    /// Whether `/dev/ntsync` exists.
    pub ntsync: bool,
    pub hdr: HdrSupport,
    /// Non-fatal problems (missing `vulkaninfo`, an unreadable card, a device
    /// id not in the table) worth showing next to the result.
    pub warnings: Vec<String>,
}

impl GpuProfile {
    /// VRAM in gibibytes, for display.
    pub fn vram_gib(&self) -> Option<f64> {
        self.vram_bytes
            .map(|bytes| bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    }

    /// The environment hints detection can offer, as `(key, value)` pairs.
    ///
    /// Only hints a present device supports: ntsync when `/dev/ntsync` exists,
    /// HDR when the compositor is known HDR-capable. The graphics layer decides
    /// whether to apply them.
    pub fn suggested_env(&self) -> Vec<(&'static str, &'static str)> {
        let mut env = Vec::new();
        if self.ntsync {
            env.push(("PROTON_USE_NTSYNC", "1"));
        }
        if self.hdr == HdrSupport::Supported {
            env.push(("DXVK_HDR", "1"));
        }
        env
    }
}

/// Detect the GPU described by `probe`.
#[must_use]
pub fn detect(probe: &GpuProbe) -> GpuProfile {
    let mut warnings = Vec::new();

    let cards = read_cards(&probe.sys_root, &mut warnings);
    let card = pick_primary(&cards);
    let card_device_id = card.and_then(|card| card.device_id);
    let vulkan_gpus = probe
        .vulkaninfo
        .as_deref()
        .map(parse_vulkaninfo)
        .unwrap_or_default();
    let vulkan = select_vulkan(&vulkan_gpus, card_device_id);

    // Prefer the DRM card's ids; fall back to the Vulkan device when sysfs told
    // us nothing (a container with a passthrough GPU, a fixture with no tree).
    let vendor_id = card
        .and_then(|card| card.vendor_id)
        .or_else(|| vulkan.and_then(|vulkan| vulkan.vendor_id));
    let device_id = card_device_id.or_else(|| vulkan.and_then(|vulkan| vulkan.device_id));
    let vendor = vendor_id.map(GpuVendor::from_id).unwrap_or_default();
    let arch = match (vendor_id, device_id) {
        (Some(vendor_id), Some(device_id)) => arch_for(vendor_id, device_id),
        _ => GpuArch::Unknown,
    };

    if probe.vulkaninfo.is_none() {
        warnings.push("vulkaninfo not available: Mesa version unknown".to_string());
    }
    if cards.is_empty() {
        warnings.push(format!(
            "no DRM card found under {}",
            drm_dir(&probe.sys_root).display()
        ));
    } else if device_id.is_some() && arch == GpuArch::Unknown {
        warnings.push(format!(
            "device id 0x{:04x} for {} is not in the GPU table",
            device_id.unwrap_or(0),
            vendor.label()
        ));
    }

    GpuProfile {
        vendor,
        arch,
        device_id,
        name: vulkan.and_then(|vulkan| vulkan.name.clone()),
        mesa: vulkan.and_then(|vulkan| vulkan.mesa),
        vram_bytes: card.and_then(|card| card.vram_bytes),
        ntsync: probe.ntsync,
        hdr: HdrSupport::from_desktop(probe.desktop.as_deref()),
        warnings,
    }
}

fn drm_dir(sys_root: &Path) -> PathBuf {
    sys_root.join("class/drm")
}

/// One DRM card read from sysfs.
#[derive(Debug, Clone)]
struct Card {
    vendor_id: Option<u16>,
    device_id: Option<u16>,
    vram_bytes: Option<u64>,
}

impl Card {
    /// The architecture this card maps to, or [`GpuArch::Unknown`].
    fn arch(&self) -> GpuArch {
        match (self.vendor_id, self.device_id) {
            (Some(vendor_id), Some(device_id)) => arch_for(vendor_id, device_id),
            _ => GpuArch::Unknown,
        }
    }
}

/// Read every `card<N>` under `<sys_root>/class/drm`, sorted by name.
///
/// Connector directories (`card0-DP-1`) and render nodes are ignored; a card
/// with neither a vendor nor a device file (a virtual device) is skipped. A
/// malformed id file is a warning, not a failure.
fn read_cards(sys_root: &Path, warnings: &mut Vec<String>) -> Vec<Card> {
    let dir = drm_dir(sys_root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_card_name(name))
        .collect();
    names.sort();

    let mut cards = Vec::new();
    for name in names {
        let device = dir.join(&name).join("device");
        let vendor_id = read_hex_id(&device.join("vendor"), warnings);
        let device_id = read_hex_id(&device.join("device"), warnings);
        if vendor_id.is_none() && device_id.is_none() {
            continue;
        }
        let vram_bytes = read_u64(&device.join("mem_info_vram_total"))
            .or_else(|| read_u64(&device.join("mem_info_vis_vram_total")));
        cards.push(Card {
            vendor_id,
            device_id,
            vram_bytes,
        });
    }
    cards
}

/// `card0`, `card12` yes; `card0-DP-1`, `renderD128` no.
fn is_card_name(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The card detection should report: the first recognised GPU, then the first
/// card from a known vendor, then simply the first card.
fn pick_primary(cards: &[Card]) -> Option<&Card> {
    cards
        .iter()
        .find(|card| card.arch() != GpuArch::Unknown)
        .or_else(|| {
            cards.iter().find(|card| {
                card.vendor_id
                    .is_some_and(|id| GpuVendor::from_id(id) != GpuVendor::Unknown)
            })
        })
        .or_else(|| cards.first())
}

/// Read a `0x1234`-style hex id file. A missing file is silent; a file that is
/// present but unreadable or malformed is a warning.
fn read_hex_id(path: &Path, warnings: &mut Vec<String>) -> Option<u16> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            warnings.push(format!("{}: {error}", path.display()));
            return None;
        }
    };
    match parse_hex_id(&text) {
        Some(id) => Some(id),
        None => {
            warnings.push(format!(
                "{}: not a hex id (`{}`)",
                path.display(),
                text.trim()
            ));
            None
        }
    }
}

fn parse_hex_id(text: &str) -> Option<u16> {
    let text = text.trim();
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u16::from_str_radix(hex, 16).ok()
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The parts of one `vulkaninfo --summary` device block we use.
#[derive(Debug, Clone, Default)]
struct VulkanGpu {
    vendor_id: Option<u16>,
    device_id: Option<u16>,
    name: Option<String>,
    mesa: Option<Version>,
}

/// Pick the `vulkaninfo` block for our card, falling back to the first.
///
/// A laptop reports an iGPU and a dGPU; the one whose device id matches the DRM
/// card we chose is the right block. With no device id to match, the first
/// block wins (the common single-GPU case).
fn select_vulkan(gpus: &[VulkanGpu], device_id: Option<u16>) -> Option<&VulkanGpu> {
    if let Some(device_id) = device_id
        && let Some(found) = gpus.iter().find(|gpu| gpu.device_id == Some(device_id))
    {
        return Some(found);
    }
    gpus.first()
}

/// Parse every `vulkaninfo --summary` device block.
///
/// The summary format is a `GPU<n>:` header followed by `key = value` lines.
/// Unknown keys are ignored, so a newer `vulkaninfo` cannot break detection.
fn parse_vulkaninfo(text: &str) -> Vec<VulkanGpu> {
    let mut gpus: Vec<VulkanGpu> = Vec::new();
    let mut current: Option<VulkanGpu> = None;
    let mut fields: Vec<(String, String)> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if is_gpu_header(trimmed) {
            if let Some(mut gpu) = current.take() {
                gpu.mesa = mesa_from(&fields);
                gpus.push(gpu);
            }
            current = Some(VulkanGpu::default());
            fields = Vec::new();
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim().to_string();
        if let Some(gpu) = current.as_mut() {
            match key.as_str() {
                "vendorID" => gpu.vendor_id = parse_hex_id(&value),
                "deviceID" => gpu.device_id = parse_hex_id(&value),
                "deviceName" => gpu.name = Some(value.clone()),
                _ => {}
            }
        }
        fields.push((key, value));
    }
    if let Some(mut gpu) = current.take() {
        gpu.mesa = mesa_from(&fields);
        gpus.push(gpu);
    }

    gpus
}

/// `GPU0:`, `GPU1:` — the summary's device block header.
fn is_gpu_header(line: &str) -> bool {
    line.strip_prefix("GPU")
        .and_then(|rest| rest.strip_suffix(':'))
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Pull the Mesa version out of a block's `driverInfo` / `driverVersion`.
///
/// Only Mesa's own drivers report a Mesa version; an NVIDIA proprietary or
/// unknown driver yields `None` rather than a made-up value.
fn mesa_from(fields: &[(String, String)]) -> Option<Version> {
    let get = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };

    if let Some(info) = get("driverInfo")
        && let Some(rest) = info.split_once("Mesa").map(|(_, rest)| rest)
        && let Some(version) = leading_version(rest)
    {
        return Some(version);
    }

    let driver_id = get("driverID").unwrap_or_default().to_ascii_uppercase();
    if driver_id.contains("MESA")
        && let Some(version) = get("driverVersion").and_then(Version::parse)
        && version.major >= 20
    {
        return Some(version);
    }
    None
}

/// Parse the `major.minor[.patch]` prefix of a token like `24.2.0-devel`.
fn leading_version(text: &str) -> Option<Version> {
    let token: String = text
        .trim()
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect();
    Version::parse(token.trim_end_matches('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `vulkaninfo --summary` block for the in-house RX 9070 XT.
    const VULKANINFO_RX9070: &str = "\
==========
VULKANINFO
==========

Vulkan Instance Version: 1.4.321

Devices:
========
GPU0:
\tapiVersion         = 1.4.321
\tdriverVersion      = 25.3.0
\tvendorID           = 0x1002
\tdeviceID           = 0x7550
\tdeviceType         = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
\tdeviceName         = AMD Radeon RX 9070 XT (RADV NAVI48)
\tdriverID           = DRIVER_ID_MESA_RADV
\tdriverName         = radv
\tdriverInfo         = Mesa 25.3.0
\tconformanceVersion = 1.4.0.0
";

    /// An AMD iGPU plus the NVIDIA discrete card, as a laptop reports them.
    const VULKANINFO_TWO_GPUS: &str = "\
Devices:
========
GPU0:
\tapiVersion         = 1.3.280
\tdriverVersion      = 25.3.0
\tvendorID           = 0x1002
\tdeviceID           = 0x15bf
\tdeviceType         = PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU
\tdeviceName         = AMD Radeon 780M (RADV PHOENIX)
\tdriverID           = DRIVER_ID_MESA_RADV
\tdriverName         = radv
\tdriverInfo         = Mesa 25.3.0
GPU1:
\tapiVersion         = 1.4.312
\tdriverVersion      = 580.65.06
\tvendorID           = 0x10de
\tdeviceID           = 0x2684
\tdeviceType         = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
\tdeviceName         = NVIDIA GeForce RTX 4090
\tdriverID           = DRIVER_ID_NVIDIA_PROPRIETARY
\tdriverName         = NVIDIA
\tdriverInfo         = 580.65.06
";

    fn write_card(root: &Path, card: &str, vendor: u16, device: u16, vram: Option<u64>) {
        let dir = root.join("class/drm").join(card).join("device");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("vendor"), format!("0x{vendor:04x}\n")).unwrap();
        fs::write(dir.join("device"), format!("0x{device:04x}\n")).unwrap();
        if let Some(vram) = vram {
            fs::write(dir.join("mem_info_vram_total"), format!("{vram}\n")).unwrap();
        }
    }

    #[test]
    fn detects_an_amd_rdna4_card_from_sysfs_and_vulkaninfo() {
        let temp = tempfile::tempdir().unwrap();
        write_card(temp.path(), "card1", 0x1002, 0x7550, Some(17_179_869_184));
        let probe = GpuProbe::at(temp.path()).with_vulkaninfo(VULKANINFO_RX9070);

        let profile = detect(&probe);

        assert_eq!(profile.vendor, GpuVendor::Amd);
        assert_eq!(profile.arch, GpuArch::Rdna4);
        assert_eq!(profile.device_id, Some(0x7550));
        assert_eq!(
            profile.name.as_deref(),
            Some("AMD Radeon RX 9070 XT (RADV NAVI48)")
        );
        assert_eq!(profile.mesa, Version::parse("25.3.0"));
        assert_eq!(profile.vram_bytes, Some(17_179_869_184));
        assert_eq!(profile.vram_gib(), Some(16.0));
        assert!(profile.warnings.is_empty(), "{:?}", profile.warnings);
    }

    #[test]
    fn picks_the_vulkaninfo_block_that_matches_the_primary_card() {
        let temp = tempfile::tempdir().unwrap();
        write_card(temp.path(), "card0", 0x10de, 0x2684, None);
        let probe = GpuProbe::at(temp.path()).with_vulkaninfo(VULKANINFO_TWO_GPUS);

        let profile = detect(&probe);

        assert_eq!(profile.vendor, GpuVendor::Nvidia);
        assert_eq!(profile.arch, GpuArch::Rtx40);
        assert_eq!(
            profile.name.as_deref(),
            Some("NVIDIA GeForce RTX 4090"),
            "the discrete card's block, not the iGPU's, must be used"
        );
        assert_eq!(profile.mesa, None, "the NVIDIA driver reports no Mesa");
    }

    #[test]
    fn a_missing_vulkaninfo_degrades_to_the_table_and_sysfs() {
        let temp = tempfile::tempdir().unwrap();
        // The RX 5700 XT: our in-house RDNA 1 target, 8 GiB.
        write_card(temp.path(), "card0", 0x1002, 0x731f, Some(8_589_934_592));
        let probe = GpuProbe::at(temp.path());

        let profile = detect(&probe);

        assert_eq!(profile.vendor, GpuVendor::Amd);
        assert_eq!(profile.arch, GpuArch::Rdna1, "arch comes from the table");
        assert_eq!(profile.device_id, Some(0x731f));
        assert_eq!(profile.name, None, "no marketing name without vulkaninfo");
        assert_eq!(profile.mesa, None, "the Mesa version is never guessed");
        assert_eq!(
            profile.vram_bytes,
            Some(8_589_934_592),
            "VRAM comes from sysfs"
        );
        assert!(
            profile
                .warnings
                .iter()
                .any(|line| line.contains("vulkaninfo")),
            "{:?}",
            profile.warnings
        );
    }

    #[test]
    fn the_table_covers_every_family_we_claim() {
        // AMD: the four RDNA generations, anchored on the in-house targets.
        assert_eq!(arch_for(0x1002, 0x7550), GpuArch::Rdna4, "RX 9070 XT");
        assert_eq!(arch_for(0x1002, 0x744c), GpuArch::Rdna3, "RX 7900 XTX");
        assert_eq!(arch_for(0x1002, 0x73bf), GpuArch::Rdna2, "RX 6800 XT");
        assert_eq!(arch_for(0x1002, 0x731f), GpuArch::Rdna1, "RX 5700 XT");
        // Intel: DG2 (A-series) and BMG (B-series).
        assert_eq!(arch_for(0x8086, 0x56a0), GpuArch::Dg2, "Arc A770");
        assert_eq!(arch_for(0x8086, 0xe20b), GpuArch::Bmg, "Arc B580");
        // NVIDIA: every RTX generation, plus the GTX lines the plan names.
        assert_eq!(arch_for(0x10de, 0x1e04), GpuArch::Rtx20, "RTX 2080 Ti");
        assert_eq!(arch_for(0x10de, 0x2482), GpuArch::Rtx30, "RTX 3070");
        assert_eq!(arch_for(0x10de, 0x2684), GpuArch::Rtx40, "RTX 4090");
        assert_eq!(arch_for(0x10de, 0x2b85), GpuArch::Rtx50, "RTX 5090");
        assert_eq!(arch_for(0x10de, 0x2182), GpuArch::Gtx16, "GTX 1660 Ti");
        assert_eq!(arch_for(0x10de, 0x1c82), GpuArch::Gtx10, "GTX 1050 Ti");
        // Unknown pairs degrade; ids never leak across vendors.
        assert_eq!(arch_for(0x1002, 0x9999), GpuArch::Unknown);
        assert_eq!(arch_for(0xdead, 0x1002), GpuArch::Unknown);
        assert_eq!(
            arch_for(0x1002, 0x2684),
            GpuArch::Unknown,
            "an NVIDIA id is not AMD"
        );
    }

    #[test]
    fn architectures_map_to_the_compat_buckets() {
        for (arch, bucket) in [
            (GpuArch::Rdna4, Arch::Rdna4),
            (GpuArch::Rdna3, Arch::Rdna3),
            (GpuArch::Rdna2, Arch::Rdna2),
            (GpuArch::Rdna1, Arch::Rdna1),
            (GpuArch::Dg2, Arch::Arc),
            (GpuArch::Bmg, Arch::Arc),
            (GpuArch::Rtx20, Arch::Rtx),
            (GpuArch::Rtx30, Arch::Rtx),
            (GpuArch::Rtx40, Arch::Rtx),
            (GpuArch::Rtx50, Arch::Rtx),
        ] {
            assert_eq!(arch.compat_arch(), Some(bucket), "{arch:?}");
        }
        assert_eq!(GpuArch::Gtx10.compat_arch(), None);
        assert_eq!(GpuArch::Gtx16.compat_arch(), None);
        assert_eq!(GpuArch::Unknown.compat_arch(), None);
        assert_eq!(GpuArch::Rdna4.vendor(), GpuVendor::Amd);
        assert_eq!(GpuArch::Bmg.vendor(), GpuVendor::Intel);
        assert_eq!(GpuArch::Rtx50.vendor(), GpuVendor::Nvidia);
    }

    #[test]
    fn ntsync_and_hdr_become_environment_hints() {
        let temp = tempfile::tempdir().unwrap();
        write_card(temp.path(), "card0", 0x1002, 0x7550, Some(17_179_869_184));

        let bare = detect(&GpuProbe::at(temp.path()));
        assert!(!bare.ntsync);
        assert_eq!(bare.hdr, HdrSupport::Unknown);
        assert!(bare.suggested_env().is_empty());

        for desktop in ["KDE", "GNOME", "mango"] {
            let probe = GpuProbe::at(temp.path())
                .with_ntsync(true)
                .with_desktop(desktop);
            let profile = detect(&probe);
            assert!(profile.ntsync);
            assert_eq!(profile.hdr, HdrSupport::Supported, "{desktop}");
            assert_eq!(
                profile.suggested_env(),
                vec![("PROTON_USE_NTSYNC", "1"), ("DXVK_HDR", "1")]
            );
        }

        // A desktop we do not recognise is Unsupported, not Unknown.
        let hyprland = detect(&GpuProbe::at(temp.path()).with_desktop("Hyprland"));
        assert_eq!(hyprland.hdr, HdrSupport::Unsupported);
        assert!(hyprland.suggested_env().is_empty());
    }

    #[test]
    fn no_card_falls_back_to_vulkaninfo_and_warns() {
        let temp = tempfile::tempdir().unwrap();
        let probe = GpuProbe::at(temp.path()).with_vulkaninfo(VULKANINFO_RX9070);

        let profile = detect(&probe);

        assert_eq!(profile.vendor, GpuVendor::Amd, "ids come from vulkaninfo");
        assert_eq!(profile.arch, GpuArch::Rdna4);
        assert_eq!(profile.device_id, Some(0x7550));
        assert_eq!(profile.mesa, Version::parse("25.3.0"));
        assert!(
            profile
                .warnings
                .iter()
                .any(|line| line.contains("no DRM card")),
            "{:?}",
            profile.warnings
        );
    }

    #[test]
    fn an_id_outside_the_table_degrades_with_a_warning() {
        let temp = tempfile::tempdir().unwrap();
        write_card(temp.path(), "card0", 0x1002, 0x9999, None);

        let profile = detect(&GpuProbe::at(temp.path()));

        assert_eq!(profile.vendor, GpuVendor::Amd);
        assert_eq!(profile.arch, GpuArch::Unknown);
        assert_eq!(profile.device_id, Some(0x9999));
        assert!(
            profile.warnings.iter().any(|line| line.contains("0x9999")),
            "{:?}",
            profile.warnings
        );
    }

    #[test]
    fn only_card_devices_are_read_and_visible_vram_is_a_fallback() {
        let temp = tempfile::tempdir().unwrap();
        write_card(temp.path(), "card0", 0x1002, 0x731f, None);
        // A visible-VRAM file is used when the total is absent.
        fs::write(
            temp.path()
                .join("class/drm/card0/device/mem_info_vis_vram_total"),
            "8573157376\n",
        )
        .unwrap();
        // Connectors and render nodes must never be mistaken for cards.
        fs::create_dir_all(temp.path().join("class/drm/card0-DP-1")).unwrap();
        fs::create_dir_all(temp.path().join("class/drm/renderD128")).unwrap();

        let profile = detect(&GpuProbe::at(temp.path()));

        assert_eq!(profile.device_id, Some(0x731f));
        assert_eq!(profile.vram_bytes, Some(8_573_157_376));
    }

    #[test]
    fn a_profile_round_trips_through_json_for_the_cli() {
        let temp = tempfile::tempdir().unwrap();
        write_card(temp.path(), "card0", 0x1002, 0x7550, Some(17_179_869_184));
        let probe = GpuProbe::at(temp.path()).with_vulkaninfo(VULKANINFO_RX9070);
        let profile = detect(&probe);

        let json = serde_json::to_string(&profile).unwrap();
        assert!(json.contains(r#""arch":"rdna4""#), "{json}");
        assert!(json.contains(r#""vendor":"amd""#), "{json}");
        let parsed: GpuProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, profile);
    }
}
