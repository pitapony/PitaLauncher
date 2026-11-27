# Graphics validation — the per-path certification record

This is the human-readable certification record for the graphics paths PitaGames
ships, on the two in-house machines ADR-0004 allows us to validate. It mirrors
the plan §9 *Hardware validation matrix* and pairs every cell with the shipped
compat DB's machine-readable `verified` field.

The record is owned by `platform::validation` (`crates/platform/src/validation.rs`):
`ValidationPath::ALL` is the one row order this document and any future CLI
iterate, and `ValidationPath::table_row` builds the exact table row below. A
test in that module reads *this file* and asserts every row is present and that
no cell claims `verified` unless the shipped `compat/` database verifies that
machine's architecture — so the document and the `verified` field cannot drift.

The claims describe the **plan's** in-house coverage (ADR-0004). No physical
hardware run has been recorded yet; the `verified` claim is the machine-readable
curation record, and the runtime checklist that turns it into a signed-off pass
is the M0 / M3 hardware closer.

## The two in-house machines

- RX 9070 (RDNA 4) - verified — RDNA 4, 16 GB. The primary card: FSR 4 (native,
  upgrade and DLSS→FSR 4), MLFG, and DLSSNR-AMD neural rendering.
- RX 5700 XT (RDNA 1) - experimental — RDNA 1, 8 GB. No FSR 4 and no neural
  rendering; HelixSR is the intended upscaler, with OptiScaler FSR 3 frame
  generation and gamescope system FSR as fallbacks.

Both are in-house. Every other architecture — RDNA 3, RDNA 2, NVIDIA, Intel — is
modeled but **`experimental` / unverified**, and the UI must say "not tested by
us" rather than imply parity. No TV is attached to either machine, so the
10-foot and gamescope-session paths are **`unvalidated`** (there is no TV in
house; monitors are proxies only).

## The matrix (plan §9)

| Path | RX 9070 (RDNA 4) | RX 5700 XT (RDNA 1) |
|---|---|---|
| Steam / EGS / GOG / Amazon launch + shim env | verified | experimental (not tested by us) |
| `PROTON_FSR4_UPGRADE` (in-place FSR 4) | verified | n/a |
| DLSS to FSR 4 via OptiScaler (Tier A + B) | verified | n/a |
| FSR 4 to XeSS / FSR 3.1 fallbacks | verified | experimental (not tested by us) |
| Frame generation (MLFG / FSR 3 FG) | verified | experimental (not tested by us) |
| DLSS 5 neural rendering (DLSSNR-AMD upstream) | verified | not supported |
| HelixSR Scaler (HLX DLSS / HLX FSR 4) | experimental (not tested by us) | experimental (not tested by us) |
| HelixSR FSR 3.1 drop-in | experimental (not tested by us) | experimental (not tested by us) |
| System FSR (gamescope) | verified | experimental (not tested by us) |
| HDR / VRR / gamescope | verified | experimental (not tested by us) |
| Console UI (in-app, windowed to fullscreen) | verified | experimental (not tested by us) |
| Console 10-foot readability / safe area | unvalidated (not tested by us) | unvalidated (not tested by us) |
| Bootable gamescope session (v1.1) | unvalidated (not tested by us) | unvalidated (not tested by us) |
| Controller navigation (Xbox / DualSense / 8BitDo) | verified | experimental (not tested by us) |
| RDNA 3 NR fork, RDNA 2, NVIDIA, Intel | experimental (not tested by us) | experimental (not tested by us) |

## What each claim means

`Claim::cell()` is the one place the cell text comes from, so the words below are
the words the table shows.

| Claim | Cell text | Meaning |
|---|---|---|
| `Verified` | `verified` | The machine's architecture is listed in the shipped compat DB's `verified` field. |
| `Experimental` | `experimental (not tested by us)` | Offered with no in-house coverage. |
| `NotSupported` | `not supported` | The hardware cannot run this path. |
| `NotApplicable` | `n/a` | The path does not apply to this machine. |
| `Unvalidated` | `unvalidated (not tested by us)` | Not testable in house (no TV). |

Per ADR-0004 an untested path is either **hidden** (not greyed) in the UI with a
one-line "why", or, where it is offered at all, marked "not tested by us". A path
the card physically cannot run (`n/a`, `not supported`) is never offered.

## The machine-readable claim

The claim in each column is **not hand-written twice**. `Machine::db_claim`
derives a machine's own claim from the shipped compat database:

- The RX 9070 detects as `Arch::Rdna4`, and every curated `compat/*.toml` entry
  lists `verified = ["rdna4"]` — so the RX 9070 column is `verified`.
- The RX 5700 XT detects as `Arch::Rdna1`, and no curated entry lists `rdna1` in
  its `verified` field — so its column is `experimental (not tested by us)`.

The per-path cells then follow that machine claim, except where the plan fixes
the answer regardless of the DB: `n/a` for the FSR 4 paths on RDNA 1, `not
supported` for neural rendering on RDNA 1, `experimental` for the HelixSR paths
and for hardware we do not own, and `unvalidated` for the no-TV paths. The
"experimental marks" the ticket asks for therefore live in the compat DB's
`verified` field, and `docs/m1-compat-db.md` describes where those entries are
curated.

Where the plan's matrix writes **certify** in the RX 5700 XT column (store
launch, the FSR 4 fallbacks, FSR 3 frame generation, HelixSR, system FSR,
HDR/VRR, the console UI and controller navigation), this record holds the cell at
`experimental (not tested by us)` because the compat DB lists no `rdna1` entry in
a `verified` field yet — the RDNA 1 smoke test has not run. The mark is
deliberate, not an oversight: ADR-0004 says even the intended primary RDNA 1 path
needs its smoke test before we claim support, and the moment the database lists
`rdna1` for the curated entries the column flips on its own.

Two invariants tie the document to that field, both enforced by tests in
`platform::validation`:

1. Every `ValidationPath::ALL` row is present in this document, as built by
   `ValidationPath::table_row`.
2. No cell is `verified` unless the shipped compat DB verifies that machine's
   architecture — and the machine summary lines above are the DB-backed claim
   verbatim.

## Not tested by us

- **RDNA 3, RDNA 2, NVIDIA and Intel** ship `experimental`; the RDNA 3 DLSSNR
  fork is pinned separately and never cross-offered to an RDNA 4 card (or the
  reverse). See `docs/dlssnr.md` and `docs/m3-capability.md`.
- **HelixSR** is `experimental` on both cards: upstream tested a BC-250
  (`gfx1013`), not our RX 5700 XT (`gfx1010`), so even the intended primary RDNA 1
  path still needs its smoke test before we claim support (ADR-0004).
- **10-foot readability, overscan and the gamescope session** are `unvalidated`:
  no TV is attached to either machine. They ship marked "not tested on a TV"
  until a tester with a TV runs the checklist in `docs/console-mode.md`.
- **No physical run has been recorded yet** for the `verified` RX 9070 column;
  the machine-readable claim is the curation record, and the M0 / M3 hardware
  checklists are what turn it into a signed-off pass.

## Verification

The record and this document are checked together, on macOS and in CI:

```bash
cargo test -p pitagames-platform validation
cargo clippy -p pitagames-platform --all-targets -- -D warnings
cargo fmt --all -- --check
```

`platform::validation` covers: both machines are in-house and map to RDNA 4 /
RDNA 1; the machine claim follows the compat DB's `verified` field; no non-RDNA4
architecture is verified in `compat/`; no RX 5700 XT cell is ever `verified`;
every matrix path has a row in this file; and the doc claim traces to the
`verified` field with the "not tested by us" marker on every untested cell.

## References

- ADR-0004 — *Validated hardware and the compatibility-claim policy*.
- ADR-0025 — the compat DB's `verified` field is read-only curated data (see
  `docs/m1-compat-db.md`).
- plan §1 *Reference hardware for v1*, §9 *Hardware validation matrix*.
- `docs/m3-capability.md`, `docs/dlssnr.md`, `docs/console-mode.md`.
