# Troubleshooting

A symptom → cause → fix guide. Most problems fall into a handful of groups:
the GUI did not come up, a library is empty, a game did not start, a store CLI is
missing or signed out, or a TV/session claim does not hold. Every entry names the
diagnostic that tells you which it is.

Before anything else:

```bash
# Run the GUI from a terminal and read stderr.
pitagames

# The per-run transcripts (bounded tail, never the whole file).
pitagames-cli logs
pitagames-cli logs --tail 200 --grep ERROR
```

The launcher stores its settings in `~/.config/pitagames/`, its database, logs
and prefixes in `~/.local/share/pitagames/`. Deleting both resets it to defaults
— they hold the launcher's cache, not your games.

## The window

### The window comes up black, or never appears

A GPU/compositor that cannot composite WebKitGTK's fast path is the classic case.
Use **safe mode**, which sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` and turns off
animation and blur:

```bash
pitagames --safe-mode
# or set it to stick:
# [ui] safe_mode = true   in ~/.config/pitagames/config.toml
```

If a normal start dies before its first render, the launcher auto-retries once in
safe mode by itself; a crash after the first render is restarted up to three
times. If safe mode also fails, run from a terminal and read stderr, and check
that the runtime is installed: `pacman -S webkit2gtk-4.1 gtk3`. See
[`docs/m2.5-safe-mode.md`](m2.5-safe-mode.md).

### A second launch does not open a window

That is the single-instance lock working: a second launch asks the running
instance to come to the front and exits `0`. If it did not raise, check the
activation socket path and test it directly:

```bash
pitagames-cli activate
```

The shim and the CLI deliberately do not take the lock, so they run alongside the
GUI. If the window is on another workspace, the compositor decides whether it
raises. See [`docs/m1-single-instance.md`](m1-single-instance.md).

### No "PitaGames" menu entry

The launcher writes `~/.local/share/applications/pitagames.desktop` at startup
unless a packaged entry exists in `XDG_DATA_DIRS` or `PITAGAMES_NO_DESKTOP_ENTRY`
is set:

```bash
pitagames-cli desktop-entry --print      # show the entry it would write
pitagames-cli desktop-entry --write      # write it now
```

### No tray icon

The tray needs a StatusNotifier host (a panel that provides one). Without it the
launcher runs fine with no tray icon; that is not an error. See
[`docs/m1-desktop-integration.md`](m1-desktop-integration.md).

## The library shows nothing, or the wrong games

### The grid is empty but you have Steam games

The scan looks at the default Steam roots. Point it at the right one and retry:

```bash
PITAGAMES_STEAM_ROOT="$HOME/.local/share/Steam" pitagames-cli scan
pitagames-cli scan --json | head -c 600
```

- **Flatpak Steam is unsupported** (a non-goal). Use native Steam.
- A second library folder is scanned from `libraryfolders.vdf`; if it is on a
  drive that is not mounted, those games are absent until it is.
- A game in the list with no art yet shows a placeholder while art downloads.

### Games you own but have not installed are missing

Those come from the **Steam Web API**, which needs a key:

```bash
pitagames-cli steam-account --set-key      # or the Account tab in the GUI
pitagames-cli steam-account --refresh
```

The key goes environment → desktop keyring → `0600` file. In a container there is
no session keyring, so it lands in the fallback file with a warning — expected.

### The `Updates` chip and "needs verification"

`Updates` means *this game needs attention*: an update is available or the store
flagged the install. It is **not** the compat-DB claim "verified" and not the
store `verify` command (a file-integrity repair). Check the unified list:

```bash
pitagames-cli stores updates
```

## Proton and launching a game

### A Windows game will not start: no Proton found

Discover what the launcher can see; a Windows build needs one:

```bash
pitagames-cli proton
```

The scan covers system/user `compatibilitytools.d`, every Steam root, umu's data
directories and the launcher's own `proton/` directory. If a build should be
found, pass its roots explicitly (`--root`, `--steam-root`). A native Linux game
does **not** need Proton and starts directly. See [`docs/m1-proton.md`](m1-proton.md)
and [`docs/m2-launch.md`](m2-launch.md).

### A game starts but ignores the shim (no FPS overlay)

`DXVK_HUD` only appears for D3D9/10/11 games running through **DXVK**:

- a **native Linux** or **native Vulkan** game does not use DXVK;
- a **D3D12** game runs through VKD3D-Proton, not DXVK.

Confirm the shim itself first, then check the routing:

```bash
pitagames-shim 620 /usr/bin/env | grep '^DXVK_HUD='
DXVK_HUD=devinfo pitagames-shim 620 /usr/bin/env | grep '^DXVK_HUD='
pitagames-cli launch-options <appid> --shim --dry-run
```

A pre-existing user `DXVK_HUD` wins over the default. The full M0 procedure is
[`docs/M0-test.md`](M0-test.md).

### "first argument must be the numeric Steam appid"

In Steam **Launch Options** the order is appid first:

```text
/usr/bin/pitagames-shim <appid> %command%
```

The appid is the number in `appmanifest_<appid>.acf`. `%command%` stays exactly
as written and last. If Steam says `command not found`, use the absolute path.

### Shader compilation stutter on the first launch

Normal DXVK pipeline-cache warm-up on a new title or driver; relaunch once.

### A store game will not start

Check the store is signed in and its CLI is present, then its status:

```bash
pitagames-cli stores
pitagames-cli launch epic <id>       # or amazon/gog; add --dry-run to inspect
```

A GOG/Amazon row can launch through its **store client** (a black box — the
client owns its runtime), an Epic row through **umu** with a per-game prefix.
See [`docs/m2-launch.md`](m2-launch.md). A game freshly installed from a store may
need the next **Sync libraries** before its executable is known.

## Stores (`legendary`, `nile`, `lgogdownloader`/`gogdl`)

### A store row reads "CLI missing"

The launcher does not bundle or self-update store CLIs — that is your package
manager's job. Install the program the row names:

| Store | Programs |
|---|---|
| Epic | `legendary` |
| Amazon | `nile` |
| GOG | `lgogdownloader` + `gogdl` (both; neither does the other's job) |

```bash
pitagames-cli stores
```

### Sign-in fails or hangs

Open the Stores screen → **Sign in** (or `pitagames-cli <store> auth`). Epic needs
a code; Amazon and GOG open a browser and ask you to paste the redirect URL back.
Closing the panel cancels the flow. A refusal that still exits `0` is detected
from the CLI's own error lines, so a "signed in" badge means the session really
stored. See [`docs/m2-auth-terminal.md`](m2-auth-terminal.md).

Only local diagnostics: the PTY echoes what you paste into the on-screen
transcript. Never paste that transcript into a report without removing the code
or URL.

### An install fails, or reports success but did not install

Store CLIs sometimes log an error and exit `0`. The launcher classifies the
verdict from the CLI's lines, not the exit code:

```bash
pitagames-cli <store> install <id>       # the CLI shows the same verdict
pitagames-cli logs --grep ERROR
```

A *running* install can be stopped from its Downloads row; whatever the CLI
already wrote stays, and the store's own repair fixes it later. A game whose
files are corrupt uses the store's verify/repair (Epic: `legendary verify`;
Amazon: `nile verify`; GOG: `gogdl repair`). See
[`docs/m2-store-installs.md`](m2-store-installs.md) and
[`docs/m2-cancel-install.md`](m2-cancel-install.md).

### A store library is empty after Sync libraries

A failed listing never deletes rows — it leaves what was cached. Check the CLI's
own status, then retry; `--json` prints the per-store result:

```bash
pitagames-cli stores sync --json
```

### Imported games (Heroic/Lutris/Bottles) are missing

`pitagames-cli import` reads the launchers' own files read-only and only **installed**
games. It looks at the XDG roots plus the three Flatpak copies per launcher; pass
`--heroic <dir>`/`--lutris <dir>`/`--bottles <dir>` to point at a specific tree.
A missing file is reported as `(missing)`; nothing is launched or written. See
[`docs/m2-importers.md`](m2-importers.md).

## Steam config writes

The launch-options and compatibility-tool writers **refuse while Steam is
running** and always back up and replace atomically:

| Symptom | Cause | Fix |
|---|---|---|
| Refuses to write | Steam is running (`/proc` shows it) | Quit Steam and retry |
| Reports `Unknown` | The host cannot read `/proc` (e.g. macOS) | Retry with `--force` and an explicit `--localconfig`/`--backup-dir` |
| A change did not stick | Steam rewrote its config on exit | Apply with Steam closed |

Always inspect first: `pitagames-cli launch-options <appid> --shim --dry-run`.
Restore with `--restore`. See [`docs/m1-launch-options.md`](m1-launch-options.md)
and [`docs/m1-compat-tool.md`](m1-compat-tool.md).

## Controllers

### No pad is detected, or the launcher ignores it

Pad access normally comes from logind's `uaccess` tag on `/dev/input/event*`.
When it is missing, the Controls screen shows one line instead of silence:

```bash
sudo usermod -aG input $USER     # then log out and back in
```

The probe only ever opens a node read-only — the launcher never `EVIOCGRAB`s, so
a running game keeps receiving input. See
[`docs/m2.5-pad-permission.md`](m2.5-pad-permission.md).

### The pad works in the launcher but not in the game

That is outside the launcher: a game reads the controller through Steam Input, a
Proton prefix's own mapping or Wine's controller support. Check the game's own
input settings, and that the pad is visible to the system (`evtest`,
`ls /dev/input/`).

### A remap did nothing, or captured the wrong control

On **Controls**, a rebind asks for the next control you press; a control of the
wrong kind (a key while editing a pad) is refused, and an accepted control is
stolen from any other action. **Save** applies live; **Discard** reverts. A
device matching its built-in layout writes no override. See
[`docs/m2.5-remap.md`](m2.5-remap.md).

## Console mode and the TV

### Content is cut off at the edges

The TV overscans. Raise the launcher's safe-area margin:

```toml
# ~/.config/pitagames/config.toml
[ui.display]
safe_area_pct = 5
```

See the TV checklist at the end of [`docs/console-mode.md`](console-mode.md).

### After a game ended, the launcher did not come back focused

Normally the window is shown again and focus requested when the supervised game
exits. If the compositor refuses focus, a "game ended — press Guide" badge
appears on the next focus; the first `Guide` press dismisses it instead of
switching shells. See [`docs/m2.5-handoff.md`](m2.5-handoff.md).

### The console session is black or does not start

The login-screen session (pick **PitaGames (console)**) runs the launcher inside
one `gamescope`. First checks:

```bash
pitagames-cli session                       # are both files present and current?
journalctl --user -u pitagames-session      # the unit's own log
systemctl --user show-environment           # WAYLAND_DISPLAY / XDG_RUNTIME_DIR
systemctl --user daemon-reload              # after the unit was first written
```

- The unit must **inherit gamescope's `WAYLAND_DISPLAY`**; confirm both variables
  reached it.
- Newer systemd watches `~/.config/systemd/user`; older versions need the
  `daemon-reload`.
- **Resolution:** unset means gamescope uses the TV's preferred mode; pin one with
  `[ui.display.session] resolution = { width, height, refresh_hz }`.
- **HDR / VRR:** `hdr = true` adds `--hdr-enabled`; `vrr = true` adds
  `--adaptive-sync`. On hardware without them, nothing changes — not an error.
- **Monitor:** `[ui.display] monitor = "HDMI-A-1"` (connector name) prefers that
  output.

The whole session is **not tested on a TV** until a tester runs the checklist. See
[`docs/v1.1-session.md`](v1.1-session.md) and ADR-0004.

## Anti-cheat and EAC

### A protected game refuses injection

By design. Injection (OptiScaler, HelixSR, DLSSNR) on an Easy Anti-Cheat or
BattlEye game is gated behind an explicit confirmation, and the gate is not
bypassed. Confirm the detection:

```bash
pitagames-cli anticheat <appid> --dir /path/to/game
```

A real marker on disk always wins over the compat DB's curated value; `Unknown`
never hard-gates. See [`docs/m1-anticheat.md`](m1-anticheat.md).

### An EAC game says "needs the EAC entry"

**Easy Anti-Cheat Linux v2** runs only from the CachyOS **EAC entry** (the
`linux-cachyos-eac` kernel plus CachyOS-signed systemd-boot). Off it, the game
page explains what is missing and offers to reboot into the entry; the only
command it runs is `pkexec eac-setup boot`. Check the verdict:

```bash
pitagames-cli eac [<appid>]
```

Injection stays hard-off for a protected game even on the entry. The EAC kernel
loads only modules signed when it was built, so NVIDIA/ZFS/other DKMS modules may
not load there — the validated in-house AMD/Mesa paths need none. PitaGames's own
processes are user-space and run on the entry. See [`docs/eac-v2.md`](eac-v2.md)
and [`docs/console-mode.md`](console-mode.md).

## Graphics options and neural rendering

### An option I expected is not shown

An option your machine may not offer is **absent, not greyed**, and the reason is
shown beside the section. Ask the matrix directly:

```bash
pitagames-cli gpu
pitagames-cli capability <store> <id>
pitagames-cli nr <store> <id>
```

A hidden option carries the same reason string the docs table uses
([`docs/m3-capability.md`](m3-capability.md)). One injection slot: an injected
upscaler and DLSSNR are mutually exclusive (ADR-0023).

### DLSSNR did not engage

Two independent codebases back one installer UX; a card is only ever offered its
own backend (RDNA 4 → upstream, RDNA 3 → the fork). Check the plan, then whether
the mod engaged:

```bash
pitagames-cli nr <store> <id> --resolution 1440p
pitagames-cli support verify --log-dir <game dir> --check nr
pitagames-cli support bundle --game-dir <game dir> --out bundle.zip
```

The verification reads the mod's own log for an engagement marker. A working
install can be removed with the **Uninstall DLSSNR** button (upstream's own
removal when its ledger is present, else the launcher's manifest, which refuses
to clobber a tampered backup). Nothing here is validated on hardware by the
project. See [`docs/dlssnr.md`](dlssnr.md) and
[`docs/m3.5-nr-verify.md`](m3.5-nr-verify.md).

## Performance

The project's budgets are acceptance criteria, not a promise for every machine:

```bash
pitagames-cli perf check --file <measurement.json> --require cold-start,frame-time,memory
```

Cold start, scroll frame time and process-group memory are modelled in
`core::perf`; a software renderer under Xvfb cannot show 60 fps and is not the
machine's number. See [`docs/m4-perf-budgets.md`](m4-perf-budgets.md).

## Collecting a report

Open the matching issue template and attach, rather than paste, large output:

```bash
pitagames-cli scan --json
pitagames-cli proton
pitagames-cli stores
pitagames-cli logs --tail 200
```

Include the distro/kernel, GPU + driver + Mesa version, the launcher version
(`pitagames --version`) and the steps that reproduced it. **Remove any Steam Web
API key, store token or auth URL/code first** — the transcripts echo what was
typed.
