# PitaGames beta tester checklist

This is the walk a beta tester runs: from a clean install to a launched game,
including the console path. It is the gate that turns a "not tested by us" /
"not tested on a TV" claim (ADR-0004) into a checked one.

> **Running the program is a human task.** Every checkbox below is a human
> observation at a real machine: only you can install the package, launch the
> GUI, plug in a pad and watch a game render. Nothing here is automated — the
> project's agents and CI build and test the pure crates in an `archlinux:latest`
> container, but no container has a GPU, a controller or a TV. Your result **is**
> the evidence; there is no telemetry, and nothing is uploaded on your behalf.

Beta testers are a small, hand-picked group on **CachyOS or Arch Linux, x86_64**,
with **native (non-Flatpak) Steam**. If you are not sure whether your machine
qualifies, open a hardware report and ask.

## What you need

- A CachyOS/Arch machine, x86_64, with native Steam installed.
- One of the GPUs below (anything else is welcome but ships `experimental`):
  - **RX 9070** (RDNA 4, 16 GB) — the primary validated path.
  - **RX 5700 XT** (RDNA 1, 8 GB) — the second validated path.
- For the console path: a gamepad (USB or Bluetooth) and, ideally, a **TV**.
- A D3D11-era Windows game to prove the shim (the M0 closer).

## How to report a result

- Something is broken → open a **Bug report**.
- Your machine's result (pass *or* fail) → open a **Hardware report**.
- Each template asks for the build, the environment and the logs it needs; the
  two are linked from the "New issue" chooser.

Keep one report per machine and one issue per problem. Attach evidence rather
than pasting it when a file is large.

## 0. Record your machine

Before anything else, fill in the top of a **Hardware report** so the result can
be attributed:

| Field | Value |
|---|---|
| GPU / driver / Mesa | |
| CPU / RAM | |
| Distro / kernel / session | |
| Display (monitor, or TV model + resolution) | |

## 1. Clean install

Start from a clean state so a stale config cannot hide a bug. Removing these
directories is safe — they hold the launcher's cache and settings, not your
games:

```bash
# Quit PitaGames first (the single-instance lock must be free).
rm -rf ~/.config/pitagames ~/.local/share/pitagames

# Build and install the package (pulls the WebKitGTK/GTK runtime stack).
cd packaging && makepkg -si
```

- [ ] `makepkg -si` finishes without error and installs `pitagames-git`.
- [ ] `pacman -Ql pitagames-git` shows `/usr/bin/pitagames`,
      `/usr/bin/pitagames-cli`, the shim and the `.desktop` entry.
- [ ] `pitagames --version` prints a version.
- [ ] A menu entry "PitaGames" appears and launching it opens the window.
- [ ] On first start, `~/.local/share/applications/pitagames.desktop` is written
      (the generated entry pointing at the running binary).

> If you prefer to test a prebuilt archive instead of building, say so in the
> report; the checklist otherwise assumes the package.

## 2. Verify the pieces before touching a game

These are read-only diagnostics. They prove the scan, Proton discovery and the
store rows independently of the GUI:

```bash
pitagames-cli scan                 # installed Steam games (not Proton/runtimes)
pitagames-cli scan --json          # attach this to your report
pitagames-cli proton               # every Proton root scanned + the auto choice
pitagames-cli stores               # each CLI store's program, version, session
pitagames-cli compat               # the shipped compatibility database loads
```

- [ ] `scan` lists your installed Steam games and nothing that is not a game.
- [ ] `proton` finds at least one launchable Proton build.
- [ ] `stores` runs and reports each store as present/absent honestly.
- [ ] `compat` prints the database with no validation warnings.

If `scan` is empty, point it at the right root and retry:
`PITAGAMES_STEAM_ROOT="$HOME/.local/share/Steam" pitagames-cli scan`.

## 3. Launch the desktop shell

```bash
pitagames          # windowed, decorated desktop shell
```

- [ ] The window opens to the library grid with cover art (or placeholders while
      art downloads).
- [ ] Search and the filter chips narrow the grid.
- [ ] Opening a game shows the detail page (Profiles, Runtime, About).
- [ ] Starting the launcher a second time **raises the existing window** instead
      of opening a second one (the single-instance lock).
- [ ] Closing the window quits the launcher.

## 4. Launch a real Steam game (the M0 closer)

This is the core end-to-end proof: the grid routes through the shim and Steam
launches the game with the injected environment. The full procedure, including
the exact launch options, is in [`docs/M0-test.md`](M0-test.md); the short form:

1. Pick a **D3D11** Windows game running through Proton.
2. Set its Steam launch options to
   `/usr/bin/pitagames-shim <appid> %command%` (or use the Runtime panel to
   route it).
3. Launch it from PitaGames.

- [ ] The game starts and the **DXVK FPS counter** is visible in a corner.
- [ ] A pre-existing `DXVK_HUD` value in your environment wins over the default.
- [ ] Closing the game returns to a responsive launcher.
- [ ] Record the appid, Proton build and whether the HUD appeared.

## 5. A store game (optional, if you have a store CLI)

Only run this if you have `legendary` (Epic), `nile` (Amazon) or
`lgogdownloader` + `gogdl` (GOG) installed. These backends have not yet been
driven against a real store CLI, so a result here is especially valuable.

- [ ] On the Stores screen, **Sign in** opens the in-app terminal; paste the one
      line it asks for; the row re-probes to "signed in".
- [ ] **Sync libraries** fills the grid with the store's owned/installed games.
- [ ] Install a small game: it appears on the Downloads board with progress and a
      Cancel button.
- [ ] The game's card flips to installed and **Play** launches it.
- [ ] A finished session shows up on the History screen.

## 6. The console path

This is the couch shell: same data and routes, 10-foot chrome, operable with a
pad alone. The full reference is [`docs/console-mode.md`](console-mode.md).

```bash
pitagames --console       # or press F11 / the pad's Guide from the desktop shell
```

- [ ] The window is **fullscreen and undecorated**, and nothing is cut off at the
      edges. If it is, raise `[ui] safe_area_pct` in
      `~/.config/pitagames/config.toml` and retry.
- [ ] With **no mouse or keyboard attached**, on the Home: `Down` enters the
      shelves; `Left`/`Right` walk a shelf and it scrolls; `Confirm` opens a game.
- [ ] `Tab`/`Q` (or `LB`/`RB`) cycles every destination and wraps; the top strip's
      highlight follows; nothing is stranded.
- [ ] On the Library: `Down` from the top tile row reaches the filter chip row;
      `Up` returns; **Installed**, a store chip and **Sort** all work, and `Back`
      clears a narrowing filter before it leaves.
- [ ] `Search` opens the on-screen keyboard; a direction walks the key ring and
      `Confirm` types.
- [ ] On the game page, the footer names the page's controls and `Confirm`
      plays/installs.
- [ ] **Launch a supervised game**: the window hides while the game runs and
      comes back when it exits, with focus returned. If the compositor refuses
      focus, the "game ended — press Guide" badge appears and `Guide` dismisses it.
- [ ] The cursor hides while only the pad is used and returns on pointer motion.
- [ ] `Start` opens the overlay: `Resume` closes it; the controller and clock are
      correct; the power rows read as not-yet-available (they arrive in v1.1).
- [ ] On Controls, rebind one action by pressing a new control; `Save` applies it
      live and the footer hint updates; `Discard` reverts.
- [ ] If no pad is detected, the Controls screen shows the actionable
      controller-permission line (the `input`-group fix) rather than nothing.

## 7. The TV checklist

A TV is the one thing neither in-house machine has, so a tester with a TV is the
only way a TV claim can be checked. The canonical checklist lives at the end of
[`docs/console-mode.md`](console-mode.md#tv-checklist-not-yet-run); run every
line there and record the result beside it.

- [ ] Overscan: nothing is cut off; the safe-area margin is right.
- [ ] Legibility at ≈ 2–3 m: titles, captions and badges read without leaning in.
- [ ] The focus ring is obvious at a glance.
- [ ] Contrast holds in a bright room and a dark room.
- [ ] A protected game's anti-cheat badge is noticed before anyone enables
      injection.
- [ ] **Attach a photo of the screen** — that is the evidence for every line above.

## 8. Recovery: safe mode

If a GPU/compositor combination cannot composite WebKitGTK's fast path, the
window can come up black. Safe mode is the way back, and it must work:

```bash
pitagames --safe-mode
```

- [ ] The window renders and a `Safe mode` chip is shown in the header.
- [ ] Animations and blur are off.
- [ ] (If you can reproduce a black window) starting normally retries once in
      safe mode automatically instead of leaving you stuck.

## 9. Report

Open a **Hardware report** with the result of every section above — pass or fail
— and attach `pitagames-cli scan --json`, the relevant transcript from
`~/.local/share/pitagames/logs/`, and any screenshot/photo. A pass on the
**RX 9070** or **RX 5700 XT** is what retires the "not tested by us" marker; a TV
pass is what retires "not tested on a TV".

If a step failed, open a **Bug report** for it (one issue per problem) and link
the two. Do not paste a Steam Web API key, a store token, or an auth URL/code
into either one.
