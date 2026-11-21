# ADR-0004: Validated hardware and the compatibility-claim policy

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** plan §1 (Reference hardware), §9 (Hardware validation matrix); `AGENTS.md` (Compatibility claims)

## Context

The graphics features (FSR 4, FrameGen, HelixSR, DLSSNR-AMD) depend heavily on
GPU architecture, driver version and game. Claiming support the project cannot
test is the fastest way to lose user trust, and there is no hardware budget to
certify a broad matrix.

## Decision

Certify exactly two in-house machines, and mark everything else:

- **RX 9070 (RDNA 4, 16 GB)** and **RX 5700 XT (RDNA 1, 8 GB)** are the only
  validated paths.
- Every other architecture — RDNA 3, RDNA 2, NVIDIA, Intel — is modeled but
  `experimental`/`unverified` in the compat DB, and the UI must say "not tested
  by us" rather than implying parity. Do not offer one architecture's package to
  another (notably: never give an RDNA 3 card the RDNA 4 DLSSNR package).
- The RDNA 3 DLSSNR fork ships behind the same backend abstraction, marked
  `experimental`, with its own version pin; "fork not updated" is *unsupported*,
  not broken.
- No TV is attached to either machine, so 10-foot readability, overscan, focus
  return and the gamescope session ship marked **"not tested on a TV"** until a
  community tester runs the checklist. Monitors are proxies only.
- The capability matrix drives the UI: an unsupported option is **hidden** (not
  greyed) with a one-line "why", sourced from the same table shown in the docs.
- No parity claims in docs or store copy.

## Consequences

- Positive: every "supported" claim corresponds to hardware the project can
  actually exercise; unsupported paths degrade predictably.
- Positive: the RDNA 3 fork can ship without pretending it is validated.
- Negative: valuable features are hidden from capable-but-untested hardware until
  a tester confirms them; the compat DB carries the burden of describing why.
- Even the "primary" RDNA 1 HelixSR path needs a smoke test first — upstream
  tested a BC-250 (gfx1013), not the 5700 XT (gfx1010).

## Alternatives considered

- **Model every architecture with equal confidence.** Rejected — untestable and
  dishonest.
- **Grey out unsupported options.** Rejected — a greyed option invites "why can't
  I?"; hiding with a reason is clearer on a TV.

## References

- plan §1 *Reference hardware for v1*, §9 *Hardware validation matrix*.
- `AGENTS.md` — *Compatibility claims*.
- `compat/*.toml`, `docs/m1-compat-db.md` — where the `experimental` markers live.
