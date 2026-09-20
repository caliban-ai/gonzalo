# ADR 0024 · Blob garbage collection, and tombstones pin their blobs

- **Status:** accepted
- **Date:** 2026-09-19
- **Amends:** [ADR 0012](0012-code-graph-two-level-keying.md), whose mark-sweep marked
  against manifests alone. [ADR 0021](0021-replicated-deletion-with-tombstones.md)
  named blob garbage collection as follow-up work; this is it.

## Context

Blobs are content-addressed and shared: two records with identical content hold
one blob, and nothing in a blob records who points at it. Reclaiming one
therefore means proving nobody does.

`gc_blobs` has existed since ADR 0012, but it marked from a caller-supplied list
of manifests — the only blob references that existed when code-graph slices
were the only blobs. Records whose *body* is a blob came later, and the CLI's
`gonzalo gc` still passed manifests only. On any store holding a blob-backed
record, a sweep deleted that record's content while the record still pointed at
it, leaving a dangling reference that no sync could repair. The narrow mark set
was a latent data-loss bug, not a design choice.

Deletion adds a second question. A tombstone's own body is empty (ADR 0021), so
once a blob-backed record is deleted nothing names its blob. Two answers were
open:

- **Free it with the delete.** Space comes back immediately. But a delete is
  replicated, not final: a peer that was offline still holds the live record and
  can sync it back, and resurrection is exactly what a tombstone's collection
  horizon is sized to allow for. Recreating the same content — the common case
  after an accidental delete — would also have to re-upload bytes the store had
  moments ago. Worst of all, the window where the record exists and its content
  does not is invisible: reads fail late, at fetch time, with no error at the
  delete.
- **Keep it until the tombstone goes.** The tombstone already encodes "this was
  deleted, and here is how long we are prepared to be wrong about it". Tying the
  blob's life to it costs the deleted bytes for exactly the horizon the operator
  already chose.

A third option — refcounting — was rejected for the reason ADR 0012 rejected it:
a drifted count either leaks forever or deletes live content, and neither
failure is self-correcting. Marking from the records is.

## Decision

**A tombstone pins its blob.** `tombstone_of` copies the deleted record's
`Body::Blob` hash into a new `Record::deleted_blob` field, and GC treats that
pin as a live reference. Recreating the key clears it, and `collect` releases it
by removing the tombstone. Reclaiming a deleted record's bytes is therefore
three deliberate steps, in order: `delete`, `collect` past the horizon, then GC.

**The mark set is built from the records, never from a caller.**
`live_blob_hashes` unions three sources over the store's **raw** records:

1. a record whose body is a `Body::Blob` — the bytes are its content;
2. a tombstone's `deleted_blob` — the pin above;
3. every slice a `graph-manifest` names (ADR 0012), referenced from a record's
   contents rather than its body.

`gc_blobs` now takes the store itself and does the listing. Marking against any
one source alone deletes the other two's content, and a caller holding a partial
view of liveness cannot know that. A `graph-manifest` whose body will not decode
is an error, not an empty reference set: silently treating it as referencing
nothing would sweep every slice it named.

GC stays explicit and operator-run, for the reason collection is: a blob swept
while a peer still holds the record naming it cannot be restored by a later
sync.

## Consequences

- **Positive:** a live record's content can no longer be swept out from under
  it, which was reachable from `gonzalo gc` on any store with a blob-backed
  record. Deletion is fully reversible for as long as the horizon says it is —
  syncing a record back from a peer, or re-putting the same content, both find
  the bytes still there. Liveness is derived, so a missed event leaves a blob
  briefly un-swept rather than wrongly deleted.
- **Negative:** deleting no longer reclaims space; an operator who expects it to
  must now also collect. Deleted-but-uncollected blobs hold space for the whole
  horizon, which on a store with a long horizon and large bodies is the dominant
  cost of this decision. A sweep reads every record in the store, so it is O(n)
  in records rather than in manifests — acceptable for an explicit, occasional
  operation, and the same shape `collect` already has.
- **Neutral:** `Record::deleted_blob` is `Option`, omitted when absent, so
  existing records and peers that never write it are unaffected. A peer running
  an older binary replicates tombstones without the pin; its own GC then frees
  the blob locally, which is the behaviour it had before. `sweep_blobs` remains
  available for a caller that has already computed a live set — with the mark
  set being the dangerous half, it is documented as such.
