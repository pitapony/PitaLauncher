# PitaGames console mode — the couch reference

> **Not tested on a TV.** No TV is attached to either in-house machine (ADR-0004).
> Everything here is verified on 1080p/4K **monitor proxies** and with a keyboard
> standing in for a pad; overscan, 10-foot readability and couch-distance focus
> return ship marked *not tested on a TV* until a community tester runs the
> checklist at the end of this document.

Console mode is the controller-first shell. It is the **same data and the same
routes** as the windowed desktop shell — `View::Library`, `View::Detail`,
`View::Home` and the rest — with 10-foot chrome: an undecorated fullscreen
window, larger type and hit areas, a passive destination strip instead of a
pointer nav, a d-pad-driven filter chip row, a full-bleed game page and an
auto-hidden cursor. It is built to be operable **with a pad alone, no pointer**.

The implementation is documented per slice under `docs/m2.5-*.md`; the
console-library slice is `docs/m2.5-console-library.md`.

## Entering and leaving

| Way in / out | How |
|---|---|
| Command line | `pitagames --console` (the desktop shell is `--desktop`) |
| Persisted start mode | `[ui] start_mode = "console"` in `<config>/pitagames/config.toml` |
| Pad | the `Guide` button (`Mode`) toggles the shell from anywhere |
| Keyboard | `F11` toggles the shell from anywhere |

The switch reconfigures the live window (`set_fullscreen`, `set_decorations`,
`set_maximized`) and remembers the shell; it never restarts the process. Entering
the console shell lands on the couch **Home**; leaving it returns to the Library
grid (a screen other than Home is left alone, so a pointer user is not thrown out
of it). `Esc` is **Back** — it clears a filter or leaves a screen, it does not
leave console mode. The windowed shell is still decorated; frameless desktop
chrome needs custom close/minimize controls and is deferred.

## Controls (the built-in layouts)

Every screen reacts to the closed 12-value `Action` set, never to a raw glyph, so
a remap changes the control without touching a screen. The two built-in layouts:

| Action | Gamepad | Keyboard | What it does |
|---|---|---|---|
| Navigate | d-pad **or** left stick | arrows **or** `W`/`A`/`S`/`D` | move focus one cell |
| Confirm | `A` (south) | `Enter` / `Space` | activate the focused item |
| Back | `B` (east) | `Esc` | clear a filter / leave the screen |
| Search | `Y` (north) | `F` | open the on-screen keyboard |
| Menu | `Start` | `M` | open the console overlay |
| Guide | `Mode` | `F11` | toggle the desktop/console shell |
| Tab next | `RB` | `Tab` / `E` | cycle to the next destination |
| Tab prev | `LB` | `Q` | cycle to the previous destination |
| Context | `X` (west) | — | reserved; no screen consumes it yet |

Only navigation and tab switching repeat while held (400 ms delay, then 80 ms
interval); `Confirm`/`Back` stay one action so a stuck button cannot walk through
a stack of screens. A stick uses a radial deadzone and resolves to a single
dominant direction, so a diagonal pushes one cell.

The footer **hint row** names the actions the current screen offers, using the
glyphs of the **device used last** — the pad's `A`/`B`/`Y`/`Start`/`Guide` while a
pad is active, `Enter`/`Esc`/`F`/`M`/`F11` otherwise — and the pointer is hidden
after two seconds of pad-only input. A remap changes the row with no second
table. The console **game page** narrows the row to its own words: `Play`,
`Overlay`, `Back`.

## Remapping

A controller-only launcher cannot ask the user to edit a TOML file, so every
binding is changeable from the couch on the **Controls** screen (`Tab` to it, or
the desktop nav pill):

- The list picks a device (the keyboard, then each connected pad by name) and
  shows every `Action` with its current bindings, then **Reset all**, **Save**
  and **Discard**.
- Opening an action shows a rebind and a remove per binding slot, plus **Add
  binding** and **Reset action**.
- A rebind **asks for the next control you press** rather than offering a list of
  buttons, because the map may not know a button yet. A control of the wrong kind
  (a key while editing a pad, a button while editing the keyboard) is refused
  with a message. An accepted control is stolen from every other action, so one
  control never drives two actions.
- **Save** writes `input.toml` (`<config>/pitagames/input.toml`) and applies the
  new maps to the running pumps immediately; **Discard** reloads the stored map.

Pads are keyed by their SDL GUID, so a remap survives a replug. Only overrides
are stored: a device that matches its built-in layout writes nothing. The feel
numbers (deadzone, repeat delay/interval) are pointer-only number fields today —
a pad cannot type a number, so tuning them waits on a couch-numeric surface.
`Back` cancels a pending capture, which is why `Back` itself cannot be captured
into another action by pressing it (it can still be rebound, removed and reset).

## The console top bar and `Tab`

The console shell mounts **no text field**. Where the desktop header has a search
box, the console header draws a **passive destination strip** — every top-level
destination (`Home`, `Library`, `Downloads`, `History`, `Stores`, `Account`,
`Controls`, `Logs`) in the desktop pill order, with the current one highlighted.
The strip is an indicator, not a control: it is not focusable, has no hover rule
and never enters a focus graph, so it cannot become a focus trap.

`Tab` (`E` is a second binding) and `Q` cycle the view through the destinations
directly and wrap at the ends; the strip and the ring read **one** list
(`navigation::DESTINATIONS`), so a `Tab` step can never land on a screen the
strip does not show. Every destination stays reachable — nothing is stranded in
console mode. The detail page is not a destination: a `Tab` step off it lands on
the first (forward) or last (backward) destination.

## The Library: the filter chip row

The Library is a full-screen **10-foot tile grid** (larger tiles than the desktop
grid — `320 px` vs `220 px` minimum, so fewer, bigger cards at 1080p). Above the
grid sits a **d-pad-driven chip row**:

```
[Installed] [Updates] [Steam] [Epic] [Amazon] [GOG] [Local] [Sort: Title]
```

- **Installed** keeps only games whose files are on disk.
- **Updates** keeps only games that need an update or verification — the *same*
  list the store-status screen reports (`stores::status::updates`). This is the
  chip that used to be called "verified"; see the vocabulary note below.
- **Store chips** are shown only for stores that actually have a game, in product
  order (Steam, Epic, Amazon, GOG, Local). They **multi-select**: pick Epic and
  GOG together. Selecting none means every store.
- **Sort** cycles `Title` (A–Z), `Playtime` (most first), `Recent` (newest
  first); its label always names the current mode, and it reorders rather than
  narrows.

The chips and the tiles are **one ragged focus space**: the chips are the top row
(wrapping onto more lines when the window is narrow) and the tiles begin on the
row after the last chip row, so `Up` from the top tile reaches the chips and
`Down` from the chips reaches the tiles — with no trap and no unreachable chip.
`Confirm` on a focused chip toggles it; `Confirm` on a tile opens its game page.

- **Back clears before it leaves.** A couch player who filtered down and pressed
  `Back` means "undo the filter", so a narrowing filter (query, store subset,
  Installed, Updates) is cleared first; a second `Back` leaves to the Home. A
  `Sort` on its own is not a narrowing filter, so `Back` leaves without
  disturbing the order.
- **The filtered-empty state teaches the way out**: "Nothing matches the current
  filters. Press Back to clear them." (naming the query when there is one).
- **Search** opens the on-screen keyboard (bottom sheet) on the Library *and* the
  Home. A search started on the Home commits the query and lands on the Library
  so the matches are shown immediately. The sheet is walked by the same focus
  graph: a direction moves the key ring, `Confirm` types, and `Back` cancels.

The **same chip row and sort** are drawn in the windowed Library, over the same
`LibraryFilter` model, so filtering behaves identically in both shells. The
desktop shell additionally keeps its text `<input>`; the console shell does not.
Filters live in memory only: they survive a view switch and a shell toggle within
a session and reset on restart.

## The game page

Opening a game shows a **full-bleed hero** — the game's art behind a legibility
gradient, its title, its meta line and its one primary action. The page shows
**only true badges**; a fact it does not have is an absent badge, not a
placeholder:

| Badge | When | Tone |
|---|---|---|
| Store | always | neutral |
| Install / Update / Not installed | always (the install state word) | neutral for a plain install, amber otherwise |
| Easy Anti-Cheat / BattlEye | only when the game is actually protected (a marker on disk or the compat DB) | **danger-red**, loud |
| Forced Proton build | only for a Steam row Steam is pinned to a `CompatToolMapping` | amber |

The **primary action** is one press: **Play** when the game can play and its
store can launch it, **Install** when its files are missing and its store has an
installer (Steam or one of the CLI stores), else a sentence explaining why it
cannot start. An **update waiting does not block play** — an installed game still
shows Play, and the amber **Update** badge is what says an update exists. The
**existing detail panels** (Profiles, Runtime, Diagnostics, resolved Command,
Recent launches) are kept **below the fold**, so nothing becomes unreachable from
a controller.

The footer cheat-sheet on the game page names the page's own actions —
`Play`, `Overlay`, `Back` — in the last device's glyphs.

## The overlay

`Menu` (`Start` / `M`) opens the **console overlay** from any screen: **Resume**,
the running/open game's **Game details** and (Steam) **Store page**, **Exit
console mode** / **Enter console mode**, and the power rows (**Suspend**,
**Restart**, **Shut down**) shown but disabled until the console session ships
(v1.1, over logind). It also carries the connected controller and its battery and
the local clock. It is a modal: the pumps route every action into it while it is
open, `Back`/`Menu`/`Guide` closes it, and the screen beneath keeps its focus and
scroll. See `docs/m2.5-overlay.md`.

## The launch hand-off

Starting a supervised (non-Steam) game from the console shell **hides the
window** and pauses the input pumps while the game runs; when the game exits the
window is shown again and focus is requested. If the compositor refuses focus, a
"game ended — press Guide" badge appears on the next focus, and the first `Guide`
press dismisses it rather than switching shells. This is what makes "launch it,
quit it and land back in console mode" true. See `docs/m2.5-handoff.md`.

## Safe mode

If the GPU cannot composite WebKitGTK's fast path, the window can come up black —
a window the user cannot see is a window they cannot close. Safe mode is the way
in that does not need the GPU:

- **Three ways in, ORed together**: `--safe-mode` (this run),
  `[ui] safe_mode = true` in `config.toml` (sticks), and
  `PITAGAMES_SAFE_MODE=1` (what the supervisor sets when it retries).
- It sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` before WebKit starts and shows a
  `Safe mode` chip in the header explaining how to leave it.
- **Auto-retry**: the launcher is a supervisor process plus a GUI child; a child
  that dies *before* its first render (the DMABUF failure) is retried once in
  safe mode, and a crash *after* it is restarted up to three times. A black screen
  with no keyboard is not acceptable, so there is always a way back.
- Safe mode's "animations and blur off" holds **by construction**: there is no
  live `animation`, `transition`, `backdrop-filter` or `scroll-behavior` anywhere
  in `crates/app/src`, enforced by a source test rather than a stylesheet this
  Dioxus build would ignore.

See `docs/m2.5-safe-mode.md` and ADR-0007.

## Vocabulary: "Updates" vs "verified"

The chip word **Updates** is *not* the compat-DB claim **verified**, and neither
is the store `verify` command. They are three different things:

- **Updates** (the chip) — *this game needs attention*: an update is available
  (a Steam `InstallPhase::UpdateAvailable`) or the store tagged it
  `needs-verification`. Backed by `stores::status::updates`.
- **verified** (compat DB) — the *anti-cheat / compatibility claim* curated in
  `compat/*.toml`.
- **verify** (store command) — the store CLI's own file-integrity **repair**
  (`legendary verify`, `nile verify`, `gogdl repair`).

## The plan's names

The plan's **"console library grid"** is `View::Library` and its **"console game
page"** is `View::Detail` — there is no separate console screen set (ADR-0022: a
second set "duplicates every screen and drifts"). A `Shell` branch renders the
console chrome over the same routes.

## TV checklist (not yet run)

Hand this to a tester with a TV. Every line is unverified until it is checked
here; record the result beside it.

**Setup**

- [ ] `pitagames --console` on the target machine, a pad connected by USB or
      Bluetooth, no mouse or keyboard attached.
- [ ] The window is fullscreen and undecorated; nothing is cut off at the edges
      (overscan). If it is, raise `[ui] safe_area_pct` in `config.toml`.

**Reading at couch distance (≈ 2–3 m)**

- [ ] Titles, captions and badges are legible without leaning in.
- [ ] The focus ring on the focused tile/chip/row is obvious at a glance.
- [ ] A protected game's anti-cheat badge is noticed **before** anyone enables
      injection.
- [ ] Contrast holds in a bright room and a dark room.

**Pad-only walk**

- [ ] On the Home: `Down` enters the shelves, `Left`/`Right` walk a shelf and it
      scrolls; `Confirm` on the hero and on a tile both open the game page.
- [ ] `Tab`/`Q` (or `LB`/`RB`) cycles every destination and wraps; the strip's
      highlight follows; nothing is stranded.
- [ ] On the Library: `Down` from the top tile row reaches the chip row; `Up`
      from the chips returns; every chip is reachable even when the row wraps.
- [ ] Toggling **Installed**, a store chip and **Sort** narrows/reorders the grid;
      `Back` clears the filter before it leaves.
- [ ] A query that matches nothing shows the "press Back to clear" message and
      `Back` recovers.
- [ ] `Search` opens the on-screen keyboard; a direction walks the key ring and
      `Confirm` types.
- [ ] On the game page: the footer names the controls; `Confirm` plays/installs.
- [ ] Launch a game: the window hides; on exit it comes back and focus returns.
      If the compositor refuses focus, the "game ended — press Guide" badge
      appears and `Guide` dismisses it.
- [ ] The cursor is hidden while only the pad is used, and returns on pointer
      motion.

**Overlay, remap and recovery**

- [ ] `Start` opens the overlay; `Resume` closes it; the controller and clock are
      correct; the power rows read as not-yet-available.
- [ ] On Controls, rebind one action by pressing the new control; `Save` applies
      it live and the footer hint updates; `Discard` reverts.
- [ ] Start once with `--safe-mode`: the window renders and the `Safe mode` chip
      shows.

Report the results (pass/fail and any photo) back to the project so this document
can lose its "not tested on a TV" marker.
