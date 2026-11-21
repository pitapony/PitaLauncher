# ADR-0007: Dioxus webview with a renderer-agnostic boundary (and safe mode)

- **Status:** Accepted (renderer, boundary and safe mode)
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §7 (Renderer boundary, Dioxus implementation notes); `AGENTS.md` (Safe mode)

## Context

Dioxus 0.7 offers two renderers: the desktop webview (WebKitGTK) and
`dioxus-native` (Blitz, an alpha 0.8 on the 0.7 core). The 8 GB RX 5700 XT
machine cares about the webview's process model, and NVIDIA/Wayland users report
WebKit rendering bugs — but the webview is by far the more mature path today.
A black screen with no keyboard is unacceptable on a couch machine.

## Decision

- Ship **Dioxus 0.7 desktop (WebKitGTK webview)** as the v1 renderer.
- Keep a **renderer-agnostic boundary** (ADR-0006) so `dioxus-native`/Blitz can
  replace the webview later as a port of markup and CSS rather than a
  rearchitecture. If the WebKit process model proves too heavy on the 8 GB
  machine, the console shell migrates first and the desktop shell keeps the
  webview.
- `crates/app` is the only crate that knows about Dioxus. No `<video>` in the
  v1 UI, no JS-based navigation, no `<img>` inlining (art is served — see
  ADR-0008).
- **Safe mode (Accepted):** default to WebKit's DMABUF renderer; a webview init
  failure makes the supervisor relaunch the GUI child with
  `WEBKIT_DISABLE_DMABUF_RENDERER=1` and animations/blur off. `--safe-mode` forces
  that path for a run, and `[ui] safe_mode` in `config.toml` makes it stick (there
  is no Settings screen yet, so the file is the toggle — deliberately not the
  `.desktop` file, since disabling DMABUF unconditionally costs performance on
  the 9070). A bounded window watchdog restarts a *ready* window that crashes.
  See `docs/m2.5-safe-mode.md`.

## Consequences

- Positive: the mature renderer now; a credible escape hatch later, with the cost
  confined to `crates/app`.
- Positive: art is fetched over HTTP (not inlined), which also means a future
  native renderer serves the same bytes.
- Positive: a GPU that cannot composite lands in safe mode instead of a black
  screen, and a crashed window comes back on its own.
- Negative: safe mode is a second startup path to keep tested, and the WebKit
  memory ceiling (~300 MB while a game runs) is a real constraint on the 8 GB
  machine — mitigated by hiding the window during play (ADR-0022).
- Negative: the supervisor re-execs the binary, so the GUI is always a child
  process; the single-instance lock and the activation socket are split across the
  two roles (the supervisor locks, the child listens).
- Negative: only a *crashing* webview is detected. A renderer that initializes
  but paints nothing (a silent failure) still needs `--safe-mode` by hand.

## Alternatives considered

- **`dioxus-native`/Blitz in v1.** Rejected — alpha and immature; the boundary
  keeps it available.
- **Disable DMABUF unconditionally.** Rejected — costs performance on the
  validated hardware.
- **In-process retry (`catch_unwind` + `exec`).** Rejected — a failed GTK/WebKit
  init can abort rather than unwind, and the crash watchdog needs a parent process
  anyway, so the two share one supervisor.

## References

- plan §7 *Renderer boundary* and *Dioxus implementation notes*.
- `AGENTS.md` — *No `<video>`*, *Safe mode*, *Renderer boundary*.
- `docs/m1-art.md`, `docs/adr/0008`.
