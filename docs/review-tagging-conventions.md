# Review-tagging conventions

How parity / source-review findings are referenced from the code, and how
the review documents that define those findings are named. The goal is one
rule: **every finding-ID tag in the source resolves to an in-tree document,
and that link cannot silently rot.**

## Finding-ID tags in source

When a fix or guard exists because a review found a defect, tag it with the
review's **series-prefixed finding ID** so a future reader can grep from the
code to the rationale.

- Always include the series prefix. Use `PVXS-SR-13`, never bare `SR-13`;
  `R27` round tags and the parity codes `H-N` / `CR-N` / `PF-N` likewise keep
  their series letter. A single grep for the canonical tag must find every
  site (see `style(epics-pva-rs): normalize bare SR-21 tags`).
- Put the rationale in a plain comment (`//`) or a module doc (`//!`).
  Reserve `///` item doc-comments for API semantics — do not bury internal
  review IDs in the public-facing doc of a `pub` item, where `cargo doc`
  surfaces them to downstream users.
- The tag is a pointer, not the explanation. Keep enough prose at the site to
  stand on its own; the legend holds the full finding.

## Where the legends live

Each ID series resolves to a dated review document under `docs/`:

| Series              | Legend document                               |
|---------------------|-----------------------------------------------|
| `PVXS-SR-N`         | `~/Documents/pvxs-source-review-2026-05-22.md` (archived) |
| `BFR-N`             | `docs/epics-ca-pva-broad-review-2026-05-22.md` |
| parity `H/CR/PF-N`  | `crates/<crate>/doc/parity-review/` (+ index) |

In-crate review sets (the `parity-review/` index + detail files) live under
that crate's `doc/` directory, not at the crate root.

## Review documents are dated, immutable snapshots

Naming a review `…-<YYYY-MM-DD>.md` is the convention and it is correct: a
review captures the codebase at one point in time, so it is append-only — you
do not edit an old review, you write a new dated one for the next round.

This is safe **only if the finding-ID namespace is stable across dates**:

- An ID is globally unique through its series prefix, so `PVXS-SR-13` always
  means the same finding regardless of how many dated `pvxs-source-review-*`
  files exist.
- Never reuse a number within a series. A later round adds `PVXS-SR-29`, it
  does not redefine `PVXS-SR-13`. If a finding is superseded, say so in the
  newer document; keep the old number meaning what it always meant.

## The link cannot rot

`crates/epics-base-rs/tests/doc_refs.rs` asserts that every
`parity-review/…​.md` path mentioned in that crate's source resolves to a real
file. A doc move that forgets to update a reference fails this test instead of
leaving a dead path behind. Any crate that starts referencing review docs by
path from its source should add the same guard.
