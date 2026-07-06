# ADR 0017 · Non-fast-forward git pull via content-aware merge

- **Status:** accepted
- **Date:** 2026-07-05
- **Source:** [`docs/superpowers/specs/2026-07-05-nonff-pull-content-merge-design.md`](../superpowers/specs/2026-07-05-nonff-pull-content-merge-design.md)

## Context

`GitStore::pull` was fast-forward-only, erroring on divergence
(`"non-fast-forward pull requires manual merge"`), so replication could not
reconcile a local branch that had diverged from its remote. git already retains
history, so the **merge base commit** is a real common ancestor — 3-way merge
needs no separate ancestry mechanism (unlike sync, ADR 0016). The open questions
were how record content is merged (git's line-based merge vs gonzalo's
class-aware `merge()`) and what happens to records that cannot be auto-merged.

## Decision

We will make non-fast-forward `pull` perform a **content-aware 3-way merge**:

- Diff the merge base against local and remote; a record changed on only one
  side takes that side, and a record changed on **both** sides is reconciled with
  gonzalo's `merge(kind.merge_class(), base, local, remote)` — never git's
  line-based content merge (which mis-handles structured JSON records).
- Auto-merged records are rebuilt with a fresh revision and committed in a
  **two-parent merge commit** that advances the branch; unresolved
  (`NeedsResolution`) records **keep local and are surfaced** in a new
  `PullReport { fast_forwarded, merged, conflicts }` rather than erroring — the
  pull makes progress and reports what it could not reconcile, mirroring `sync`.

`pull` returns `PullReport` (no existing callers). Fast-forward and up-to-date
paths are preserved. Scope is merge (not rebase); `push` and the `Store` surface
are unchanged.

## Consequences

- **Positive:** diverged git peers reconcile automatically with record-correct
  semantics (append-only union, structured field-merge, etc.); a single
  unmergeable record no longer blocks pulling everything else; conflicts are
  reported with both sides for resolution.
- **Negative:** conflicted records keep local until the caller resolves them
  (the merge commit records a partial reconciliation); no rebase option; the git
  tree-walking/merge logic adds complexity to `gonzalo-store-git`.
- **Revisit if:** callers need rebase semantics, automatic conflict escalation,
  or conflict-marker artifacts for manual resolution.
