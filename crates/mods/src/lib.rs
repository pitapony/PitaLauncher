//! Mod primitives for PitaGames.
//!
//! Framework-free by design, like `pitagames-platform`: the GUI and the CLI
//! call in and render what they report, and every rule is testable against a
//! `tempfile` tree, a fixture archive or a transcript with no game, no GPU, no
//! webview and no network.
//!
//! M3, M3.5 and M4 deploy three different mod systems — Tier B OptiScaler,
//! HelixSR and DLSSNR-AMD — and each one is a set of files dropped into a game
//! directory over whatever was already there, fetched from a release host and
//! configured through an ini. Four modules make that one mechanism instead of
//! three:
//!
//! - [`manifest`] — the one ledger: every file a mod created, every original it
//!   moved to `.gl-backup/`, a BLAKE3 hash per file, and the verified restore
//!   that proves the tree came back. "Uninstall restores byte-for-byte" is
//!   provably the same for all three.
//! - [`release`] — resolve a pinned release on GitLab or GitHub, download the
//!   named asset, verify its SHA-256 against a pin *we* ship (never the API's
//!   digest, ADR-0024), and unpack a zip / zstd-tar / tar. The [`release::Fetcher`]
//!   seam keeps the tests offline.
//! - [`ini`] — a line-preserving patcher driven by a per-mod data surface: only
//!   the allowed keys change, comments and ordering survive, and an unknown
//!   section, key or value is refused by name.
//! - [`capture`] — read the `%command%` launch options an installer prints into
//!   an env map, keeping only the fixed allowlist and naming every reject.
//! - [`tierb`] — deploy managed OptiScaler: resolve the exe dir, choose a free
//!   DLL name from the ordered candidate list, fetch/verify the pinned release,
//!   patch the ini, write the manifest, and return the `WINEDLLOVERRIDES` env as
//!   a vector. Uninstall restores the tree from the manifest.
//! - [`conflict`] — the pre-flight scan for the single injection slot: it names
//!   each known injection file in a game directory and its manifest owner, so a
//!   slot switch can offer to uninstall the previous one before it clobbers it.

pub mod capture;
pub mod conflict;
pub mod ini;
pub mod manifest;
pub mod release;
pub mod tierb;

pub use conflict::{ConflictFile, ConflictScan, KnownFile, Mechanism, Owner, scan, scan_dir};

pub use capture::{ALLOWED_KEYS, ALLOWED_PREFIXES, CapturedEnv, RejectReason, RejectedAssignment};
pub use ini::{
    AllowedValues, IniError, IniSetting, IniSurface, IniValue, SurfaceKey, SurfaceSection,
};
pub use manifest::{
    BACKUP_DIR, MANIFEST_NAME, MANIFEST_VERSION, ModEntry, ModError, ModIssue, ModManifest,
    ModSource, RestoreReport, VerifyReport, file_hash, tree_hash,
};
pub use release::{
    ArchiveKind, FetchError, Fetched, FetchedRelease, Fetcher, LinkPolicy, MapFetcher,
    PinnedRelease, Release, ReleaseAsset, ReleaseError, ReleaseSource, Unpacked, sha256_hex,
    unpack_bytes, unpack_bytes_with,
};
pub use tierb::{
    Deployment, DllName, TierBError, choose_dll, deploy, override_env, resolve_exe_dir, uninstall,
};
