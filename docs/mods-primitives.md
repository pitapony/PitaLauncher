# Mod primitives (`mods::release`, `mods::ini`, `mods::capture`) — fetch, patch, capture

Status: implemented and verified against fixture archives, `tempfile` trees and a
fake installer on macOS. Nothing consumes them yet — the Tier B OptiScaler
deployment (M3), the HelixSR installer (M3) and the DLSSNR-AMD installer (M3.5)
are the three systems they exist for. No real release has been fetched and no
real installer transcript parsed yet.

## Why this slice exists

`mods::manifest` (ticket #11) is the one ledger every mod writes. The three
systems that write it also share three *other* jobs, and each one getting its own
would mean three ways to fetch a release, three ways to edit an ini and three
ways to read a launch-option line — the drift `AGENTS.md`'s "shared mod
primitives" principle exists to prevent (Foundation, spec #8):

- **fetch a pinned release**: the mods live on release hosts (GitLab for the
  DLSSNR upstream, GitHub for the RDNA 3 fork and HelixSR), each download must be
  SHA-256-verified against a pin *we* ship (ADR-0024), and each archive arrives
  as a zip or a (zstd) tar;
- **edit an ini**: each ships an `OptiScaler.ini` / `dlssnr-amd.ini` whose
  comments, ordering and formatting must survive, and only a handful of keys are
  ours to set;
- **capture launch options**: each installer prints a `%command%` line, and only a
  fixed allowlist of environment may come from it (ADR-0023).

All three are framework-free and pure at their seam, so their tests need no
network, no game and no webview.

## `mods::release` — resolve, verify, unpack

| Item | Role |
|---|---|
| `release::Fetcher` | the network seam: `get(url) -> Result<Vec<u8>, FetchError>` |
| `release::MapFetcher` | the fixture fetcher: an in-memory URL → bytes map (tests, CLI) |
| `release::ReleaseSource` | `GitLab` / `GitHub`: the release API URL and its JSON shape |
| `release::Release` / `ReleaseAsset` | a tag and its downloadable assets, host-agnostic |
| `release::PinnedRelease` | `{ source, project, tag, asset, sha256 }` — the pin |
| `release::Fetched` | the bytes and the digest they actually had |
| `release::ArchiveKind` | `Zip` / `TarZstd` / `Tar`, detected by magic bytes |
| `release::sha256_hex` / `unpack_bytes` | the digest, and the safe unpack |
| `release::FetchedRelease` | asset, verified SHA-256, kind, extracted file list, dest |
| `release::ReleaseError` | every failure, named |

**The pin is authoritative.** `PinnedRelease::fetch` resolves the release, finds
the named asset, downloads it and hashes it; a byte stream whose SHA-256 is not
the shipped pin is refused. When the host *also* published a digest and it
disagrees with the bytes, that is refused too, because a host that contradicts
its own published digest is not something to install from. The two hosts differ
only in their JSON — GitHub's `browser_download_url` + `digest`, GitLab's
`assets.links[].direct_asset_url` (a link with no URL is dropped) — and GitLab's
project path is percent-encoded into one API segment.

**Unpack chooses by magic, not by name**, then refuses to escape its destination:
a zip entry whose `enclosed_name()` is `None`, a tar entry `unpack_in` declines,
or a symlink/hardlink/special entry is a named error, so a hostile archive cannot
write outside the game directory. Only regular files are recorded.

## `mods::ini` — a line-preserving patch on a data surface

| Item | Role |
|---|---|
| `ini::IniSurface` | a mod's allowed sections and keys (`optiscaler`/`helixsr`/`dlssnr`) |
| `ini::SurfaceSection` / `SurfaceKey` | one section, one key |
| `ini::AllowedValues` | `Any`, `OneOf([…])` (case-insensitive), or `Range { min, max }` |
| `ini::IniSetting` | one `{ section, key, value }`, parseable from `Section.Key=Value` |
| `ini::IniValue` | one allowed key the file currently sets |
| `ini::IniError` | `UnknownSection` / `UnknownKey` / `InvalidValue` / `MalformedSetting` |

The rules are data: `IniSurface::optiscaler()` exposes only `[Upscalers]
Dx12Upscaler` and `[FrameGen] Enabled`/`FGType`; `helixsr()` exposes `[HelixSR]
Int8` (`auto`/`on`/`off`) and `WaveSize` (`auto`/`64`/`32`); `dlssnr()` exposes
`[Network] Model`/`model_scale` (25..=100), `[Int4Mixed] Enabled`, `[Preprocess]
Enabled`/`Exposure`/`Curve` and `[NRControl] MenuKey`. Everything else stays the
in-game menu's.

`patch` validates every setting against the surface *before* applying any, so a
refused patch cannot half-modify a file, and then rewrites only the value token of
the matching line: indentation, whole-line and inline comments, blank lines, CRLF
endings and ordering all survive. A missing key is added to the end of its
section; a missing section is appended with a blank separator. `read` returns an
allowed key's current value (`None` if unset) and refuses an unknown one, and
`current` lists every allowed key the file sets.

## `mods::capture` — printed `%command%` → allowlisted env

| Item | Role |
|---|---|
| `capture::capture(text)` | every `%command%` line → a `CapturedEnv` |
| `capture::CapturedEnv` | the allowlisted `{ KEY: value }` map and the rejects |
| `capture::RejectedAssignment` / `RejectReason` | a dropped key and its named reason |
| `capture::ALLOWED_KEYS` / `ALLOWED_PREFIXES` | the fixed allowlist |

Only what appears **before** `%command%` is read (that is where env belongs;
anything after is the game's own arguments). A value may be bare or double-quoted
with a backslash escape. A `KEY=value` whose key is on the allowlist —
`WINEDLLOVERRIDES`, `VK_ADD_LAYER_PATH`, `VK_INSTANCE_LAYERS`, or any `DXVK_*` /
`VKD3D_*` key — is captured; any other `KEY=value` is dropped and reported with
the reason "not on the captured-env allowlist", so a mod's printed line can never
smuggle arbitrary environment into a launch. A later line for the same key wins.

## CLI diagnostic

```bash
pitagames-cli mods release --fixture <dir> [--dest <dir>] [--json]
pitagames-cli mods ini <file> --surface <name> [--set Section.Key=Value]... [--write] [--json]
pitagames-cli mods capture <file|-> [--json]
pitagames-cli mods capture --run <program> [args...] [--json]
```

- **`release`** runs the whole fetch → verify → unpack pipeline with no network. A
  fixture directory holds `fixture.json` (the pin), `release.json` (the API body
  the source parses) and `<asset>` (the archive bytes); a wrong pin is refused.
- **`ini`** with no `--set` prints the allowed keys the file sets; with `--set` it
  patches (to stdout, or in place with `--write`) and refuses an unknown
  section/key/value.
- **`capture`** parses a transcript file (or `-` for stdin); `--run` drives a
  program through the shared `stores::cli` PTY driver and captures its output
  live, so the installer-install is the same code path as the store backends.

`mods <dir>` (the manifest diagnostic) is unchanged; the three subcommands are
dispatched by the first argument.

## Dependencies

Added to `crates/mods`: `sha2` (the release digest; `blake3` stays the tree hash),
`tar` 0.4, `zstd` 0.14, and `zip` 9 trimmed to its deflate reader with flate2's
pure-Rust `zlib-rs` backend (`default-features = false,
features = ["deflate-flate2-zlib-rs"]` — the mod packages are read-only here).
`crates/cli` gains `sha2`/`tar`/`zip` as dev-dependencies to build fixture
archives in its tests.

## Verification

Pure crates (macOS host and CI):

```bash
cargo test -p pitagames-mods -p pitagames-cli
cargo clippy -p pitagames-mods -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`pitagames-mods` ships 52 tests (16 manifest + 36 new):

- **release (13)** — the SHA-256 of `""` and `"abc"` against their published
  vectors; both hosts' release URLs and JSON (including a GitLab link without a
  URL); a fixture zip round-trips through fetch → verify; a pin mismatch is
  refused; an API digest that disagrees with the bytes is refused; a missing asset
  is named; zip / zstd-tar / plain-tar all unpack; a full fixture round-trip; an
  unknown archive is refused; a zip-slip entry and a tar symlink are refused.
- **ini (14)** — a patch preserves a comment/ordering fixture byte-for-byte; an
  inline comment survives the replaced line; CRLF survives; a missing key joins
  its section; a missing section is appended; an unknown section, unknown key, bad
  enum and out-of-range number are refused; a refused patch leaves the input
  untouched; `read`/`current`; `Section.Key=Value` parsing; the empty file.
- **capture (9)** — one allowlisted assignment; several including both prefixes;
  an off-list key dropped with a named reason; a line without `%command%` ignored;
  only the prefix before `%command%` read; surrounding prose harmless; last
  assignment wins; an escaped quote kept; the allowlist's exact/prefix edges.

`pitagames-cli` adds 4: the three argument parsers and the dispatcher; a fixture
fetch/verify/unpack (and a wrong pin refused); a read → patch → refuse-unknown run
over a `tempfile` ini; and a transcript parse plus one `--run` through the PTY
driver with a `#!/bin/sh` fake installer.

## Known limits

- **Not driven against a real release host or installer.** The parsers cover the
  shapes the two hosts publish and the fixtures are hand-built; the first real
  fetch is the M3 Tier B deployment.
- **`unpack` handles zip, tar.zst and tar only.** A gzip-compressed tar is not one
  of the three the ticket ships; it is a named `UnknownArchive`, not a silent
  failure. Symlinks and hardlinks are refused rather than created.
- **`zip` is read-only.** The crate is built with the deflate reader only; there
  is no writer (fixtures are written by the CLI's dev-dependency copy).
- **The ini parser is deliberately small.** It understands `[section]` headers,
  `key=value`, `;`/`#` comments and CRLF; an inline comment is a `;`/`#` preceded
  by whitespace. It is a patcher, not a general ini library.
- **`capture` reads the env prefix only.** A `%command%` line whose environment is
  written *after* the token (not the Steam convention) is not read.
- **`mods capture --run` needs at least one program argument**, because the PTY
  driver runs a program through an argv template.

## Deliberately not in this slice

- Any caller. Tier B deployment, the HelixSR and DLSSNR installers, the graphics
  screen and the generalized terminal (the shared `app::terminal`) are later
  tickets; the tool pins registry and per-game `compat/<game>.toml` overlays are
  M3/M3.5.
- A live HTTP `Fetcher`. Production plugs one in; this slice ships the trait, the
  fixture fetcher and the CLI's fixture diagnostic.
- `mods::conflict`, `mods::verify` (log markers) and `mods::bundle`; the model
  sourcing and the per-arch DLSSNR backend registry (M3.5).
