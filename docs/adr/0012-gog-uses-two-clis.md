# ADR-0012: GOG is driven by two CLIs (overrides plan §4)

- **Status:** Accepted — **overrides** the plan's "prefer `lgogdownloader` for
  install/verify"
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §4 (GOG) and §12 (GOG requires Node for `gogdl`); `docs/m2-gog.md`

## Context

The plan assumed one GOG CLI, `lgogdownloader`, could cover the whole backend.
In practice neither CLI covers a launcher's needs: `lgogdownloader` lists owned
games with titles and owns the login, but only fetches installer files and has
**no launch command**; `gogdl` installs a launchable game directory and launches
it, but cannot list the library.

## Decision

Route each GOG command to the program that actually supports it, through
`Gog::spec_for(command)`:

- **`lgogdownloader`**: `--list json` (the only listing with titles),
  `--browser-login` (a browser URL + pasted-redirect flow) and
  `--check-login-status` (answers "signed in?" offline, exits `1` when signed
  out).
- **`gogdl`**: `download {id} --path {dir} --platform windows`, `repair`, and
  `launch {dir} {id} --platform windows` — plus the `--auth-config-path` file at
  `<config>/pitagames/gog/auth.json` (never Heroic's), passed *before* the
  subcommand because argparse treats it as a main-parser option.
- `--list json` is a `gameDetails` array (`product_id`/`gamename`/`title`) with
  DLCs nested under `dlcs[]`, so no DLC filter is needed; the id is the numeric
  `product_id`.
- **`gogdl` exits non-zero on failure**, so library/install/verify use
  `succeeded()` and `launch` uses `exited_cleanly()` (ADR-0011).
- There is no installed listing, so `verify`/`launch` require `--dir`; Windows
  platform only; no logout/sync.
- GOG's status row carries **two tools** and is ready only when both resolve.
  Node remains an optional dependency (for `gogdl`) in `optdepends`.

## Consequences

- Positive: each capability is implemented by the CLI that has it; the backend
  works instead of failing on a missing launch command.
- Positive: the plan's incorrect claim is recorded as an explicit override rather
  than silently patched.
- Negative: GOG has two dependencies and two version/status surfaces, and its
  `--auth-config-path` ordering is a documented footgun.

## Alternatives considered

- **`lgogdownloader` for install/verify (the plan's plan).** Rejected — it has no
  launch command and only downloads installer files.
- **`gogdl` for everything.** Rejected — it cannot list the library or own the
  login.
- **A native GOG client.** Deferred (ADR-0003).

## References

- `docs/m2-gog.md`; `crates/stores/src/gog.rs`.
- plan §4 (GOG bullet), §12 (Node optdepend).
- `docs/adr/0011`.
