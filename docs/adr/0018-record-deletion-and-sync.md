# ADR 0018 · Record deletion and its sync semantics

- **Status:** accepted
- **Date:** 2026-07-11

## Context

The `Store` trait had no way to remove a record — only `get`, `put`, and `list`.
The caliban integration ([#1](https://github.com/caliban-ai/gonzalo/issues/1))
needs deletion to prune memory topics, retire sessions, and sweep old
checkpoints, so `Store` must gain a `delete`. Two questions had to be settled:

1. **Concurrency.** `put` is OCC-aware (`expected: Option<Revision>`), surfacing
   a `Conflict` rather than silently clobbering a concurrent write (ADR 0005). A
   delete that ignored the current revision could remove a record a peer had just
   updated. Should delete carry the same precondition?
2. **Replication.** `sync` (ADR 0016) and git `pull` (ADR 0017) reconcile two
   substrates by copying each side's records into the other. A delete leaves *no*
   trace, so a later sync against a peer that still holds the record cannot tell
   "deleted here" apart from "never seen here" — and copies it back. Do we need
   tombstones to propagate deletes, or is a local delete enough for now?

## Decision

We will add `Store::delete(key, expected: Option<Revision>) -> DeleteResult`,
OCC-aware and mirroring `put`:

- `expected == None` removes the record if present and is an idempotent no-op if
  absent → `DeleteResult::Deleted`.
- `expected == Some(rev)` removes only if the current revision matches → `Deleted`;
  a mismatch leaves the record untouched and returns `DeleteResult::Conflict`
  holding the live record. An already-absent key is an idempotent `Deleted` — the
  named revision is already gone, so there is nothing to conflict on.
- The check-and-remove runs in the same critical section as the substrate's `put`
  (fs: the per-record flock; git: the repo lock plus a commit of the removal; s3:
  a conditional `DeleteObject` with `If-Match: <etag>`), so it is atomic against a
  concurrent writer. `DeleteResult::Conflict`, like a `put` conflict, is a normal
  recoverable result, not an error.

We will make delete **local-only**. It is *not* a tombstone and is *not*
propagated by `sync`: a later sync against a peer that still holds the record will
resurrect it. Full tombstone propagation (a deletion marker that survives and
replicates so a delete on one substrate erases the record everywhere) is
**deferred** until multi-substrate delete-sync is actually needed.

## Consequences

- **Positive:** callers get OCC-safe deletion across every substrate with the
  same conflict semantics they already know from `put`; the conformance suite
  exercises it on fs/git/s3/server. No new replication machinery, wire tombstone
  type, or GC of deletion markers to build and reason about yet.
- **Negative:** delete does not replicate — a record deleted on one substrate and
  then synced from a peer that still holds it comes back. Callers who need a
  delete to stick across a sync must delete on both sides (or not rely on sync).
  This is a real, documented sharp edge until tombstones land.
- **Revisit if:** a consumer needs a delete on one substrate to propagate to its
  peers — that is the trigger to design tombstone records, their replication in
  `sync`/`pull`, and their eventual garbage collection.
