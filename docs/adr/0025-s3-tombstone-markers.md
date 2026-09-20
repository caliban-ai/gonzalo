# ADR 0025 · Sibling tombstone markers in the S3 layout

- **Status:** accepted
- **Date:** 2026-09-19
- **Source:** [`docs/superpowers/specs/2026-09-19-s3-tombstone-marker-layout-design.md`](../superpowers/specs/2026-09-19-s3-tombstone-marker-layout-design.md)
- **Amends:** [ADR 0021](0021-replicated-deletion-with-tombstones.md), which put
  the tombstone at the deleted record's own key and named the resulting S3
  listing cost as work to revisit. The tombstone stays where 0021 put it; this
  adds a marker beside it.

## Context

A tombstone lives at the deleted record's object key and differs from a live
record only in its body. Hiding tombstones from the consumer `list` therefore
costs a `GetObject` per key on top of the `ListObjectsV2` pages, and `reset` and
`collect` inherit it. Bounded concurrency (gonzalo#286) capped the latency but
not the request count: listing N records costs N round trips and N charges.

`ListObjectsV2` returns key, size, ETag, last-modified and storage class. Only
the key carries meaning we assign, so making liveness visible to a listing is
necessarily a layout change. Two properties of the existing layout make a
sibling key safe: `segment()` percent-escapes `.`, so no encoded id can contain
one and no dotted suffix can collide with a record key; and `parse_object_key`
accepts only three slash-separated parts ending in `.json`, so any other shape is
already skipped rather than misread.

Three layouts were weighed. Moving the tombstone to its own key makes listing
entirely free but splits every delete, recreation and purge across two objects,
losing the single conditional write that the substrate's concurrency
correctness rests on. A per-collection tombstone index is cheaper still to read
but serializes every delete in a collection behind one object's compare-and-swap,
converting a read cost into a write-throughput ceiling. Both are argued in full
in the spec.

## Decision

**Deleting a record also writes a marker at
`namespace/collection/id.json.tombstone`, whose body is the tombstone's
revision.** The record object is untouched, so every compare-and-swap remains
one conditional write on one object. The marker is a hint about that object,
never a second source of truth.

The invariant is one-directional — **a tombstone implies a marker; a marker
implies nothing** — and two rules hold it. First, the marker is written *before*
the tombstone, so a crash between them leaves a marker for a record that is
still live rather than a tombstone with no marker. Second, **writers never
remove a marker; only `list` does, and only with `If-Match` on the ETag it saw
in its own listing.**

The second rule is what makes concurrent removal safe, and the reason is not a
crash but a race: if a recreation or a purge removed the marker it found, a
concurrent delete — which writes its marker first and its tombstone second —
could have that marker deleted out from under it, stranding an unmarked
tombstone on a perfectly healthy system. So a recreation leaves its marker
stale and a purge leaves its marker orphaned, and the next listing removes them
conditionally. Because a tombstone's revision counter always exceeds the record
it replaced, no two markers for a key share a body or an ETag, so a marker
rewritten since the listing is left alone. A stale marker costs one `GetObject`
to resolve and nothing else, and the layout heals itself.

`list` makes one pass, sorts record keys from marker keys as it goes, and reads
only marked keys. Steady-state cost falls from one `GetObject` per *record*,
which nothing bounds, to one per *tombstone*, which `collect` bounds.

**A reader may trust the absence of a marker only where markers have always been
maintained**, recorded per collection by a flag object at
`namespace/collection/_tombstone_markers`. Until a collection is flagged, `list`
behaves exactly as it does today — reading every key — and, while already
reading them, writes the markers it finds missing and then sets the flag. The
upgrade installs itself on first use: one listing at the old cost, every later
one at the new. Writers maintain markers regardless of the flag, which gates only
what a reader may assume.

## Consequences

- **Positive:** listing a large namespace stops costing a request per record,
  and `reset` and `collect` get the same reduction for free. The tombstone
  itself does not move, so ADR 0021's single-object compare-and-swap, its
  recreation semantics and its replication surface are all unchanged. A bucket
  written by the previous version upgrades without an admin step and without any
  window in which a deleted record could reappear in a listing.
- **Negative:** a delete now touches two objects instead of one, costing an
  extra `PutObject` of a few dozen bytes. It is unconditional, so it cannot fail
  a compare-and-swap, but it is a round trip that can fail on its own. Markers
  accumulate wherever listings are rare, because only a listing removes them: a
  workload that deletes and recreates the same keys without ever listing them
  leaves a stale marker per key, each costing one read whenever a listing
  finally happens. The first `list` of each collection after the upgrade still
  pays the old cost, and a deployment whose collections are never fully
  enumerated never earns the flag and never gets faster.
- **Neutral:** the layout is additive. Markers and the flag are invisible to
  `parse_object_key`, so an older reader ignores them entirely and behaves as it
  does today. Mixed-version deployments remain out of scope for the reason ADR
  0021 already gives: a 0.7.0 writer deleting into a flagged collection would
  write a tombstone with no marker, which is the breakage that ADR's
  upgrade-together requirement exists to prevent.
