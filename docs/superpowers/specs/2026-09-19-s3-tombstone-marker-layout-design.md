# S3 tombstone markers — design

**Ticket:** [gonzalo#294](https://github.com/caliban-ai/gonzalo/issues/294)
**Status:** approved
**Date:** 2026-09-19

## 1. Problem

Since tombstones (ADR 0021), the consumer `list` has to hide tombstoned keys,
and a tombstone is only recognisable by reading the record — it lives at the
deleted record's own object key, `namespace/collection/id.json`, and differs
only in its body. On fs and git that costs a local file read per key. On S3 it
costs a `GetObject` per key on top of the `ListObjectsV2` pages:

```rust
// crates/gonzalo-store-s3/src/lib.rs — Store::list today
let keys = self.list_keys(prefix).await?;          // 1 pass, paginated
for batch in keys.chunks(LIST_READ_CONCURRENCY) {  // then N GetObjects,
    …                                              // 16 in flight
}
```

`reset` and `collect` inherit the cost, because both enumerate before acting.
Bounded concurrency (#286 item 5) made the latency survivable; it did not change
that a listing of N records costs N round trips and N request charges.

`ListObjectsV2` returns only key, size, ETag, last-modified and storage class
per object. Of those, **only the key is ours to assign meaning to.** Any fix is
therefore a layout change.

## 2. Two facts the layout rests on

1. **`segment()` escapes `.`** (`crates/gonzalo-core/src/paths.rs:21-34`: only
   `A-Za-z0-9-_` pass through literally). An encoded id can never contain a dot,
   so a *sibling* key built by appending a dotted suffix to an object key cannot
   collide with any record's own key, for any id a caller can write.
2. **`parse_object_key` requires exactly `.json` and three slash-separated
   parts.** Anything else already parses to `None` and is skipped by every
   listing — so a new key shape is invisible to existing code paths rather than
   misread by them.

## 3. Decision

### 3.1 The marker

Deleting a record additionally writes a **zero-byte marker object** beside it:

```
ns/col/id.json             the record — a tombstone, unchanged in place
ns/col/id.json.tombstone   the marker — zero bytes, presence is the signal
```

The record object keeps its current meaning, so **every compare-and-swap stays a
single conditional write on a single object.** The marker is a hint about that
object, never a second source of truth.

### 3.2 The invariant, and the ordering that buys it

> **A tombstone implies a marker.** A marker implies nothing.

Enforced by ordering every marker write on the safe side of the record write:

| Operation | Order |
|---|---|
| delete / replicate a tombstone | `PUT` marker, **then** `PUT` tombstone (OCC) |
| recreate over a tombstone | `PUT` live record (OCC), **then** `DELETE` marker |
| purge | `DELETE` record, **then** `DELETE` marker |

Every crash window leaves a marker that is *stale* — present for a record that
is live or absent — and never a tombstone that lacks one. A stale marker costs
one `GetObject` to resolve and nothing else. This is the whole reason the
marker is written first and removed last.

### 3.3 Reading

`list` makes one `ListObjectsV2` pass and sorts the keys it sees into records
and markers in that same pass — markers are siblings, so the existing prefix
already covers them, and no second traversal is needed.

- A record key **with no marker** is live. No read.
- A record key **with a marker** is read to resolve it: a tombstone is hidden; a
  live record is listed **and its stale marker is deleted** (self-healing); an
  absent record leaves an orphan marker, which is deleted.
- A marker with no record key is an orphan; delete it.

Steady-state cost: one listing pass plus one `GetObject` per *tombstone*, which
`collect` bounds — instead of one per *record*, which nothing bounds.

`list_raw` is unchanged: it never filtered, so it never read, and markers are
simply not record keys.

### 3.4 Trusting the absence of a marker

"No marker means live" is only true where markers have always been maintained.
A bucket written by 0.7.0 has tombstones with no markers, and trusting absence
there would resurrect deleted records in `list` — the one outcome this design
must never produce.

So trust is **earned per collection** and recorded in the store:

```
ns/col/_tombstone_markers    body: "1"   (the marked flag)
```

The name has no dot and does not end in `.json`, so — by §2 fact 2 — it is not a
record key and cannot be produced by one.

- **Flag present:** the fast path of §3.3.
- **Flag absent:** exactly today's behaviour — read every key, bounded
  concurrency, correct and no faster. While that pass is already reading every
  record, it **writes the markers it finds missing**, and on completion sets the
  collection's flag.

The upgrade therefore installs itself: the first `list` of a collection after
the upgrade costs what it costs today, and every later one takes the fast path.
Nothing to run, nothing to remember, and no window in which a deleted record can
reappear.

A pass that spans several collections (a namespace-scoped or unscoped `list`)
marks each collection it fully enumerated. A pass that could not prove a
collection does not flag it.

Writers always maintain markers regardless of the flag; the flag gates only
whether a *reader* may trust an absence. So markers accumulate from the moment
the new binary runs, and the backfill has less to do the longer it waits.

### 3.5 Caching the flag

Reading the flag per `list` would add a round trip to the operation this design
exists to speed up. The store caches the collections it has seen marked. The
flag is monotonic — it is set once and never cleared — so a cached `true` can
never go stale. A cached `false` is not cached at all: an unmarked collection
re-checks, which costs one `HeadObject` against a path that is already paying N
reads.

`S3Store::handle()` clones the store per read task, so the cache must be shared
rather than copied: `Arc<RwLock<BTreeSet<(String, String)>>>` keyed by the
encoded `(namespace, collection)`.

## 4. What this costs

Per delete: one extra `PutObject` (zero bytes). Per recreation over a tombstone:
one extra `DeleteObject`. Per purge: one extra `DeleteObject`. All are on write
paths that already do a read-modify-write round trip, and none is conditional,
so none can fail a compare-and-swap or retry loop.

Mixed-version deployments are out of scope, as they already are: ADR 0021
requires every binary that reads a store or runs sync to be upgraded together. A
0.7.0 writer deleting into a collection this version has already flagged would
write a tombstone with no marker, and `list` would show it as live. That is the
same class of breakage ADR 0021 already documents, not a new one.

## 5. Testing

1. **Unit (no S3):** marker key derivation round-trips and cannot collide with a
   record key, including ids containing `.`, `/`, `%` and `.json.tombstone`
   itself; the marked-flag key is likewise unproducible.
2. **Integration (RustFS), the behaviours:** a delete writes a marker; a
   recreation removes it; a purge removes it; `list` hides a tombstone on the
   fast path; `list_raw` still shows it.
3. **Integration, the invariant under failure:** a stale marker over a live
   record must not hide it, and must be cleaned up; an orphan marker over an
   absent record must be cleaned up. Both are written directly with the S3
   client to simulate the crash window, since the code cannot produce them.
4. **Integration, the upgrade:** seed a collection the way 0.7.0 would — records
   and tombstones, no markers, no flag — then assert the first `list` returns
   the correct live set, has written the missing markers, and has set the flag;
   and that a second `list` returns the same set having read only the
   tombstones.
5. **Conformance:** `run_store_conformance` and `run_tombstone_conformance`
   continue to pass against S3 unchanged. They are the substrate-independent
   contract and must not need a special case for this.
6. **Benchmark (the ticket's acceptance):** over a namespace with N live records
   and M tombstones on RustFS, measure `list` before and after. Report the
   request count as well as wall-clock — the request count is the part that does
   not depend on the machine the benchmark ran on.

## 6. Rejected alternatives

**Move the tombstone to its own key** (`id.json.tombstone` holding the record).
Listing becomes entirely free — no `GetObject` at all — but the state change then
spans two objects, so delete, recreation and purge each lose their single
conditional write. Both keys can exist after a crash, needing a repair rule
(higher revision counter wins) that has to be right in every concurrent
interleaving, and reading a deleted key costs two `GetObject`s. Trading the
substrate's central atomicity guarantee for the last `O(tombstones)` reads is
the wrong side of the bargain.

**A per-collection tombstone index object.** One object listing the collection's
tombstoned ids, conditionally updated on delete and purge. Reads get cheaper
still, but every delete in a collection then contends on one object's
compare-and-swap, so concurrent deletes serialize and retry. It converts a read
cost into a write-throughput ceiling, which is a worse trade for a store whose
deletes are already the rarer operation.

**Inferring liveness from listing metadata.** `ListObjectsV2` exposes size and
ETag; a tombstone's serialized body is small and structurally bounded, so size
"nearly" discriminates. Nearly is not a contract — a live record with a tiny
body would be hidden, silently. Rejected on correctness.
