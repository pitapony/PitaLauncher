# ADR-0017: Argv-only launch composition and the shim profile

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §5 (Steam shim); `AGENTS.md` (Launch composition is argv-only)

## Context

A game is launched by composing an environment, a wrapper chain (gamemode,
mangohud, gamescope, …) and a program with arguments. Doing that as a shell
string would be a quoting minefield and a security smell; doing the risky Steam
`localconfig.vdf` write on every settings toggle would multiply the risk.

## Decision

- **`LaunchSpec` is the single artifact** shared by the GUI, the CLI and the
  shim: `{ env, wrappers: Vec<WrapperStep>, program, args, cwd, prefix,
  log_path }`. Everything else is a producer or a consumer of it.
- **Argv-only, never a shell:** no `sh -c`, no string interpolation. Wrapper
  steps are typed `program + args` vectors; `core::launch` flattens, quotes and renders
  them for display only. `render_shell_command` exists to *show* the command, not
  to run it.
- **Steam games launch through the shim:** `pitagames-shim <appid> %command%` in
  Steam launch options, set once. The shim reads
  `<data>/pitagames/profiles/steam-<appid>.json` (`ShimProfile`: ordered env
  additions + wrapper steps + the recorded Proton choice, written atomically on
  every profile save), builds env, flattens the wrapper chain in front of the
  game, logs the argv + env, then `exec`s, forwarding the exit code and signals.
- A missing/corrupt profile **keeps the M0 environment and logs a warning; it
  never fails the launch.** `--explain <appid> [command]` prints the resolved
  profile, Proton, env and a copyable command.
- **Why a shim:** profile edits apply instantly, and the risky
  `localconfig.vdf` write happens once per game instead of on every toggle.

## Consequences

- Positive: the same composed command is what the GUI previews, what the CLI
  prints and what the shim runs — they cannot drift.
- Positive: no shell means no quoting/injection classes of bug.
- Negative: anything that wants "just run this shell line" must be expressed as
  argv; the shim adds one process to a Steam launch.
- Non-Steam launches share the same composition via `platform::launch`
  (ADR-0020).

## Alternatives considered

- **A shell string / `sh -c`.** Rejected — quoting bugs and injection; also
  breaks the "GUI preview equals the real command" guarantee.
- **Write `localconfig.vdf` on every toggle.** Rejected — multiplies exposure to
  the highest-risk code path (ADR-0018).
- **No shim (env compiled at launch only).** Rejected — Steam starts its own
  games, so a per-game hook is required for Steam titles.

## References

- plan §5 *Steam shim*; `AGENTS.md` — *Launch composition is argv-only*.
- `docs/m1-profile.md`, `docs/m1-wrappers.md`; `crates/core/src/launch.rs`,
  `crates/shim`.
- `docs/adr/0018`, `docs/adr/0020`.
