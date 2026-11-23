# Domain Docs

How the engineering skills should consume this repo's domain documentation when
exploring the codebase.

## Before exploring, read these

- **`AGENTS.md`** at the repo root — this repo's single vocabulary source. Its
  "What this repo is (and isn't)" bullet and the "Non-negotiable design
  constraints" section define the domain language (module names, the store
  backends, the hand-off shapes, the console shell, the install queue, …). There
  is deliberately **no separate `GLOSSARY.md`**: a second glossary would drift
  from `AGENTS.md`, which agents already read on every session.
- **`docs/adr/`** — 27+ Architecture Decision Records. Read the ADRs that touch
  the area you're about to work in before proposing a change. `docs/adr/README.md`
  is the index.
- **`docs/m*-*.md`** — the per-slice implementation docs an ADR cites for detail
  (`docs/m2-store-sync.md`, `docs/m2.5-shell-model.md`, …). Read the one for the
  slice you're changing.

If any of these don't exist, **proceed silently**. Don't flag the absence; don't
suggest creating them upfront.

## Layout

Single-context repo (no `GLOSSARY-MAP.md` at the root):

```
/
├── AGENTS.md          ← vocabulary + the non-negotiable constraints
├── docs/adr/          ← one record per decided question
├── docs/m*-*.md       ← per-slice implementation notes
├── crates/            ← the twelve-crate workspace
└── docs/agents/       ← this file and the tracker/label config
```

## Use the repo's vocabulary

When your output names a domain concept — an issue title, a refactor proposal, a
hypothesis, a test name — use the term as `AGENTS.md` defines it. Don't drift to
synonyms it avoids, and don't invent new language for something already named
(e.g. it is the *hand-off*, the *store backend*, the *install queue*, the
*console overlay*).

If the concept you need isn't in `AGENTS.md` yet, that's a signal: either you're
inventing language the project doesn't use (reconsider) or there's a real gap
(note it for `/domain-modeling`, which adds the term to `AGENTS.md` and, when it
is a decision, an ADR).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than
silently overriding it:

> _Contradicts ADR-0022 (the console shell), but worth reopening because…_

`docs/adr/` is **append-only**: a decision is revised by a new record that
supersedes the old one, never by editing the old one's meaning.

## When a decision is worth recording

Offer an ADR only when all three hold: it is hard to reverse, it would surprise a
reader without context, and it was a real trade-off between alternatives. The
repo's records follow the MADR-lite shape in `docs/adr/0001` — Context → Decision
→ Consequences → Alternatives considered → References.
