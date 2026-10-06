# ADR 0029 · Manifest tombstones retain their body, and `undelete` restores it

- **Status:** accepted
- **Date:** 2026-10-03
- **Source:** [`docs/superpowers/specs/2026-10-03-manifest-tombstone-pin-design.md`](../superpowers/specs/2026-10-03-manifest-tombstone-pin-design.md)
- **Amends:** [ADR 0021](0021-replicated-deletion-with-tombstones.md), whose
  tombstone discarded the deleted body. For the two manifest kinds the body is
  now retained, and the tombstone gains a `deleted_kind` field.
- **Amends:** [ADR 0024](0024-blob-garbage-collection.md), whose pin covered only
  the blob a tombstone's `deleted_blob` named. The pin now also covers every
  blob a deleted manifest referenced.

Neither ADR is superseded; both stay `accepted`.

**Amended by** [ADR 0030](0030-manifest-tombstone-recreate-guard.md), which refuses a
consumer create over a manifest tombstone and so closes the recreate hazard
below.

## Context

ADR 0024 gives deletion three steps: `delete` writes a tombstone that **pins**
the deleted record's blob, `collect` removes the tombstone once it is older than
the operator's horizon, and only then does `gc` reclaim the bytes. The pin is
what makes an accidental delete recoverable for exactly that horizon.

That promise did not hold for a manifest. `tombstone_of`
(`crates/gonzalo-core/src/tombstone.rs`) replaced every body with an empty
inline one and pinned only a `Body::Blob` hash. A `VectorManifest`'s body is
inline JSON that names its shard blobs out of line, so deleting one did two
things:

- it dropped the shard hashes from the GC mark set, because
  `live_blob_hashes` (`crates/gonzalo-core/src/gc.rs`) marks a manifest's
  references by the record's kind, and the record's kind had become
  `Tombstone`; and
- it **destroyed the shard-to-blob mapping**, because the body that held it was
  discarded.

The next sweep therefore freed every shard. For a `GraphManifest` that is
recoverable, since slices regenerate by re-parsing source. For a vector shard
it is permanent: with caller-supplied embeddings (#317) gonzalo never saw the
model and cannot regenerate the vectors. ADR 0027 carried this as a known
consequence.

It is reachable by an operator. `gonzalo delete` on a manifest key does it, and
so does `reset`, which tombstones **every** key in a namespace
(`crates/gonzalo-core/src/reset.rs`), manifests included. Nothing else recovers
the lost body: `AncestryStore` retains committed bodies by revision hash, but
it is never constructed outside its own tests, so no production path can
recover a discarded one.

### Why pinning hashes alone was rejected

The first idea was to generalise `deleted_blob` into a set of hashes. That buys
almost nothing. The mapping is gone, so the preserved bytes are unnameable:
nothing can say which blob was which shard, or that they belonged to one index.
And a shard genuinely shared with another index is already marked live by that
index's manifest, so the pin adds nothing there either. Recovery needs the
mapping, not just the bytes, so the body has to be kept.

Two other options were weighed and rejected. **Documenting the exception**
(deleting a manifest is immediate and final) costs no code, but it makes the
promise true by narrowing it, and a recoverable delete of a vector index is
worth having. **Refusing `delete` for manifest kinds** is cheap, but it makes
`reset` either fail partway through a namespace or silently skip records, and it
removes a capability rather than fixing a defect.

## Decision

**A manifest tombstone retains the deleted body and records which kind it was;
GC marks through that body; and a new `undelete` restores the manifest from it.**

- **Retention.** `Record` gains `deleted_kind: Option<RecordKind>`, beside
  `deleted_at` and `deleted_blob`, with the same
  `#[serde(default, skip_serializing_if = "Option::is_none")]` attributes, so
  records already on disk read unchanged. `tombstone_of` keeps `current.body`
  and sets `deleted_kind` for `GraphManifest` and `VectorManifest` only. Every
  other kind is exactly as before: empty body, `deleted_blob` pinning a
  `Body::Blob` hash, `deleted_kind: None`.
- **Marking.** `live_blob_hashes` gains an arm for a `Tombstone` whose
  `deleted_kind` is a manifest kind, parsing the retained body with that kind's
  parser. `deleted_kind` is why this is a field rather than a guess: trying both
  parsers on every tombstone body would be ambiguous. A retained body that fails
  to parse is an error, never a reason to sweep, matching the existing manifest
  arm.
- **`collect` is unchanged.** Removing the tombstone unmarks the hashes and the
  next sweep frees the shards. The horizon still ends; ADR 0024's three steps
  now work for manifests.
- **`undelete`.** `gonzalo_core::undelete`, the `gonzalo undelete` subcommand and
  the facade re-export restore the manifest. It refuses, writing nothing, when
  there is no tombstone, the record is live, `deleted_kind` is `None`, a blob the
  body names is missing (it lists the hashes), or the key was re-created in the
  meantime. Otherwise it writes the retained body back under `deleted_kind`, one
  counter past the tombstone, with the tombstone as parent and `meta.created`
  preserved. It restores body, kind and `meta`; `links` are not restored because
  a tombstone never kept them.
- **Manifest kinds only.** Every other kind's tombstone still has no body, so
  there is nothing to restore it from.

Two properties of `tombstone_of` were checked because they are easy to break.
Two peers deleting the same revision still produce byte-identical tombstones: a
tombstone's revision hash is a constant that never depended on its body, and the
retained body is a deterministic clone. And `MergeClass::Opaque` still holds for
`RecordKind::Tombstone`, with sync reconciling tombstones before any body merge,
so retaining a body introduces no merge path that did not exist.

### Why `put_raw` and not a new planner arm

`undelete` rides `put_raw`, the replication write. Be plain about that: it is
the replication planner, and "raw" here means a record that already happened,
which a restoration is. `plan_put_raw` already writes a non-tombstone record
over a tombstone when `expected` matches the tombstone's revision, and it never
re-stamps anything. That is exactly an undelete, and it is what preserves
`meta.created`. A third planner arm would add a decision path to the planner
every substrate routes through, for no behaviour the existing one lacks. This
needs no new `Store` method, no substrate change, no wire change (records cross
gRPC as `bytes record_json`) and no daemon route (it uses `get_raw`, `put_raw`
and `has_blob`, which already have routes). Authorization is `write` on the
namespace, as for any raw write.

Because `undelete` passes the tombstone's revision as `expected`, it does not
trip `plan_put_raw`'s guard that makes a replication *create* over a tombstone a
`Conflict`. A restored record's parent is the tombstone, so peers order it after
the delete rather than racing it.

### Why `put` keeps resetting `created`

`plan_put` treats a tombstone as absent, so a create recreates past it and
resets `meta.created`: a recreation starts a new life at that key. Preserving
`created` there would be wrong, because `put` cannot distinguish restoring a
deleted record from reusing a recycled key, and it would make an identical call
mean different things depending on whether `collect` had run. Only an explicit
`undelete` can say "this is the same record returning".

## Consequences

- **Positive:** ADR 0024's promise now holds for manifests. Deleting a vector
  index no longer frees its shards on the next sweep, and a deleted index is
  recoverable for the whole horizon.
- **Positive:** the fix adds no wire surface, trait method or planner arm. Old
  records read unchanged, and a tombstone written before this one marks nothing,
  as it did not before.
- **Negative:** **a `RecordVectorIndex` opened over a manifest tombstone still
  opens successfully.** `RecordVectorIndex::open`
  (`crates/gonzalo-vector/src/record_index.rs`) reads with `store.get`, which
  hides tombstones, so it builds a fresh empty manifest and fails only at its
  first commit. A `put` that creates over a manifest tombstone is refused
  ([ADR 0030](0030-manifest-tombstone-recreate-guard.md)), so the restore
  window survives the attempt, and `gonzalo undelete` still works afterwards.
  The late failure is tracked in gonzalo#340.
- **Negative:** **a divergent delete can drop a pin early.** When two peers
  delete *different revisions* of the same manifest key, sync's
  diverged-tombstone merge calls `tombstone_winner`
  (`crates/gonzalo-core/src/tombstone.rs`), which keeps the tombstone with the
  higher `(counter, hash)` and discards the other. Shards unique to the losing
  revision lose their pin before the horizon. It degrades gracefully: the
  surviving tombstone is a complete, self-consistent manifest, so `undelete`
  restores a coherent index, and the higher counter means the *newer* manifest is
  the one that survives. Selection does not look at the body, so when the
  counters differ every peer converges on the same tombstone. **When the
  counters are equal they do not.** `tombstone_hash()` is a constant
  (`ContentHash::of(b"gonzalo:tombstone:v1")`), so every tombstone's revision
  hash is identical and two tombstones at the same counter have *equal
  revisions*. `tombstone_winner` compares `(counter, hash)` with `>=`, so on a
  tie each peer keeps its own side, and both `sync` (`Relation::InSync`) and git
  `pull` then treat the pair as already agreeing. The peers end up holding
  different retained bodies at the same revision, permanently and silently. It
  is reachable whenever two peers' live manifests diverged at the same counter
  (same counter, different body hash) and both were then deleted. Revision-level
  convergence holds; body-level convergence does not, and each peer's `undelete`
  restores its own manifest.
- **Negative:** **an older binary's `gc` frees the shards the pin protects.**
  `Record` has no `deny_unknown_fields`, so a binary that predates
  `deleted_kind` reads a manifest tombstone without error, but its
  `live_blob_hashes` does not mark through the retained body, and re-serialising
  the record drops the field. Its `gc` therefore frees exactly the shards the
  tombstone exists to pin, and a vector index's shards cannot be regenerated.
  Compatibility holds one way only: old records read unchanged under the new
  binary, but a new record is not safe under the old one. The realistic exposure
  is one store root run by two binary versions; sync copies records and not
  blobs, so peers with separate blob stores are not affected. Upgrade every
  binary that runs `gc` against a store before relying on the pin.
- **Negative:** **a manifest tombstone is no longer small.** It carries the
  manifest's JSON, a few hundred bytes for a 256-shard index, until `collect`
  removes it. That departs from "tombstones are tiny and uniform".
- **Negative:** **only manifest kinds can be undeleted**, and past `collect` a
  deleted index is gone. The second is ADR 0024's design rather than a gap; the
  first is a deliberate asymmetry, since retaining every kind's body would bloat
  every tombstone.
- **Negative:** **#325's residual sweep windows still apply.** A sweep that
  decided to delete a shard before the tombstone existed will delete it even
  though the tombstone now pins it. #328 tracks closing them.
- **Negative:** `undelete` restores the record, not the world. If another index
  referenced the same shards and was itself deleted and collected, those blobs
  are gone and `undelete` refuses, naming them.
- **Negative:** `undelete`'s `PutResult::Conflict` arm, taken when the key
  changes between its read and its write, is a race guard that no
  single-threaded test reaches.
- **Negative:** **#198's drift check compares paths and methods, not schemas**,
  so the `deleted_kind` addition to the `Record` schema in
  `docs/api/openapi.json` is not guarded by a test.

## Revisit if

- A kind other than a manifest needs to be undeletable, which would mean
  deciding when a body is worth retaining rather than hard-coding two kinds.
- Manifest tombstones grow costly: a very large shard count, or very many
  deleted indexes inside one horizon.
- `put` ever needs to express succession explicitly, which reopens the
  consumer-path question and is also the natural fix for the reopen hazard
  above.
- Either operator hazard above is observed in practice.
