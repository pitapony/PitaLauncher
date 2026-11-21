# ADR-0001: Record architecture decisions in `docs/adr/`

- **Status:** Accepted
- **Date:** 2026-11-20
- **Deciders:** julienpohie
- **Source:** this decision; `AGENTS.md` (documentation discipline); plan §1, §13

## Context

PitaGames' decisions live in three places that all say the same things
differently: the plan (a 1,400-line single file with a *Decisions already made*
table and a *Risks & guardrails* table), `AGENTS.md` (a dense prose digest of the
non-negotiable constraints), and ~40 slice docs under `docs/`. When the
implementation diverged from the plan — GOG turned out to need two CLIs; Lutris
stores its library in `pga.db`, not `*.yml` — the correction was visible only as
a milestone bullet, and nothing gave those decisions a stable identifier to cite
from code, commits or a review.

## Decision

Keep a numbered set of Architecture Decision Records under `docs/adr/`, one
decision per record, in MADR-lite form:

- Filename `NNNN-kebab-slug.md`; the title inside is `# ADR-NNNN: <Title>`.
- Fixed sections: `Status`, `Date`, `Deciders`, `Source` front matter, then
  Context → Decision → Consequences → Alternatives considered → References.
- `docs/adr/README.md` is the single index (number, title, status).
- Status values: `Accepted`, `Proposed`, `Accepted (overrides plan §X)`,
  `Superseded by ADR-NNNN`.
- A record states the **decision and its rationale**; it links to the slice doc
  that holds the implementation detail instead of restating it.
- The set is append-only: a decision is revised by a new record that supersedes
  the old one, never by rewriting the old one's meaning.

## Consequences

- Positive: one citable place for "why is it like this", with a stable number to
  reference from code, commits and reviews.
- Positive: divergences from the plan become explicit (`overrides plan §X`)
  rather than an archaeology exercise.
- Negative: one more doc set to keep current as M3/M4 land; a stale record is
  worse than none, so each slice that changes a decision must add or supersede a
  record.
- The ADRs deliberately do not duplicate the plan or the slice docs, so there is
  no third copy of the implementation to drift.

## Alternatives considered

- **A single `DECISIONS.md`.** Rejected — concurrent edits conflict, and decisions
  cannot be cited by number.
- **Keep decisions in the plan only.** Rejected — the plan is a monolith and its
  tables have no per-decision identity or status.
- **A full RFC process with review periods.** Rejected — disproportionate for a
  one-developer project; MADR-lite is the right weight.

## References

- `docs/adr/README.md` — the index and the format rules.
- `/Users/julienpohie/.opencode/plan/pitagames.md` — the source of truth for
  anything the ADRs do not cover.
- `AGENTS.md` — the non-negotiable constraints several ADRs record.
