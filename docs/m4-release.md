# M4 — Release packaging: the tagged package, signed tags, reproducible builds

Status: implemented as packaging + process; **not yet exercised against a real
tag**. `packaging/PKGBUILD.release` (`pitagames`) is checked in beside the
development `packaging/PKGBUILD` (`pitagames-git`), `packaging/release.sh` cuts
the signed tag and pins the digest, and this file is the checklist. The release
package builds only from a pushed tag with a filled-in `sha256sums`, so the
"builds and installs" half of the acceptance criteria waits for the first tag
(see *Not run yet*). Submitting to the AUR and the CachyOS-repository proposal
is out of scope here.

## Why this exists

`docs/m1-packaging.md` ships the `-git` package so an M1 user can install the
launcher; it clones `main` and cannot name a version. A v1 release needs an
artifact that is pinned to one commit, verifiable from the tarball alone, and
reproducible — `--locked`, a pinned toolchain and a pinned source digest. This
is that package and the process that produces it.

## The two packages

| File | `pkgname` | Source | When |
|---|---|---|---|
| `packaging/PKGBUILD` | `pitagames-git` | `git+$url.git`, `pkgver()` per commit | always; the development package |
| `packaging/PKGBUILD.release` | `pitagames` | the signed tag's `archive/refs/tags/v$pkgver.tar.gz` | from the first release tag |

Both install the same layout — the same binaries, the shim split, the desktop
entry, the theme icon, the AppStream metainfo and the compat database — and
declare the same `depends`/`makedepends`/`optdepends`. They are only allowed to
differ in *where the source comes from* and *how the version is named*;
`crates/core/src/release.rs` asserts those arrays, the `arch`/`url`/`license`
values and every install path are identical in the two files, so the release
cannot quietly ship a different dependency set from the one that was tested.

The dev package `provides=('pitagames')` and `conflicts=('pitagames')`; the
release package is `pitagames` and `conflicts=('pitagames-git')`. Installing
the release replaces the dev build instead of leaving two launchers on disk.

### The release PKGBUILD is a template until the tag

`PKGBUILD.release` carries `sha256sums=('SHA256_PLACEHOLDER')`. That placeholder
is deliberate: the digest of a release tarball does not exist until the tag is
pushed, so a checked-in real digest could only be for a commit that is not yet a
release. `packaging/release.sh srcinfo <version>` downloads the tag archive,
computes its `sha256sum`, substitutes it for the placeholder and writes the
generated `.SRCINFO`. Never ship `SKIP` or the placeholder: the pinned digest is
what makes the build reproducible.

## The release checklist

Run on Arch/CachyOS (or in an `archlinux:latest` container with `base-devel`)
from the repository root.

1. **Bump the version.** Set `version = "<version>"` in `Cargo.toml`
   (`[workspace.package]`) and `pkgver=<version>` in `packaging/PKGBUILD.release`
   (and the metainfo's `<release>`, a data update). `packaging/release.sh`
   refuses to tag when the two disagree.

2. **Green the tree.** The pure crates:

   ```sh
   cargo test -p pitagames-core -p pitagames-db -p pitagames-art \
     -p pitagames-metadata -p pitagames-platform -p pitagames-mods \
     -p pitagames-stores -p pitagames-secrets -p pitagames-input \
     -p pitagames-shim -p pitagames-cli
   cargo clippy --all-targets -- -D warnings
   cargo fmt --all --check
   ```

   and the GUI in `archlinux:latest` (`cargo build -p pitagames`, per
   `AGENTS.md`).

3. **Create the signed tag.** A release tag is annotated and signed:

   ```sh
   packaging/release.sh tag <version>     # git tag -s v<version> -m "PitaGames <version>"
   git tag -v v<version>                  # verify the signature before pushing
   git push origin v<version>
   ```

   GnuPG must have a signing key configured (`git config user.signingkey`).
   `git tag -v` fails the release if the signature does not verify; that is the
   point of signing.

4. **Pin the digest and generate `.SRCINFO`.** Once the tag is pushed:

   ```sh
   packaging/release.sh srcinfo <version>
   ```

   This downloads `.../archive/refs/tags/v<version>.tar.gz`, finds its
   `sha256sum`, replaces `SHA256_PLACEHOLDER` in `sha256sums`, and runs
   `makepkg -p PKGBUILD.release --printsrcinfo > .SRCINFO` in `packaging/`.
   Commit the pinned `PKGBUILD.release` and the generated `packaging/.SRCINFO`
   (the AUR is served from these two files).

5. **Build and install the release package.**

   ```sh
   cd packaging
   makepkg --noconfirm                       # builds pitagames-<version>-1-x86_64.pkg.tar.zst
   pacman -U --noconfirm pitagames-<version>-1-x86_64.pkg.tar.zst
   pitagames --version                        # prints <version>
   ```

   `makepkg` downloads the tag tarball and verifies it against the digest in
   `sha256sums`; a mismatch stops the build. Confirm the shim symlink, the
   compat database and the desktop entry behave as in
   `docs/m1-packaging.md`, then re-run `appstreamcli validate --no-net`,
   `desktop-file-validate` and `namcap` on the release package.

6. **Only then publish.** The AUR submission and the CachyOS-repo proposal are
   the publish step and are out of scope for this ticket; they consume steps
   4–5's outputs unchanged.

## Reproducible builds

The release is pinned on four axes; a rebuild on the same axis set must produce
the same package contents.

- **The source is immutable.** `source` names `archive/refs/tags/v$pkgver`, not
  a branch, and `sha256sums` carries the released tarball's digest. `makepkg`
  refuses a tarball whose digest has changed, which is what detects a moving or
  tampered archive.
- **Dependencies are locked.** `build()` runs `cargo build --release --locked`,
  so `Cargo.lock` — checked in — pins every crate. Updating a dependency is a
  commit, never a silent build-time drift.
- **The toolchain is pinned.** The workspace's `rust-version` is the floor
  (ADR-0005, currently 1.89) and the reproducible reference build uses one
  fixed `rustup` toolchain in a fixed `archlinux:latest` digest for both
  `makepkg` and the CI `gui` job. Arch's rolling `rust` package tracks the
  latest release, so a byte-reproducible local build must select the same
  toolchain as the reference build; a different `rustc` is the one input a user
  can change and still get a working package.
- **The build stamps are fixed.** `makepkg` exports `SOURCE_DATE_EPOCH` (from
  the source commit) and strips build paths, and `options=('!lto')` keeps link
  order stable; together they remove the last sources of nondeterminism.

To compare two builds, build twice (a second `makepkg` on the same tarball and
toolchain) and compare the resulting `.pkg.tar.zst` with `sha256sum`. The debug
package's `.build-id` differs between toolchains; compare the install file lists
and the release package's payload first.

## `.SRCINFO`

The AUR reads `.SRCINFO`, not the PKGBUILD, and it is always *generated*, never
hand-edited:

```sh
cd packaging
makepkg -p PKGBUILD.release --printsrcinfo > .SRCINFO
```

A `-git` package's `.SRCINFO` changes every commit, which is why the dev package
ships none (`docs/m1-packaging.md`); the release package's `.SRCINFO` changes
only when `PKGBUILD.release` changes, so it is committed at tag time and treated
as a build artifact of that file.

## Tests

`crates/core/src/release.rs` holds the contract and its tests. They assert that
the dev and release PKGBUILDs declare identical `depends`/`makedepends`/
`optdepends`, `arch`, `url`, `license` and `pkgdesc` and install the same paths;
that the release package is `pitagames`, versions with Cargo, takes its source
from `archive/refs/tags/`, and pins a real digest with `--locked`; that the dev
package keeps tracking `main`; that `packaging/release.sh` knows the placeholder
and runs `makepkg --printsrcinfo`; and that this checklist still names every
step (`git tag -s v`, `git tag -v`, `sha256sums`, `--locked`,
`SOURCE_DATE_EPOCH`, `makepkg`, `--printsrcinfo`, `.SRCINFO`,
`packaging/release.sh`). A rename or a dropped step fails a test instead of
shipping silently.

## Not run yet

- **No tag exists**, so `PKGBUILD.release` still carries the placeholder and the
  package has not been built, installed, `appstreamcli`-validated or
  `namcap`-checked from a real release tarball. The container check so far is
  `makepkg -p PKGBUILD.release --printsrcinfo` parsing cleanly and `sh -n
  packaging/release.sh`.
- **No signature has been produced** by this repo; the signing key is the
  maintainer's.
- **The reproducible comparison has not been run** on two real builds (it needs
  the tag).
- **The CachyOS-repository proposal is out of scope** — it is the only publish
  step and follows the AUR files this process generates.
