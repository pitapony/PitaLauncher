# PitaGames user guide

PitaGames is a game launcher for **CachyOS and Arch Linux** — a single library
across Steam and the other stores, one place to set up how a game runs, and a
10-foot console shell for the couch. It is not a store and it does not replace
Steam: it reads what is already on the machine, launches through the tools that
are already there, and stays out of the way.

> **Honest status.** Only the **RX 9070** (RDNA 4) and **RX 5700 XT** (RDNA 1)
> are validated by the project. Everything else ships `experimental` and is
> labelled **"not tested by us"**. No TV is attached to either in-house machine,
> so every couch/TV claim ships **"not tested on a TV"**. The launcher's real
> end-to-end hardware check — launching one Steam game with `DXVK_HUD=fps` — is
> still open. See [ADR-0004](adr/0004-validated-hardware-and-compatibility-claims.md)
> and [`docs/beta-testing.md`](beta-testing.md).

## Install

The package builds the GUI, the CLI and the shim and installs the menu entry, the
theme icon and the compatibility database:

```bash
git clone https://github.com/pitapony/PitaLauncher pitagames
cd pitagames/packaging
makepkg -si
```

`makepkg` pulls the runtime stack (`webkit2gtk-4.1`, `gtk3`, `openssl`,
`xdotool`, `libayatana-appindicator`, `xdg-utils`). From a checkout you can also
run straight from the tree — `cargo run -p pitagames` — but the GUI only builds
against Arch's WebKitGTK.

Requirements: **CachyOS or Arch, x86_64**, **native Steam** (Flatpak Steam is
unsupported), and Rust 1.89+ to build. A controller, `umu-launcher`, Proton
builds, gamescope/MangoHud/gamemode and the store CLIs are optional and are
`optdepends`.

## First run

```bash
pitagames            # the windowed desktop shell
```

The desktop shell is a frameless 1280×800 window with a drag header. On first
start the launcher:

- takes a single-instance lock, so a second launch **raises the running window**
  instead of opening a second one (run `pitagames-cli activate` yourself to test
  it);
- writes a menu entry to `~/.local/share/applications/pitagames.desktop` pointing
  at the binary you are running;
- scans the Steam library and shows it in the grid.

If the window comes up black, use **safe mode**: `pitagames --safe-mode`
(see [Safe mode](#safe-mode)).

## The window, screen by screen

The desktop header has a search box and a row of nav pills. The console shell
shows the same destinations as a passive strip and cycles them with `Tab`
(see [Console mode](#console-mode)).

### Home

The couch start screen: a **Continue** hero for the game you played last, then
horizontally-scrolled shelves. The same tiles the Library uses.

### Library (`View::Library`)

A grid of everything you own or have installed, across every store. Above it:

- **Search** — type to filter. In console mode, `Search` opens an on-screen
  keyboard instead.
- **Chips** — `Installed`, `Updates`, one chip per non-empty store
  (`Steam`/`Epic`/`Amazon`/`GOG`/`Local`), and `Sort: Title | Playtime | Recent`.
  Store chips multi-select; selecting none means every store.
- **`Updates`** keeps only games that need attention (an update is available, or
  the store flagged the install for verification). It is **not** the compat-DB
  claim "verified", and not the store's file-integrity `verify` command.
- **`Sort`** reorders; the other chips narrow. `Back` clears a narrowing filter
  before it leaves the screen.

Tiles show capsule art (a placeholder until art downloads) and a badge when a
game is owned but not installed.

### The game page (`View::Detail`)

Opening a game shows a full-bleed hero with **only true badges** — store,
install/update state, a red anti-cheat badge only when the game is actually
protected, and a forced-Proton badge only when Steam is pinned to a build. The
page's **primary action** is one press:

| You see | Meaning |
|---|---|
| **Play** | The game can start, and its store can launch it. |
| **Install** / **Update** | The files are missing (or stale) and the store has an installer. |
| A sentence | Why it cannot start yet (no executable, no Proton, protected, …). |

An **update waiting does not block play** — an installed game still shows Play,
and the amber badge says an update exists. Below the fold are the panels:

- **Profiles** — named launch profiles.
- **Runtime** — pick a Proton build, edit the wrapper chain (gamemoderun,
  MangoHud, gamescope options, ordered custom `program + args` rows), set
  diagnostics and see the exact resolved command (with a Copy button). For a
  Steam game this panel can also route the game through the shim and **force or
  clear the build in Steam**.
- **Graphics** (`View::Graphics`) — the upscaler/frame-gen/injection choices your
  machine may actually offer. A hidden option is absent, and the reason it is
  hidden is shown next to the section. One injection slot: Tier A, Tier B,
  HelixSR and DLSSNR are mutually exclusive.
- **About** — cached Steam store metadata.
- **Recent launches** — the last few supervised sessions for this game.
- **Anti-cheat** — the detection result and the injection warning.

### Stores

One row per CLI store — **Epic** (`legendary`), **Amazon** (`nile`), **GOG**
(`lgogdownloader` + `gogdl`) — with its program, version, sign-in state and game
counts. From here:

1. **Sign in** opens the in-app terminal: it streams the CLI and asks for the one
   line the CLI needs (Epic a code, Amazon/GOG a pasted browser redirect URL).
   Closing the panel cancels the flow. The command-line equivalent is
   `pitagames-cli <store> auth`.
2. **Sync libraries** reads the store's owned and installed games and folds them
   into the grid.

None of the three backends has been driven against a real store CLI by the
project, so a result here is especially valuable — see
[`docs/beta-testing.md`](beta-testing.md).

### Downloads

Install tracking for **Steam** (`steam://install` plus appmanifest polling with
byte progress) and for the **CLI stores** (a serial queue: install progress, the
target directory, Cancel while queued or running, Retry, Remove, Clear
finished). A running store install can be stopped from its row; whatever the CLI
already wrote stays, and the store's own repair can fix it later.

### History

One row per supervised (non-Steam) session the launcher started — including a
launch that never started, with its reason: title, start/end, mode, exit code,
signal, duration, verdict and a link to the transcript. The detail page shows the
same rows in its Recent launches card. `pitagames-cli history` is the read-only
equivalent; Clear history is a two-step action on the screen.

### Account

Set, replace or forget the **Steam Web API key** (kept in the desktop keyring,
falling back to a `0600` file). The key lets the launcher list the games your
account owns but has not installed, and is the only secret the project holds. It
is shown masked and never logged.

### Controls

Remap the keyboard or a pad. Every action is rebindable by **pressing the new
control**, not by picking from a list; a control of the wrong kind is refused, and
a control is stolen from any other action so one control never drives two. **Save**
writes `<config>/pitagames/input.toml` and applies the new maps live; **Discard**
reverts. Pads are keyed by their SDL GUID, so a remap survives a replug. If a pad
is present but unreadable, the screen shows the fix (`sudo usermod -aG input
$USER`, then log out and back in).

### Logs

A read-only tail of the per-run transcripts under
`~/.local/share/pitagames/logs/` — `pitagames-cli logs` is the same reader.

## Console mode

The controller-first shell: same data and same routes, 10-foot chrome, operable
with a pad alone. Enter it with `pitagames --console`, the pad's **Guide**, or
`F11`; the switch is live and remembered at next start.

- The pad drives focus; only navigation and tab switching repeat.
- The **Library** has the same filter chips, walked with the d-pad; `Search`
  opens the on-screen keyboard; `Back` clears a filter first.
- The **game page** is a full-bleed hero with a `Play`/`Overlay`/`Back` footer.
- Starting a supervised game **hides the window** and brings it back when the game
  ends.
- `Start`/`M` opens the **overlay**: Resume, the game's details/store page, the
  shell switch, and the power rows (enabled in the console session).

The full reference — controls, remapping, the top bar, the TV checklist — is
[`docs/console-mode.md`](console-mode.md). It is **not tested on a TV**.

## The console session (v1.1)

PitaGames can be a login-screen session: pick **PitaGames (console)** in the
display manager and one `gamescope` owns the output and runs the launcher's
systemd unit. A launched game renders in that one compositor — **no nested
gamescope**. The session is never the default session. `pitagames-cli session`
reports the two generated files; `--print` shows the exact `gamescope` argv.
Details and the session checklist are in
[`docs/console-mode.md`](console-mode.md) and
[`docs/v1.1-session.md`](v1.1-session.md).

## Safe mode

If the GPU/compositor cannot composite WebKitGTK's fast path, the window can come
up black. Safe mode turns off WebKit's DMABUF renderer and all animation and blur:

- `pitagames --safe-mode` (this run);
- `[ui] safe_mode = true` in `<config>/pitagames/config.toml` (sticks);
- the launcher auto-retries into it once if the GUI child dies before its first
  render, and restarts a window that crashes after it (bounded).

A `Safe mode` chip in the header explains how to leave it. See
[`docs/m2.5-safe-mode.md`](m2.5-safe-mode.md).

## The command-line tools

`pitagames-cli <command>` is read-only by default and useful on its own. The full
list is `pitagames-cli --help`; the common ones:

| Command | What it does |
|---|---|
| `scan [--json]` | List installed Steam games (not Proton/runtimes). |
| `proton` / `proton update` | List Proton builds; fetch a pinned Proton/GE build (opt-in). |
| `stores` / `stores updates` | Each CLI store's status; the unified update list. |
| `stores sync` | Refresh the CLI stores' library rows. |
| `stores update --store <name> <id>` | Enqueue a store update (rides the install queue). |
| `steam-account [--set-key\|--forget-key\|--refresh]` | Manage the Steam Web API key and the owned list. |
| `compat` | List or validate the shipped compatibility database. |
| `anticheat <appid> [--dir <path>]` | Detect anti-cheat and show the injection gate. |
| `eac [<appid>]` | Report EAC Linux v2 readiness and the game's verdict. |
| `gpu` / `capability` / `nr` | The GPU, which graphics options are offerable, the NR backend/pin. |
| `launch-options <appid>` | Read/set/shim/restore a game's Steam launch options. |
| `compat-tool <appid>` | Read/force/clear/restore a game's compatibility tool. |
| `profile <appid>` | Print the compiled shim profile. |
| `prefix` / `disk` | One non-Steam game's prefix and umu env; free-space checks. |
| `history [--clear]` | The launch history. |
| `logs [--tail n] [--grep text]` | Tail the launcher's log directory. |
| `import` | List what Heroic, Lutris and Bottles have installed (read-only). |
| `mods` | Show/hash/verify/restore a mod manifest; the release/ini/capture primitives. |
| `perf check` | Check a performance measurement against the budgets. |
| `desktop-entry` / `activate` / `session` | The generated entries and single-instance focus. |
| `epic` / `amazon` / `gog` | Drive a store backend directly. |

From a checkout prefix any of these with `cargo run -p pitagames-cli --`.

## Where things are kept

| What | Where |
|---|---|
| Games and profiles, metadata, owned games, history | `~/.local/share/pitagames/library.db` |
| Per-run logs | `~/.local/share/pitagames/logs/` |
| Non-Steam Wine/Proton prefixes | `~/.local/share/pitagames/prefixes/` |
| Settings (shell, display, retries) | `~/.config/pitagames/config.toml` |
| Controller maps | `~/.config/pitagames/input.toml` |
| Compat DB overrides | `~/.config/pitagames/compat/` |
| Steam Web API key fallback | `~/.config/pitagames/secrets/` (`0600`) |

## Getting help

- Something broken → the **Bug report** template.
- Your machine's result, pass or fail → the **Hardware report** template. A pass
  on the RX 9070 or RX 5700 XT retires a "not tested by us" marker; a TV pass
  retires "not tested on a TV".
- The walk a tester runs: [`docs/beta-testing.md`](beta-testing.md).
- A symptom and its fix: [`docs/troubleshooting.md`](troubleshooting.md).
- What the code does and why: [`docs/architecture.md`](architecture.md) and
  [`docs/adr/`](adr/README.md).

Never paste a Steam Web API key, a store token, or an auth URL/code into a report.
