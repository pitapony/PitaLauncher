# Architecture Decision Records

Every decision PitaGames has locked in, one record each, in MADR-lite form.
An ADR states **what we chose and why**; the implementation detail lives in the
slice doc each record links to, and the plan
(`/Users/julienpohie/.opencode/plan/pitagames.md`) stays the source of truth for
anything not covered here.

- **Location / naming:** `docs/adr/NNNN-kebab-slug.md`; the title inside is
  `# ADR-NNNN: <Title>`.
- **Status:** `Accepted`, `Proposed` (agreed but not built yet, or still open),
  `Accepted (overrides plan §X)` when the record overrides the plan, and
  `Superseded by ADR-NNNN` when a later record reverses one.
- **Append-only:** a decision is revised by writing a new record that supersedes
  it, never by editing the old one's meaning.
- **Adding one:** next free number, add a row here, keep it to
  Context → Decision → Consequences → Alternatives considered → References.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions in `docs/adr/` | Accepted |
| [0002](0002-product-name-and-identifiers.md) | Product name and identifiers | Accepted |
| [0003](0003-v1-scope-and-non-goals.md) | v1 scope and non-goals | Accepted |
| [0004](0004-validated-hardware-and-compatibility-claims.md) | Validated hardware and the compatibility-claim policy | Accepted |
| [0005](0005-license-and-toolchain.md) | MIT license and the Rust 1.89 toolchain | Accepted |
| [0006](0006-workspace-layout-and-framework-free-boundary.md) | Twelve-crate workspace and the framework-free boundary | Accepted |
| [0007](0007-dioxus-webview-with-renderer-agnostic-boundary.md) | Dioxus webview with a renderer-agnostic boundary (and safe mode) | Accepted |
| [0008](0008-storage-split-sqlite-and-files.md) | Storage split: SQLite for data, files for shipped/hand-edited config | Accepted |
| [0009](0009-secret-handling-steam-api-key.md) | Secret handling: the Steam Web API key | Accepted |
| [0010](0010-single-cli-driver-for-store-backends.md) | One CLI driver for every store backend | Accepted |
| [0011](0011-exit-zero-is-not-success.md) | Exit code 0 is not success | Accepted |
| [0012](0012-gog-uses-two-clis.md) | GOG is driven by two CLIs (`lgogdownloader` + `gogdl`) | Accepted (overrides plan §4) |
| [0013](0013-read-only-installed-only-importers.md) | Read-only, installed-only importers; Lutris via `pga.db` | Accepted (overrides plan §4) |
| [0014](0014-library-sync-never-deletes-rows.md) | A failed listing never deletes rows | Accepted |
| [0015](0015-non-steam-prefixes-disk-and-install-queue.md) | Non-Steam prefixes, disk pre-check and the serial install queue | Accepted |
| [0016](0016-in-app-sign-in-terminal-relay.md) | The in-app sign-in terminal is one shared relay | Accepted |
| [0017](0017-argv-only-launch-composition.md) | Argv-only launch composition and the shim profile | Accepted |
| [0018](0018-steam-config-write-guardrails.md) | Steam config write guard rails | Accepted |
| [0019](0019-process-supervision-and-playtime.md) | Process supervision and playtime accounting | Accepted |
| [0020](0020-three-handoff-shapes.md) | Three launch hand-off shapes: Umu / native / store client | Accepted |
| [0021](0021-controller-input-gilrs.md) | Controller input is `gilrs`, not the webview Gamepad API | Accepted |
| [0022](0022-console-shell.md) | The console shell | Accepted |
| [0023](0023-graphics-injection-tiers-and-slot.md) | Graphics injection tiers and the single injection slot | Accepted |
| [0024](0024-run-upstream-installers-never-redistribute.md) | Run upstream installers; never redistribute artifacts | Accepted |
| [0025](0025-compat-db-and-anticheat-are-read-only-data.md) | Compat DB and anti-cheat are read-only curated data | Accepted + Proposed |
| [0026](0026-open-questions.md) | Open questions | Proposed |
| [0027](0027-app-shell-session-model.md) | The app shell session model and the screen seam | Accepted |
| [0028](0028-design-tokens-and-legibility-floor.md) | Design tokens and the legibility floor | Accepted |
| [0029](0029-one-in-app-terminal.md) | One in-app terminal, generalized from the sign-in panel | Accepted |

## See also

- The plan: `/Users/julienpohie/.opencode/plan/pitagames.md` (§1 decisions,
  §12 risks, §13 open questions).
- `AGENTS.md` — the non-negotiable design constraints and the build/verify notes.
- `docs/m*-*.md` — the per-slice implementation docs an ADR cites for detail.
