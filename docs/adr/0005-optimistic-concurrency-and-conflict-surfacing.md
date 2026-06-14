# ADR 0005 · Optimistic concurrency with explicit conflict surfacing

- **Status:** accepted
- **Date:** 2026-06-13

## Context

Gonzalo's premise is that multiple systems and contributors share state.
Concurrent edits to the same record are therefore expected, and caliban's
durable memory must **never silently lose** a contributor's write. We need a
concurrency model that works identically for local writes and cross-replica
sync, across every substrate.

## Decision

Use **optimistic concurrency** with **explicit, typed conflict surfacing**:

- Writes are conditional: `put(record, expected_parent_rev)`. If the stored
  revision no longer matches the expected parent, the store returns
  `PutResult::Conflict` rather than overwriting.
- The core ships merge strategies keyed by `RecordKind`: append-only kinds
  (auto-memory topics, session transcripts) auto-merge by union/concatenation;
  structured kinds attempt a field-level 3-way merge against the base; anything
  ambiguous is surfaced to the caller and to `gonzalo-cli` for resolution.
- `Sync` (pull → detect divergence → merge → push) reuses this exact machinery,
  so reconciliation and local writes share one code path.
- `Conflict` is a recoverable result variant, **not** a generic error.

## Consequences

- **Positive:** Concurrent edits are never silently lost — the core invariant.
  One conflict/merge implementation serves both local writes and sync. Callers
  get a typed, recoverable outcome they must handle, not a stringly error.
- **Negative:** Every writer must handle a `Conflict` outcome — more caller
  complexity than last-write-wins. Concurrency is optimistic, so a
  high-contention record can see repeated retry/merge cycles.
- **Revisit if:** a workload needs last-write-wins or server-side locking, or
  the per-kind merge strategies prove insufficient for a real conflict pattern.
