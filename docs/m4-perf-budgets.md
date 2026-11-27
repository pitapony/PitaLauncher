# M4 — performance budgets and the measurement script

Status: the budget model (`crates/core/src/perf.rs`), the diagnostic
(`pitagames-cli perf check`) and the harness (`scripts/perf-smoke.sh`) are
implemented and unit-tested on macOS. The **container run is wired into CI** and
asserts resident memory; cold start and frame time are recorded there and
asserted on hardware. **Not yet run on the RX 9070 or the RX 5700 XT** — no
target machine has been reachable — so the hardware column below is empty and
marked *not measured* (ADR-0004).

## Why this slice exists

The plan makes three numbers acceptance criteria for the launcher, and M4's job
is to turn that line into something CI and a tester can re-run:

> **Performance budgets as acceptance criteria**: cold start to first paint < 2 s
> with a 500-game library, 60 fps grid scroll, and a documented resident-memory
> ceiling with the webview (~300 MB); measured by a script in the container and
> on hardware.

The plan also records why the ceiling is worth watching at all:

> WebKit process model costs ~300 MB while a game runs — hide the window and
> pause animation/input during play; document the ceiling; the renderer boundary
> keeps a Blitz migration viable if it hurts on the 8 GB machine.

Two rules shape the slice:

- **One definition of "pass".** The budget constants, the measurement format and
  the verdict live in `crates/core`, and the CLI judges them. The harness only
  *collects* numbers, so the container and a target machine cannot disagree
  about what a number means.
- **Only assert what the container can see.** A software renderer under Xvfb
  cannot show 60 fps and its cold start is not the machine's, so CI enforces the
  one number it can trust — resident memory — and records the other two. A
  hardware run enforces all three.

## The budgets

| Metric | Budget | Where it comes from |
|---|---|---|
| `cold-start` | < **2000 ms** to the first painted library, with **500** games | plan §7 exit criteria; user story 52 |
| `frame-time` | < **16.7 ms** (60 fps), 95th percentile of the scroll steps | plan §7 |
| `memory` | < **300 MiB** resident, the whole process group | plan §9 risk row |

`cold-start` is measured to the **first painted library**, not the shell's first
frame: the Steam scan and the SQLite read sit behind it, so the 500-game
qualifier is real. `frame-time` is the wall time between the scroll steps the
launcher actually processes, at the 95th percentile — a scroll that keeps up
with a 60 Hz input rate passes, one that falls behind does not.

## The model — `crates/core/src/perf.rs`

Framework-free and unit-tested on the host (the GUI cannot build on macOS).

| Item | Role |
|---|---|
| `COLD_START_BUDGET_MS` / `FRAME_TIME_BUDGET_MS` / `MEMORY_CEILING_BYTES` / `LIBRARY_GAMES` | the four constants, one place |
| `Metric` | the closed set (`ColdStart`, `FrameTime`, `Memory`), its name, its unit |
| `FrameStats::from_samples` | the percentile summary (linear interpolation, non-finite samples dropped) |
| `Measurement` | the harness JSON: `games`, `cold_start_ms`, `frame_ms`, `memory_bytes` |
| `Budget` / `Verdict` / `Violation` | the pure verdict: `Pass` / `Fail` / `Incomplete`, only over the metrics a run requires |
| `PerfRecorder` | the launcher's half — cold start and scroll frames — with an injected `Instant`, so the rules are tested without sleeping |

The **`Incomplete`** verdict is what lets CI enforce a subset: a required metric
that was not measured fails the run, an unrequired one is recorded and printed
as `recorded`.

## The diagnostic — `pitagames-cli perf check`

```
pitagames-cli perf check [--file <path>] [--require <metrics>] [--memory <bytes>] [--json]
```

- `--file` reads the measurement (default: stdin); `--json` prints the report.
- `--require cold-start,frame-time,memory` chooses the metrics that must be
  inside budget; the rest are recorded. The default is all three.
- `--memory 300MiB` injects the harness's resident-memory figure, because the
  launcher cannot see the webview's other processes. It takes plain bytes or a
  `MiB`/`GiB` suffix (the same parser as `pitagames-cli disk`).

The exit is non-zero when an enforced metric is over budget or a required one
was not measured. The human report prints one line per metric (`PASS` / `FAIL` /
`recorded` / `MISSING` / `skipped`) and a verdict line.

## The harness — `scripts/perf-smoke.sh`

```
scripts/perf-smoke.sh [--require <metrics>] [--games <n>] [--display <:N>] [--target <dir>] [--json] [--keep]
```

What it does, in order:

1. **Seeds a library.** Writes `--games` (default 500) `appmanifest_*.acf`
   files into a throwaway Steam root and points `PITAGAMES_STEAM_ROOT` at it, so
   the scan has real work and no machine state is touched. `XDG_DATA_HOME` and
   `XDG_CONFIG_HOME` are throwaway too.
2. **Finds a display.** Uses `--display`, an existing `$DISPLAY`, or starts
   `Xvfb :99` at 1920×1080.
3. **Launches under one session.** `setsid dbus-run-session -- pitagames`, with
   `PITAGAMES_PERF_FILE` set. `setsid` puts the launcher, the webview and
   WebKit's helper processes in one process group, so the memory sample and the
   teardown are both *the whole launcher and nothing of the script*.
4. **Waits for the first painted library** by polling the measurement file the
   launcher writes on first paint.
5. **Samples resident memory** every 200 ms and keeps the peak, summing the
   `rss` column of every process in the group. This is deliberately the whole
   group — the UI process, the WebKit web/network/GPU processes and the session
   bus — because the plan's "~300 MB with the webview" is about the group, not
   one process. Summing `rss` over-counts shared pages, so the number is a
   conservative ceiling.
6. **Drives a scroll burst** with `xdotool` (150 `Down` presses at a 60 Hz-ish
   rate) and lets the launcher record the interval between the steps it
   processes.
7. **Judges** by calling `pitagames-cli perf check --memory <bytes> --require …`
   and exits with its status.

The launcher's side is `crates/app/src/perf.rs`, a thin seam over
`PerfRecorder`: `main` installs it at process start, the library grid marks the
first paint, and the grid's scroll path marks each step. Everything is gated on
`PITAGAMES_PERF_FILE`, so a normal run pays one atomic load per call and writes
nothing.

## What CI asserts, and what it records

`scripts/perf-smoke.sh --require memory` runs in the `gui` job after the build.

| Metric | Container (CI) | Hardware |
|---|---|---|
| `cold-start` | recorded | asserted |
| `frame-time` | recorded | asserted |
| `memory` | **asserted** | asserted |

The container enforces only memory because the other two are not the machine's
numbers: Xvfb renders in software (no vsync to keep 60 fps against) and its cold
start measures the container's WebKit init, not a CachyOS desktop's. Enforcing
them there would make CI flaky without proving anything about the two target
machines. They are printed in the report so a regression is still visible, and a
hardware run passes `--require cold-start,frame-time,memory`.

## Recording on hardware

On the RX 9070 and the RX 5700 XT (CachyOS, a real session):

```sh
cargo build --release -p pitagames -p pitagames-cli
scripts/perf-smoke.sh --require cold-start,frame-time,memory \
    --target target/release --display "$DISPLAY" --keep
```

`--display "$DISPLAY"` uses the running session instead of Xvfb, and
`--keep` leaves the fixture root and the measurement JSON for the record. Run it
on a freshly booted desktop with no other launcher open, once per machine, and
paste the numbers here.

| Metric | Budget | RX 9070 | RX 5700 XT (8 GB) |
|---|---|---|---|
| `cold-start` (500 games) | < 2000 ms | *not measured* | *not measured* |
| `frame-time` (p95) | < 16.7 ms | *not measured* | *not measured* |
| `memory` (process group) | < 300 MiB | *not measured* | *not measured* |

Until a machine fills this in, the budgets are **container-verified, not
hardware-verified** (ADR-0004).

## The memory ceiling and the Blitz-migration escape hatch

The 300 MiB ceiling is not an accident of the code: it is WebKitGTK's process
model. The launcher is one UI process plus a WebKit web process, a network
process and (with DMABUF compositing) a GPU process, and the launcher already
hides its window and pauses input while a supervised game runs
(`docs/m2.5-handoff.md`) so the webview is not compositing on top of a game.

If the ceiling hurts — most plausibly on the **RX 5700 XT machine, which has
8 GB of RAM** and shares it with a game and a Wine prefix — the escape hatch is
already in the architecture, not something to retrofit:

- **Only `crates/app` may use Dioxus/RSX.** Every other crate — `core`, `db`,
  `stores`, `art`, `platform`, `mods`, `metadata`, `input`, `shim`, `cli` — is
  framework-free (ADR-0006, ADR-0007). The webview is one seam, at the very top.
- That seam is what keeps a migration to **Blitz** (`dioxus-native`, the
  Dioxus-native renderer that does not use WebKitGTK) viable: the domain,
  storage, stores, launch composition and input layers do not change, and
  `docs/m2.5-design.md`'s inline-style and no-`<style>`-element rules were
  chosen partly because they survive a renderer change.
- **No `<video>`** in the v1 UI (a hard constraint in `AGENTS.md`) is the same
  bet: it would bind the UI to GStreamer/WebKit and block Blitz.

So the documented response to a memory regression is: re-measure, and if the
group is over budget on the 8 GB machine, evaluate the Blitz migration rather
than shave the WebKit process model — the boundary that makes it a migration and
not a rewrite is a non-negotiable design constraint.

## Known limits

- **No hardware run yet.** The numbers above are container-only; ADR-0004 keeps
  them "not tested by us" until a target machine fills the table in.
- **`frame-time` is a proxy.** It is the interval between the scroll steps the
  launcher processes, not a compositor vsync interval. It answers "can the
  launcher keep up with a 60 Hz input rate", which is the user-visible question;
  it is not a frame-time histogram from the GPU.
- **Memory is a sum of RSS.** It over-counts shared pages and includes the
  session bus the launcher runs under in the container. Both make it a ceiling,
  not an exact figure.
- **The cold-start clock starts at the GUI child**, not at the supervisor's
  `main`, so the supervisor's spawn cost (a few milliseconds) is not counted.
- The harness needs a display; the container path needs `Xvfb`, `dbus`,
  `xdotool` and `procps`.
