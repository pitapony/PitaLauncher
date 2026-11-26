# Mod manifests (`mods::manifest`) — one backup format for every mod

Status: implemented and verified against `tempfile` trees on macOS and in
`archlinux:latest`. Nothing consumes it yet — the Tier B OptiScaler deployment
(M3), the HelixSR installer (M3) and the DLSSNR-AMD installer (M3.5) are the
three systems it exists for, and M4's repair re-checks the same tree hashes. No
real mod has been installed through it yet.

## Why this slice exists

Three independent mod systems drop files into a game directory over whatever was
already there:

- **Tier B OptiScaler** copies `OptiScaler.dll`/`.ini` in under a chosen DLL
  name and patches the ini in place;
- **HelixSR** runs its own `helixsr-setup.sh` installer, which writes whatever it
  wants;
- **DLSSNR-AMD** runs its own `install.sh`, which does the same.

Each one needs to answer the same two questions: *what did I create?* and *what
was here before?* If each answered them its own way, "uninstall restores
byte-for-byte" would be three mechanisms, three ledgers and three sets of bugs.
`mods::manifest` is the one format and the one restore, so the answer is provably
the same for all three.

The ticket's second half is honesty: a mod that a user or another tool has
changed since it was installed must be **reported**, not silently overwritten,
and an uninstall that cannot reproduce the original tree must say so instead of
pretending it succeeded.

## Pieces

| Item | Role |
|---|---|
| `manifest::tree_hash(root)` | BLAKE3 of a directory's files, sorted, `.gl-backup/` skipped |
| `manifest::file_hash(path)` | BLAKE3 of one file, lowercase hex |
| `manifest::ModSource` | a destination path plus the file that fills it |
| `manifest::ModEntry` | one path: the hash of what the mod created and/or backed up |
| `manifest::ModManifest` | the ledger: mod name, time, tree hashes, directories, entries |
| `manifest::ModManifest::install` | hash → back up → write → hash → save the manifest |
| `manifest::ModManifest::restore` | verify backups, delete created, restore, compare tree hash |
| `manifest::ModManifest::verify` | report every file that no longer matches, change nothing |
| `manifest::ModError` / `ModIssue` / `VerifyReport` / `RestoreReport` | the failure and diagnostic shapes |
| `pitagames-cli mods` | the diagnostic: show, `--hash`, `--verify`, `--restore` |

The crate is the twelfth in the workspace (`pitagames-mods`), framework-free like
`pitagames-platform` and `pitagames-core` (ADR-0006). It depends on `blake3`
(the per-file and tree hash), `walkdir` (the bounded, symlink-free tree walk),
`serde`/`serde_json` (the manifest file) and `time`. `sha2` is untouched — it
stays the digest of release artifacts and the Steam-config journal, which are
SHA-256 by contract.

## Where the files live

Everything is inside the game directory the mod touches:

| Layer | Path | Notes |
|---|---|---|
| originals | `<game>/.gl-backup/<relative path>` | the same layout as the game tree |
| the ledger | `<game>/.gl-backup/manifest.json` | written last, atomically |

A mod that replaces `<game>/dxgi.dll` moves it to `<game>/.gl-backup/dxgi.dll`
and writes its own `dxgi.dll`; a mod that adds a file records it with no backup.
Nothing is written outside the game directory, and `.gl-backup/` is excluded from
every tree hash (it holds the ledger and the originals, not the game).

## The tree hash

`tree_hash` is the BLAKE3 of the directory's **regular files only**: each file
contributes its forward-slash relative path, a NUL, its length and its bytes, in
sorted path order.

- **Directories are not hashed**, so the empty scaffolding an install creates (and
  a restore removes again) cannot make a round-trip disagree.
- **The backup directory is skipped**, so writing the ledger never changes the
  hash it records.
- **Symlinks contribute nothing and are never followed**, so a self-referential
  tree cannot loop and a file reached by two names is hashed once.

Two directories with the same files and the same bytes hash the same, regardless
of the order the files were written in.

## Install, verify, restore

```text
install:  tree_hash(pre) ─► for each file: back up an existing destination,
          write the new bytes, hash them ─► tree_hash(post) ─► save manifest
verify:   for each entry: created file matches? backup matches? ─► tree hash == post?
restore:  every backup matches? ─► delete created ─► move backups back ─►
          prune created directories ─► tree_hash == pre?
```

- **`install` refuses to guess.** Every destination is validated as a safe
  relative path *before* the tree is touched, so `../escape.dll`, an absolute
  path, a `..` component and anything inside `.gl-backup/` are refused with the
  tree left untouched. A second install is refused until the first is restored
  (the one injection slot, ADR-0023).
- **`restore` checks the backups first.** A missing or modified backup is an
  error that names the path and stops the restore before it changes anything — a
  restore that put back the wrong bytes would be worse than no restore. Its
  report says whether the tree hash returned to the pre-install value.
- **`verify` reports everything.** A created file that is missing or modified, a
  backup that is missing or tampered with, and a whole-tree mismatch (a file
  added or removed outside the manifest, which no per-file comparison can see) —
  each becomes a `ModIssue`. `VerifyReport::ok()` is "nothing to report".
- **A malformed or future manifest is refused by name**, not read as if it were
  this format, so a format change is loud.

## CLI diagnostic

```bash
pitagames-cli mods <dir>              # the manifest a mod left in <dir>
pitagames-cli mods <dir> --hash       # the directory's tree hash
pitagames-cli mods <dir> --verify     # check every file against the manifest
pitagames-cli mods <dir> --restore    # uninstall, restoring .gl-backup/
pitagames-cli mods <dir> --json       # any of the above as JSON
```

```
Mod manifest in /games/epic/Gizmo
  mod          optiscaler
  installed    2026-11-25T09:14:07.912Z
  tree hash    6c1f… 
  pre-install  2a90…
  files        2
    OptiScaler.ini    created 7e4d8b…
    dxgi.dll          created 3f2a91…  backup 9c1b77…
```

`--verify` and `--restore` exit non-zero when something does not match (printing
the report first), so a script can gate on them. `mods <dir>` on a directory with
no manifest prints that there is none and exits `0`. The command never installs
anything; installation runs the upstream installer and belongs to the GUI.

## Verification

Pure crate (macOS host and CI):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-mods -p pitagames-cli
cargo clippy -p pitagames-mods -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`pitagames-mods` ships 16 tests over `tempfile` trees:

- a tree hash tracks contents and not walk order, ignores empty directories, and
  skips `.gl-backup/`;
- `file_hash` matches the published BLAKE3 digest of the empty input;
- install records a replaced file as created **and** backed up, a new file as
  created only, and leaves everything else untouched;
- a second install is refused until the first is restored;
- an unsafe destination is refused and leaves nothing behind;
- **install → restore returns the original tree hash** (the headline criterion),
  deletes the created file, restores the original, and clears the ledger;
- a restore prunes the directories the install created;
- `restore` with no manifest is an error;
- `verify` accepts a clean install, and reports a modified created file, a
  missing created file, a tampered backup, a missing backup, and an unexpected
  file as a tree-hash mismatch;
- `restore` refuses a tampered backup without changing the tree;
- a malformed or future manifest is reported, not ignored.

`pitagames-cli` adds two: the `mods` argument parser (one directory, one action,
conflicting actions refused) and an end-to-end show/hash/verify/restore run over
a `tempfile` tree, including a tampered backup failing both `--verify` and
`--restore`.

## Known limits

- **No consumer yet.** The three mod systems still install by hand (Tier B
  OptiScaler), or not at all (HelixSR, DLSSNR); nothing calls `install`/`restore`
  outside the tests. The first real caller is the M3 Tier B deployment.
- **File-based installs only.** `install` copies files it is given. A mod whose
  own installer writes the tree (HelixSR, DLSSNR) will need the record-after
  shape: snapshot the pre-install hash, run the installer, then hash what it
  created. That constructor is not in this slice.
- **Nothing verifies a mod's signature or origin.** The manifest proves *what*
  changed and that it can be undone; the SHA-256 release pin and "never
  redistribute" rules live in `mods::release` (ADR-0024), which is not here.
- **No symlinks, no hardlinks, no special files** in the tree hash. A mod that
  installs one is not restored byte-for-byte.
- **Not run against a real mod.** No Tier B, HelixSR or DLSSNR install has been
  driven through it yet; the first is the M3 hardware spike.

## Deliberately not in this slice

- `mods::release` (fetch/verify/unpack), `mods::ini` (the line-preserving
  patcher), `mods::capture` (printed `%command%` → env), `mods::conflict`,
  `mods::verify` (log markers) and `mods::bundle`.
- The GUI's mod install/uninstall surfaces and the generalized terminal.
- Any execution of an upstream installer (ADR-0024).
