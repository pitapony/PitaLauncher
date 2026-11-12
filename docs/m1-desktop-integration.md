# M1 — Desktop integration: `.desktop` entry, tray icon, single-instance focus

Status: implemented and verified in `archlinux:latest` (build, clippy, tests,
Xvfb smoke run). The tray icon needs a StatusNotifierItem host, so it was
exercised only through its failure path in the container. Not yet run on the RX
9070 / RX 5700 XT machines, and not tested on a TV.

## Why this exists

Three separate holes, all of them "the launcher exists but the desktop does not
know it":

1. **No menu entry off-package.** A checkout or a hand-copied binary has no
   `/usr/share/applications/pitagames.desktop`, so it cannot be started from a
   menu or pinned to a dock.
2. **A second launch looks broken.** The single-instance lock
   (`docs/m1-single-instance.md`) correctly refuses to start a second copy, but
   until now it printed a line and exited — nothing the user clicked on.
3. **No way back from a game.** The launcher is usually behind Steam or a game;
   a tray icon with "Show PitaGames" / "Quit PitaGames" is the shortest path
   back without a taskbar hunt.

## The `.desktop` entry (`core::desktop`)

`core::desktop::DesktopEntry` renders the launcher's entry from typed fields:

```text
[Desktop Entry]
Type=Application
Version=1.5
Name=PitaGames
GenericName=Game launcher
Comment=Launch and configure your Steam library
Exec=pitagames
Icon=pitagames
Terminal=false
Categories=Game;
Keywords=game;launcher;steam;proton;
StartupWMClass=pitagames
```

**The rendered text is a contract.** `packaging/pitagames.desktop` is checked in
and a `pitagames-core` test asserts it is byte-identical to
`DesktopEntry::default_for(vec!["pitagames"]).render()`. Regenerating it is:

```sh
pitagames-cli desktop-entry --print > packaging/pitagames.desktop
```

(the packaged `Exec=pitagames` is the installed path; a user-level entry written
by the GUI carries the absolute path of the running binary instead).

### Quoting

Two layers, exactly as the Desktop Entry specification describes them:

- every value is first escaped as a string (`\\`, `\n`, `\t`, `\r`),
- `Exec` is then whitespace-split into arguments in which reserved characters
  (space, quotes, `$`, `&`, `;`, …) force double quotes, with `\`, `"`, `` ` ``
  and `$` escaped inside them, and `%` doubled because the spec reserves it for
  field codes.

An argument with no reserved characters stays bare, so `Exec=pitagames` and
`Exec=/home/tester/code/pitagames/target/debug/pitagames` need no quotes while
`/home/two words/pitagames` does. Nothing here builds a shell command; it is the
desktop file's own argv syntax.

### Writing

`DesktopEntry::install` writes through a sibling temp file
(`.<name>.tmp-<pid>`) with `create → write → sync_all → rename`, then fsyncs the
directory. The parent directory is created when missing
(`~/.local/share/applications`). An identical file is left untouched, so a
startup that changes nothing does not touch the applications directory; a
changed `Exec` replaces it. Removal is `DesktopEntry::remove` (also exposed as
`pitagames-cli desktop-entry --remove`).

### When the GUI writes it (`app::desktop_entry`)

`main` calls `desktop_entry::ensure()` before opening the window. It writes
`$XDG_DATA_HOME/applications/pitagames.desktop` unless one of these is true:

- **A packaged entry exists** in an `XDG_DATA_DIRS` directory
  (`/usr/local/share`, `/usr/share`). A user-level entry shadows a packaged one,
  so generating it would point the menu at a stale checkout.
- **`PITAGAMES_NO_DESKTOP_ENTRY`** is set and non-empty. Packagers (the
  PKGBUILD task) own the applications directory; this is their off switch.
- **There is no data directory** (`HOME` unset): there is nothing to write.

A failure is a warning on stderr; the window still opens.

### CLI

```sh
pitagames-cli desktop-entry                  # state: absent / current / stale, packaged?
pitagames-cli desktop-entry --print          # the rendered file
pitagames-cli desktop-entry --write          # install or refresh
pitagames-cli desktop-entry --remove         # delete
pitagames-cli desktop-entry --exec <path>... # a different argv
pitagames-cli desktop-entry --path <file>    # somewhere else
pitagames-cli desktop-entry --json           # machine-readable
```

## Single-instance focus (`core::activation`)

The lock says "someone else is running"; the activation socket is how the second
process tells that someone to come forward.

- **Path:** `<XDG_DATA_HOME>/pitagames/instance.sock`, next to the lock. The
  path is limited to 100 bytes (Linux allows 108 including the NUL, macOS 104);
  a longer `XDG_DATA_HOME` disables focus forwarding with a warning instead of
  failing at `bind`.
- **Protocol:** one line of text, one request, one acknowledgement:
  `focus\n` → `ok\n`. Nothing else is carried — no data, no credentials, no
  shell. An unknown request is acknowledged with `unknown` and ignored.
- **Permissions:** the socket file is `0600`, so only the same user can ask.
- **Stale sockets:** a file left by a crash is probed with `connect`; a listener
  answers (`AlreadyRunning`), a corpse refuses the connection and is removed
  before binding. `Drop` removes the socket on a clean exit; the kernel does not
  for `SIGKILL`, which is exactly the stale case.
- **Timeouts:** a client that connects and says nothing is dropped after two
  seconds, so a stray connection cannot wedge the accept loop.

### In the GUI (`app::activation`)

`main` binds the socket right after taking the lock (so only the owner listens)
and spawns one thread running `accept` — it blocks, so it cannot live on the
event loop. Each request goes into a `futures_channel` receiver that a
`use_future` in `App` polls; the future runs on the event-loop thread and calls:

```rust
window.set_visible(true);   // was it hidden?
window.set_minimized(false);
window.set_focus();
```

Tao window calls are only safe on the event-loop thread, which is why the socket
thread never touches the window. Calls are best effort: a Wayland compositor may
refuse focus (focus-stealing prevention) and the window is still raised and
un-minimised. Activation tokens are the documented fallback and belong to the
M2.5/v1.1 session work.

When the lock is busy, the second launch calls
`core::activation::request_focus` and prints either
`PitaGames is already running; asked it to come to the front.` or a warning
naming why the request failed, then exits `0` — the same exit code as before, so
a desktop launcher sees a successful activation.

The CLI exposes the client side for scripts and diagnostics:

```sh
pitagames-cli activate            # or --socket <path>, --json
```

## Tray icon (`app::tray`, `app::icon`)

Dioxus forwards tray and menu events into its event loop, but its
`init_tray_icon` **panics** when the tray cannot be built. A launcher must not
die because a status-notifier host is missing, so the icon is built here with
`TrayIconBuilder` instead:

- menu: **Show PitaGames** and **Quit PitaGames** (with a separator),
- left click: Dioxus's built-in handler makes the window visible and focuses it
  (`tray_icon_show_window_on_click`, on by default) — the menu is on the right
  click,
- tooltip: `PitaGames`,
- icon: the painted mark (see below).

`tray::build_optional` returns `Option<Tray>` and prints
`[pitagames] no tray icon: …` when the platform refuses. The menu handler is
registered unconditionally (hooks must not be conditional) and no-ops without an
icon. "Quit" closes the window through Dioxus; the default close behaviour ends
the event loop when the last window is gone.

`libappindicator` is worth a note: when neither `libayatana-appindicator3` nor
`libappindicator3` can be loaded it **panics** inside `dlopen` instead of
returning an error. `build_optional` therefore mutes the panic hook for that one
call and runs it under `catch_unwind` — the container reproduced exactly this,
and without the catch the panic unwound through the render and killed the
window. With it, the log shows the warning above and the window opens normally.

### Known limit: no close-to-tray

Closing the window still quits the launcher. Hiding on close is a behaviour
change users notice (a background process that keeps scanning), so it is not
bundled into this slice; the tray is for coming back from Steam, not for keeping
the launcher resident.

### Icon (`app::icon`)

The icon is painted in code: a rounded tile (`#2e3a56` → `#1a1f2e` gradient) with
a play triangle (`#8fb0e8`), at any size (`DEFAULT_SIZE = 64`). There is no SVG
rasteriser in the dependency set and no PNG asset to ship, so the same geometry
drives both the window icon (`tao::window::Icon`) and the tray icon
(`trayicon::Icon`). `packaging/pitagames.svg` is the hand-written 256 px twin;
its geometry (`8/52/100,74 → 100,182 → 186,128`) and the three colours are the
same constants. Tests assert the buffer shape, transparent corners, the gradient
direction, the accent-coloured triangle and that a 32 px icon is still the same
mark.

`Icon=pitagames` in the entry is a **theme** name: `packaging/PKGBUILD` installs
the SVG into `hicolor/scalable/apps` (see `docs/m1-packaging.md`), so an
installed launcher shows the painted mark in menus and docks too. An uninstalled
checkout has no theme icon yet and falls back to the desktop's missing-icon
placeholder, while its window and tray already use the painted RGBA version.

## Guarantees and limits

- **Guaranteed:** a checkout writes a launchable menu entry pointing at the
  running binary; an unchanged entry is not rewritten; a packaged entry always
  wins; a second launch raises the running window and exits `0` (when the socket
  exists); a missing tray host is a warning; no failure in any of this prevents
  the window from opening.
- **The socket is advisory**, like the lock: another program could ask for focus
  if it runs as the same user, which is a harmless request.
- **Wayland focus** may be refused by the compositor; the window is raised but
  the compositor decides whether it gets keyboard focus.
- **No TV / hardware validation.** Focus forwarding and the tray are untested on
  the two target machines and on a TV-sized display.
- **No autostart and no `--hidden` start**: the entry starts the window, by
  design (the console shell is M2.5).

## Verification

- `pitagames-core`: **81 tests**, including the new `desktop` (13, among them the
  `packaging/pitagames.desktop` sync test and the `%`/quote/backslash quoting
  cases), `activation` (7: round trip, unknown request, second bind, stale
  socket, path limit, no listener, silent client) and `paths` (2: socket and
  entry locations under `XDG_DATA_HOME`) tests.
- `pitagames-cli`: **29 tests**, including the new `desktop-entry` parser,
  write/read/remove round trip, and `activate` argument handling.
- Pure crates: `cargo fmt`, `cargo clippy --all-targets -D warnings` and tests
  clean locally (core 81, db 22, art 14, metadata 28, platform 69, stores 98 unit
  + 13 integration, secrets 9, shim 8, cli 29).
- `crates/app`: builds, clippy-clean and **73 tests** pass in `archlinux:latest`
  (66 before this slice; the new ones cover the `icon` buffer geometry and the
  `activation` channel round trip).
- **Xvfb smoke run** in the container (`Xvfb :99` + `dbus-run-session`, an empty
  `PITAGAMES_STEAM_ROOT`): the window came up titled `PitaGames`, the startup
  wrote `$XDG_DATA_HOME/applications/pitagames.desktop` with
  `Exec=/target/debug/pitagames` and it matched the renderer
  (`desktop-entry --json` reports `"current": true` for that `--exec`), a second
  `pitagames` printed `PitaGames is already running; asked it to come to the
  front.` and exited `0`, and `pitagames-cli activate` exited `0` with
  `{"activated": true, "socket": …}`. The GUI log shows
  `[pitagames] raised the window for a second launch` once per request.
- **Tray failure path**: without a status-notifier library the run logged
  `[pitagames] no tray icon: no status-notifier host (install …); the window
  still works` and the window rendered normally (screenshots in
  `target/smoke/shot-desktop-*.png`).
- **Tray with its library**: the installed package pulls
  `libayatana-appindicator`; the tray then builds and the log only carries
  upstream's `libayatana-appindicator is deprecated` warning. The container has
  no StatusNotifier host, so nothing is displayed and the icon's visual
  behaviour is still unverified.
