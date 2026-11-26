# M2 — the in-app sign-in terminal (`stores::auth`, `app::auth`)

Status: implemented. The relay is verified against fake CLIs on macOS and in
`archlinux:latest`; the GUI is built, linted, tested and smoke-run under Xvfb in
the container, where a fake `legendary` signed in from the Stores screen — and a
second run was cancelled from the same panel. No real `legendary` / `nile` /
`lgogdownloader` has been driven yet — none is installed on either target
machine.

## Why this slice exists

Every store backend could already sign in (`pitagames-cli <store> auth`), and
the Stores screen said so — with the command to copy into a terminal. That is a
dead end for the person the launcher is for: a signed-out store could never be
signed in from the app, and the row's "sign in required" badge was the last
thing that still needed a shell.

The three flows are also *almost* identical, and the CLI had them written out
three times: legendary and `lgogdownloader` ask for one pasted line at a
`:`-terminated prompt, nile adds a "press ENTER so I can open the browser" step
first, and all three are judged the same way (a refusal can log an `ERROR` and
still exit `0`). Three copies is where prompt policy drifts.

```text
 Stores row ──Sign in──► auth::spawn ──► stores::auth::AuthRun ──► the store's CLI
                              │                   │
                              │                   ├─ Line / Prompt / Continued
                              ▼                   │
                    AuthTerminal (lines, hint)  ◄─┘
                              ▲
        the input row ──AuthInputs──► writer thread ──answer/cancel──► the PTY
```

## `stores::auth` — the relay, shared by the CLI and the GUI

| Item | Role |
|---|---|
| `has_sign_in(store)` | Epic, Amazon and GOG sign in through a CLI; Steam and Local do not |
| `spec_for(store)` | the default `CliSpec` for that store's sign-in (`None` when it has none) |
| `hint(store)` | the one sentence that says what to paste |
| `AuthStep` | `Line(text)`, `Prompt { text, hint }`, `Continued(text)` |
| `AuthRun` | a running sign-in: `next`, `answer`, `cancel`, `finish`, `log_path` |
| `AuthOutcome` | the finished run's `RunReport` plus `succeeded`/`cancelled`/`failure_message` |
| `AuthError` | `Unsupported(store)` or a `CliError` from the spawn |

**The prompt policy lives here, once.** `AuthRun::next` classifies each line:
a "press ENTER" line is answered by the relay itself and reported as
`Continued` (the CLI is about to open the page; nothing has to be typed), a line
ending in `:` becomes `Prompt` with the store's hint, and everything else is a
`Line`. The callers only render and, for a `Prompt`, supply one line.

**The verdict is `RunReport::succeeded()`** with the store's sign-in CLI's log
format (`legendary`/`nile`, and `lgogdownloader` through `legendary`'s
bracketed layout), because every one of these CLIs can log an error and still
exit `0` — a sign-in that stored nothing must not read as a success.
`AuthOutcome::cancelled()` reports a stopped flow separately from a failure.

`AuthRun::attach(store, handle)` takes a handle the caller spawned itself, which
is how `pitagames-cli --program <path>` keeps its override; `AuthRun::start`
uses the default spec.

## The driver change that made this possible

An interactive flow is two threads on one CLI: one reads the events, another
types an answer (or cancels). The driver's poll loop used to hold its state lock
across the blocking wait for output, so the second thread could only take the
lock in the microseconds between two polls. In the container that starved
`cancel()` for ~12 seconds, and the panel's Cancel button looked dead.

`CliHandle` now has two locks: the state (`inner`) and the reader's channel
(`inbox`). `pump` takes a buffered event or waits on the inbox — never both —
and `CliInner::step` only folds one poll's input in while the state lock is
held. `next_event`, `wait` and the new `drain(&self)` (a non-consuming wait, so
an interactive caller can keep the handle across threads) are all built on
`pump`. A regression test (`a_second_thread_is_not_starved_by_the_poll_loop`)
measures the lock latency while a reader is blocked on a silent child.

## The CLI

`relay_auth` in `crates/cli` is now the only implementation of the `stdin` side:
print the lines, print the hint at a prompt, read one line, `run.cancel()` on a
closed `stdin`. `pitagames-cli epic|amazon|gog auth` are three one-line wrappers
over `AuthRun::attach`. That deleted ~150 lines of duplicated prompt handling —
and with it the risk that the terminal and the CLI answer different prompts.

## `app::terminal` — one terminal for every interactive flow

The sign-in model was generalized into `app::terminal` (ADR-0029), so the M3/M3.5
mod installers can render in the same bottom sheet instead of copying it.

| Item | Role |
|---|---|
| `TerminalEvent` | `Line`, `Prompt`, `Finished` — one report from the worker, tagged with the run id |
| `Terminal` | what the panel renders: lines, the current hint, the status, the transcript path, and the caller's `title` |
| `TerminalStatus` | `Running`, `Succeeded`, `Failed(message)`, `Cancelled` |
| `TerminalInputs` | the non-blocking handle the input row types through (`answer`, `cancel`) |
| `TerminalDriver` | the seam: `next_step`, `answer`, `cancel`, `finish` — what the terminal drives |
| `terminal::spawn(run, name, start, events)` | the reader thread, plus the writer thread that applies typed lines, over one `Arc<dyn TerminalDriver>` |
| `TerminalChannel` | the app's `sender()` + `async next()` pair, like the install and play channels |
| `next_run()` | the id that makes a late report from a closed panel harmless |

`Terminal::apply` folds a report in and returns whether the run completed
**successfully** — the caller's cue to act on a win. A report for another run id is
ignored, so closing a panel and opening a new one cannot be coloured by the old
one's verdict.

`terminal::spawn` runs the caller's `start()` **inside the worker thread**: a slow
or failing spawn becomes a failed run rather than a blocked click, and the thread
owns the driver while the window keeps only the inputs. Dropping the inputs
cancels the run — the same cancel-on-close the panel relies on.

## `app::auth` — the store sign-in, on the shared terminal

`app::auth` is now the thin adapter from a store's sign-in CLI to the shared
terminal: an `AuthDriver` (`Arc`-shared across the reader and writer threads) maps
`stores::auth::AuthStep` → `TerminalStep` and `AuthOutcome` → `TerminalOutcome`,
and `auth::spawn(run, store, options, events)` starts it with the store's own
thread name (`pitagames-auth-<store>`). The relay's prompt policy and verdict are
untouched, so the panel behaves exactly as before.

The writer also cancels when the window drops the inputs: leaving the Stores
screen with a sign-in open stops the CLI instead of leaving it waiting for an
answer nobody can type. `Terminal::apply`'s success return is what re-probes the
row — true exactly when a session was stored.

## The GUI

**The row.** A store whose sign-in CLI is on `PATH` (the first tool: `legendary`,
`nile`, `lgogdownloader` — never `gogdl`) gets a **Sign in** button. While that
store's run is live the button reads `signing in…` and is disabled. A signed-out
row still shows the `pitagames-cli <store> auth` command with its Copy button, as
the headless fallback.

**The panel.** A modal over the Stores screen: the store's name, a status badge,
Cancel/Close, a monospace output pane (newest line pinned at the bottom, so it
follows the CLI), the hint when a value is wanted, a one-line input with Send,
and the transcript path. Enter submits; the panel is the only place a sign-in is
typed.

**On success** the row is re-probed, so the badge, the account label and the game
counts catch up without a click; the panel stays open with a `signed in` badge
and a Close button, so the transcript can be read.

## Verification

- `stores::auth` (7 tests): a store without a sign-in is refused by name; a
  missing program is a `CliError`; a code-paste prompt asks for a line and the
  run succeeds; a refusal that logs an error and exits `0` is not a success; a
  "press ENTER" notice is answered by the relay and does not ask for a line;
  cancel ends the run and is reported as cancelled; every store gets its own
  prompt copy. `stores` 251 → 258.
- `stores::cli` (31 tests, +2): a second thread can answer a prompt while the
  reader waits, and is not starved by the poll loop. The whole `cli::` module
  went from 12.5 s to 5.5 s in the container — the starvation was real.
- `pitagames-cli` 53 tests unchanged: the three sign-in commands were rewritten
  onto the relay without changing what they print or accept.
- `crates/app` (`terminal` 10 + `auth` 2; 270 total): the generalized terminal —
  lines and a prompt land in order; a successful run reports success; a failure
  keeps the driver's message and does not; a cancelled run says so; a run that
  never started reports the error; a late report from a closed panel is ignored;
  a chatty process cannot grow the terminal without bound — plus a **scripted
  test-double `TerminalDriver`** driving the same `spawn`: it streams an answer
  and finishes, an explicit cancel reports cancelled, and dropping the inputs
  cancels the run. The `auth` adapter keeps the end-to-end fake-`legendary` test
  (prompt, typed answer, `Authentication successful.` → `Succeeded`) and adds one
  for a missing program.
- Xvfb smoke run in the container with a fake `legendary` on `PATH`:
  the Epic row showed **sign in required** and a **Sign in** button; clicking it
  opened the panel, which streamed the fake's URL line and its prompt; typing
  `123456` and pressing Enter produced `[cli] INFO: Authentication successful.`,
  the badge flipped to **signed in**, the transcript path appeared, and the row
  behind went to **ready · signed in as player · 2 owned · 1 installed** (the
  re-probe). A second run was cancelled from the panel: the badge read
  **cancelled**, the input row disappeared and the button became Close.

## Known limits

- **Not driven against a real store CLI.** The prompt shapes come from the
  backends' own docs and fixtures; the first machine with a real account is the
  closing check. In particular nile's ENTER step and `lgogdownloader`'s
  browser-login wording are taken from the CLIs' sources, not observed.
- **The PTY echoes what is typed**, so the pasted code or URL appears in the
  panel and in the transcript under `<data>/pitagames/logs/`. That is the CLI's
  own behaviour (it also prints it in a terminal); it is local diagnostics only,
  never written to SQLite or the library. Redaction is M4.
- **The terminal is a stream + one line + Cancel.** It renders lines and takes one
  line of input; it is not a general PTY terminal (no cursor control, no history,
  no ANSI layout). The model is now generic (`app::terminal`, ADR-0029) and the
  HelixSR/DLSSNR installers are the next caller, but nothing else drives it yet.
- **Closing the panel stops the flow.** There is no "leave it running"; the
  input handle is what keeps the CLI alive, so closing (or leaving the Stores
  screen) cancels it. Nothing is left half-signed-in: the CLI either stored a
  session before the signal or it did not.
- **No "open the page" button.** The store's CLI opens (or prints) its own
  sign-in page; the terminal only shows what it said, and the text is
  selectable. If a CLI stops opening the browser, the URL is still on screen.
- **One sign-in at a time.** The panel is a single slot in the Stores screen;
  starting a second store's sign-in replaces the first (the first's inputs drop,
  which cancels it).
- **The URL/code is not validated before it is sent.** The relay passes the
  typed line through; a wrong code is the CLI's error to report, and the panel
  shows it verbatim.

## Deliberately not in this slice

- A general in-app PTY terminal for the mod installers (M3/M3.5) and the
  `pitagames-cli` auth flows' interactive parts.
- Redacting the pasted value from the transcript (M4 hardening).
- Sign-out from the GUI (`legendary auth --delete`, `nile auth --logout`); GOG
  has no logout command at all.
- The cancellable install: a running install still has no channel into the
  worker that owns its `CliHandle`. The driver side now supports it (the same
  two-lock fix), so the next slice is the channel.
