# M1 — Packaging: PKGBUILD, AppStream metadata, theme icon

Status: implemented and verified in `archlinux:latest` — `makepkg` builds
`pitagames-git`, `pacman -U` installs it, `appstreamcli validate` and
`desktop-file-validate` pass, `namcap` reports no missing dependency, and the
installed GUI runs under Xvfb and leaves the package's desktop entry alone. Not
submitted to the AUR or the CachyOS repository yet, and not installed on the RX
9070 / RX 5700 XT machines.

## Why this exists

M1 closes with an artifact a user can install, not a checkout they build by
hand. The package is also where three loose ends meet:

1. **`Icon=pitagames` is a theme name.** The entry could only point at a
   placeholder until an icons directory existed. The package installs
   `packaging/pitagames.svg` into `hicolor/scalable/apps`.
2. **The compat DB has an installed location.** `platform::compat` reads
   `/usr/share/pitagames/compat` (or `PITAGAMES_COMPAT_DIR` for a checkout), so
   the curated `compat/*.toml` files must be installed there.
3. **The package owns the menu entry.** `app::desktop_entry::ensure` skips
   writing a user-level entry when one exists under `XDG_DATA_DIRS`, so the
   packaged entry is never shadowed by a stale checkout path.

## The package (`packaging/PKGBUILD`)

`pitagames-git` while there is no release tag; the plan's release package keeps
the name `pitagames` and `provides=('pitagames')` / `conflicts=('pitagames')`
already match it, so switching is a rename, not a layout change.

```sh
cd packaging
makepkg -si        # build, install dependencies, install
makepkg -e         # rebuild without re-cloning the source (iterating on package())
```

`pkgver()` is `r<commit-count>.<7-char sha>`, so the installed version names the
exact commit. The binary itself reports Cargo's `0.1.0` (`pitagames --version`)
until a release tag sets both to the same number.

### Install layout

| Path | What |
|---|---|
| `/usr/bin/pitagames` | the GUI |
| `/usr/bin/pitagames-cli` | the headless diagnostic CLI |
| `/usr/bin/pitagames-shim` | symlink — Steam launch options contain this name |
| `/usr/lib/pitagames/pitagames-shim` | the real shim binary |
| `/usr/share/applications/pitagames.desktop` | `Exec=pitagames`, `Icon=pitagames` |
| `/usr/share/icons/hicolor/scalable/apps/pitagames.svg` | the theme icon |
| `/usr/share/metainfo/io.github.pitapony.PitaGames.metainfo.xml` | AppStream |
| `/usr/share/pitagames/compat/*.toml` | the shipped compat database |
| `/usr/share/licenses/pitagames-git/LICENSE` | MIT |

The shim is the one binary in `/usr/lib` with a symlink in `/usr/bin`: a Steam
launch option says `pitagames-shim <appid> %command%`, so it must be on `PATH`,
and keeping the payload in `/usr/lib` means a partial upgrade cannot leave the
GUI and the shim from different builds.

### Dependencies

`depends` is what the binary actually needs, checked with `readelf -d` and
`ldd` on the built GUI:

- `webkit2gtk-4.1`, `gtk3` — the renderer and widget toolkit (they pull gdk,
  glib, gdk-pixbuf, cairo, soup, javascriptcore).
- `openssl` — `libssl`/`libcrypto` are direct `DT_NEEDED` entries.
- `xdotool` — supplies `libxdo.so.4`, which `dioxus-desktop` links directly.
- `libayatana-appindicator` — **dlopen'd**, not linked: the tray library is
  loaded at runtime, so `ldd` and `namcap` cannot see it. The launcher still
  works without it (the tray is optional); it is a dependency so the tray
  works out of the box.
- `xdg-utils` — `xdg-open` is executed for log files.
- `hicolor-icon-theme` — so `Icon=pitagames` resolves.

`optdepends` is the plan's list, split so each entry names one package:
`proton-cachyos`, `gamescope`, `mangohud`/`lib32-mangohud`,
`gamemode`/`lib32-gamemode`, `umu-launcher`, the store CLIs (`legendary`,
`nile`, `lgogdownloader`, `nodejs`) and `bash`/`python` for the later
DLSSNR/HelixSR installers. `umu-launcher` stays optional: M1 launches Steam
games through Steam and never invokes it.

`makedepends` is `git`, `rust` and `pkgconf`; every native library the build
probes (gtk, webkit, openssl) is already in `depends`. Nothing here links
`libsecret` or SQLite: the keyring crate uses the D-Bus Secret Service API and
SQLite is vendored, which is why `libsecret`/`sqlite` from the plan's original
list are absent — adding them would install libraries the binary never loads.

## AppStream (`packaging/io.github.pitapony.PitaGames.metainfo.xml`)

- **Component ID:** `io.github.pitapony.PitaGames` — reverse-DNS because
  AppStream requires it, and stable once the project is hosted. The *desktop
  file* deliberately stays `pitagames.desktop`: `core::desktop` renders that
  name, every user-level entry already written points there, and renaming it
  would invalidate them. `<launchable type="desktop-id">pitagames.desktop</launchable>`
  is what pairs the two.
- **No screenshots.** AppStream wants real screenshots over HTTPS; the project
  has no hosting for them yet and shipping mocked images would be dishonest.
  They land with the release hosting.
- **`metadata_license` MIT** (the project's own license) and
  **`project_license` MIT**.
- `<releases>` names `0.1.0` (Cargo's version) rather than the `-git` package
  version, which changes with every commit.
- `<provides>` lists all three binaries.

`appstreamcli validate --no-net` passes with **one pedantic note**
(`cid-contains-uppercase-letter`). Uppercase in the last component is what
GNOME-style IDs use; the note is informational and does not fail validation.
Changing the ID later is possible but would also change the metainfo file name,
so it is left alone.

## The theme icon

`packaging/pitagames.svg` is the 256 px twin of the icon painted at runtime
(`crates/app/src/icon.rs`): the same rounded tile, gradient and play triangle,
with the geometry in the SVG's comment. The desktop entry's `Icon=pitagames`
finds it under `hicolor/scalable/apps`, the window/tray keep using the painted
RGBA version (no SVG rasteriser in the dependency set), and the two are kept in
step by hand — the icon module's tests pin the painter's shape and colours.

## Behaviour once installed

- **No user-level entry from the GUI.** With the package installed,
  `XDG_DATA_DIRS` contains `/usr/share`, so `desktop_entry::ensure` finds the
  packaged file and writes nothing (`pitagames-cli desktop-entry --json` reports
  `"packaged": true`, `"present": false`). `PITAGAMES_NO_DESKTOP_ENTRY` remains
  the packager's explicit off switch.
- **The compat DB loads from `/usr/share`.** `pitagames-cli compat` lists all
  30 entries with no `PITAGAMES_COMPAT_DIR` and no checkout present.
- **The tray gets its library.** With `libayatana-appindicator` installed the
  tray object builds (the container only logs
  `libayatana-appindicator is deprecated` — upstream's own warning). Without a
  StatusNotifier host nothing is displayed, so visual tray behaviour is still
  container-unverified.

## Verification

Run in `archlinux:latest` (container starts without `base-devel`, so
`fakeroot`, `debugedit` and `git` were installed explicitly; a normal
Arch/CachyOS install gets them from `base-devel`):

```sh
# The committed PKGBUILD tracks main; for the local run the source line was
# pointed at the checkout (file:///work) because HEAD was not pushed yet.
su builder -c 'cd /tmp/pkg && makepkg --noconfirm'
pacman -U --noconfirm /tmp/pkg/out/pitagames-git-*.pkg.tar.zst
appstreamcli validate --no-net /usr/share/metainfo/io.github.pitapony.PitaGames.metainfo.xml
desktop-file-validate /usr/share/applications/pitagames.desktop
namcap /tmp/pkg/out/pitagames-git-*.pkg.tar.zst
```

Results:

- `makepkg` produced `pitagames-git-r15.89268e1-1` (11 MiB package, 27.1 MiB
  installed) plus the debug package; `pacman -U` installed it cleanly and ran
  the icon-cache and desktop-database hooks.
- `pitagames --version` / `pitagames-cli --version` / `pitagames-shim --version`
  print `0.1.0`; `ldd /usr/bin/pitagames` reports no missing library.
- `appstreamcli validate --no-net`: success, 1 pedantic note (the uppercase ID).
  `desktop-file-validate`: clean. `namcap`: no missing dependency; it flags
  `libayatana-appindicator` and `xdg-utils` as "may not be needed" because both
  are used at runtime (dlopen, `Command`), not linked.
- `pitagames-cli compat` reads `/usr/share/pitagames/compat` and lists 30
  entries; `desktop-entry --json` reports `"packaged": true`.
- Xvfb run of the installed `/usr/bin/pitagames`: the window opened, no
  user-level entry was written, the tray was built (only the deprecation
  warning), a second launch asked the window to come to the front and exited
  `0`, and `pitagames-cli activate` returned `{"activated": true, …}`
  (`target/smoke/shot-pkg.png`).
- Regression side: `crates/app` is clippy-clean in the release profile
  (`cargo clippy --release --all-targets -p pitagames -- -D warnings`) and its
  **73 tests** pass; the pure crates stay clean (371 tests, fmt and
  `clippy -D warnings` over all nine).

## Known limits

- **Not on the AUR or the CachyOS repo.** No `.SRCINFO` is checked in for the
  `-git` package: it changes with every commit, so it is generated at submission
  time with `makepkg --printsrcinfo > .SRCINFO`. The release package
  (`packaging/PKGBUILD.release`) checks in the `.SRCINFO` its tag generates —
  see `docs/m4-release.md`.
- **The dev package is unsigned and untagged.** `pitagames-git` clones `main`;
  it has no release tag, no signature and no reproducible-build note. The
  tag-driven `pitagames` release package, the signed-tag process and the
  reproducible-build notes are `docs/m4-release.md`.
- **No gamescope session entry.** `/usr/share/wayland-sessions/pitagames.desktop`
  belongs to v1.1, never as the default session.
- **AppStream has no screenshots** and the reverse-DNS ID is fixed before
  hosting is chosen (open question 2 in the plan); both are data updates, not
  code.
- **Tray success is still unverified visually** — the container has no
  StatusNotifier host — and none of this has run on the RX 9070 / RX 5700 XT or
  a TV-sized display.
