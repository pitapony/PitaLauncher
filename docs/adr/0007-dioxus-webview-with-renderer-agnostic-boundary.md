# ADR-0007: Dioxus webview with a renderer-agnostic boundary (and safe mode)

- **Status:** Accepted (renderer and boundary); **Proposed** for safe mode
  (`--safe-mode` is not implemented yet)
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
- **Safe mode (Proposed):** default to WebKit's DMABUF renderer; if webview init
  fails, relaunch with `WEBKIT_DISABLE_DMABUF_RENDERER=1` and animations/blur
  off. `--safe-mode` forces that path. Keep the toggle in Settings, not in the
  `.desktop` file — disabling it unconditionally costs performance on the 9070.
  Console mode auto-retries into it so a GPU crash never leaves a black screen.

## Consequences

- Positive: the mature renderer now; a credible escape hatch later, with the cost
  confined to `crates/app`.
- Positive: art is fetched over HTTP (not inlined), which also means a future
  native renderer serves the same bytes.
- Negative: safe mode is a second startup path to keep tested, and the WebKit
  memory ceiling (~300 MB while a game runs) is a real constraint on the 8 GB
  machine — mitigated by hiding the window during play (ADR-0022).
- Until safe mode ships, a webview init failure on an unsupported GPU has no
  automatic recovery.

## Alternatives considered

- **`dioxus-native`/Blitz in v1.** Rejected — alpha and immature; the boundary
  keeps it available.
- **Disable DMABUF unconditionally.** Rejected — costs performance on the
  validated hardware.

## References

- plan §7 *Renderer boundary* and *Dioxus implementation notes*.
- `AGENTS.md` — *No `<video>`*, *Safe mode*, *Renderer boundary*.
- `docs/m1-art.md`, `docs/adr/0008`.
