# Manifest tombstone pin and undelete — design

**Ticket:** [#327](https://github.com/caliban-ai/gonzalo/issues/327)
**Date:** 2026-10-03
**Status:** approved, ready for an implementation plan

## Context

ADR 0024 gives deletion a three-step shape: `delete` writes a tombstone that
**pins** the deleted record's blob, `collect` removes the tombstone past the
operator's horizon, and only then does `gc` reclaim the bytes. The pin is what
makes an accidental delete recoverable for exactly that horizon.

That promise does not hold for a manifest. `tombstone_of` replaces the body with
`Body::Inline(Vec::new())` and pins only a `Body::Blob` hash
(`crates/gonzalo-core/src/tombstone.rs`). A `VectorManifest`'s body is inline
JSON naming shard blobs out of line, so deleting one:

- drops the shard hashes out of the GC mark set — `live_blob_hashes`'s
  `VectorManifest` arm is gated on `record.kind`, and the record's kind is now
  `Tombstone`; and
- **destroys the shard-id → blob-hash mapping**, because the body is discarded.

The next sweep therefore frees every shard, and ADR 0027 carries that as a known
consequence. `GraphManifest` has the same shape, but its slices regenerate from
source; a vector shard does not, because with caller-supplied embeddings (#317)
gonzalo never saw the model.

### Why pinning the hashes alone would not help

The ticket's first option was to generalise `deleted_blob` to a set of hashes.
That buys almost nothing:

- The mapping is gone, so preserved bytes are unnameable. Nothing can tell which
  blob was which shard, or that they belonged to one index.
- A shard genuinely shared with another index is **already** marked live by that
  index's manifest, so the pin adds nothing there either.

Recovery needs the mapping, not just the bytes. So the fix is to retain the body.

### How it is reachable

No in-tree code deletes a manifest. The realistic triggers are an operator's
`gonzalo delete` on a manifest key, and `reset_as`, which tombstones **every**
key in a namespace (`crates/gonzalo-core/src/reset.rs`) — including a manifest.

`AncestryStore`, which would otherwise retain each committed body by revision
hash, is never constructed outside its own tests, so there is no production path
that recovers a discarded manifest body.

## Decisions

1. **A manifest tombstone retains its body**, and records which kind was deleted
   in a new `Record` field `deleted_kind: Option<RecordKind>`.
2. **GC marks through a retained body.** `live_blob_hashes` gains an arm for a
   `Tombstone` whose `deleted_kind` is a manifest kind.
3. **`undelete` rides the existing `put_raw` path**, preserving `meta.created`
   and continuing the key's revision lineage. No new planner, no new `Store`
   method, no substrate change, no new daemon route.
4. **`undelete` works for manifest kinds only** — the kinds whose body is
   retained.
5. **`collect` is unchanged.** Removing the tombstone unmarks the hashes and the
   next sweep frees them, which is ADR 0024's three-step finally working for
   manifests.

### Rejected alternatives

- **Document the exception** — state in ADR 0024 and 0027 that deleting a
  manifest is immediate and final. Zero code, and it makes the promise true by
  narrowing it. Rejected in favour of making the promise hold, since recoverable
  deletion of a vector index is worth having.
- **Pin the hashes without the body** (the ticket's own first option). Rejected:
  see "Why pinning the hashes alone would not help".
- **Refuse `delete` for manifest kinds**, requiring something explicit. Cheap,
  but it makes `reset_as` either fail partway through a namespace or silently
  skip records, and it removes a capability rather than fixing it.
- **A third planner arm (`plan_undelete`).** This was the initial design, and it
  is unnecessary. `plan_put_raw` already writes a non-tombstone record over a
  tombstone when `expected` matches the tombstone's revision, and it never
  re-stamps anything — which is exactly an undelete. A third arm would add a
  decision path to the planner every substrate routes through, for no behaviour
  the existing one lacks.

## The record model

```rust
/// Tombstones only: the kind of the record that was deleted.
///
/// Set when the deleted body is retained — the manifest kinds, whose bodies
/// name blobs out of line — so GC knows which parser to use for the pinned
/// references and `undelete` knows what kind to recreate. `None` on a tombstone
/// whose body was discarded, and on every tombstone written before this.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub deleted_kind: Option<RecordKind>,
```

Placed beside `deleted_at` and `deleted_blob`, with the same serde attributes, so
records already on disk deserialise unchanged and behave exactly as they do
today. `RecordKind` serialises PascalCase, so the field reads
`"deleted_kind": "VectorManifest"`.

### `tombstone_of`

```rust
let retain = matches!(
    current.kind,
    RecordKind::GraphManifest | RecordKind::VectorManifest
);
```

- **Retaining kinds:** `body: current.body.clone()`, `deleted_kind: Some(current.kind)`,
  `deleted_blob: None`.
- **Every other kind:** unchanged — `body: Body::Inline(Vec::new())`,
  `deleted_blob` pinning a `Body::Blob` hash, `deleted_kind: None`.

Two properties this preserves, each easy to break and each worth a test:

- **Two peers deleting the same revision still produce byte-identical
  tombstones.** `tombstone_hash()` is a constant — `ContentHash::of(b"gonzalo:tombstone:v1")`
  — so a tombstone's revision never depended on its body, and the retained body
  is a deterministic clone of `current.body`.
- **`MergeClass::Opaque` still holds for `RecordKind::Tombstone`,** and sync
  reconciles tombstones before any body merge runs, so retaining a body
  introduces no merge path that did not exist.

The cost: a manifest tombstone is now as large as the manifest it replaced — a
few hundred bytes of JSON for a 256-shard index — until `collect` removes it.
That is a real departure from "tombstones are tiny and uniform".

## Garbage collection

`live_blob_hashes` gains one arm:

```rust
if record.kind == RecordKind::Tombstone {
    match record.deleted_kind {
        Some(RecordKind::VectorManifest) => live.extend(
            VectorManifest::from_body(&record.body)?.entries.into_values(),
        ),
        Some(RecordKind::GraphManifest) => live.extend(
            Manifest::from_body(&record.body)?.entries.into_values(),
        ),
        _ => {}
    }
}
```

`deleted_kind` is why this is a field rather than a guess: trying both parsers on
every tombstone body would be ambiguous and would make an unrelated inline body
that happens to parse look like a manifest.

A retained body that fails to parse propagates the error rather than sweeping,
matching the existing `GraphManifest` arm and its test
`mark_set_reports_an_undecodable_manifest_rather_than_sweeping_it`.

## `undelete`

A core function, with a CLI wrapper, built entirely from operations that already
exist:

```rust
pub async fn undelete<S: Store + BlobStore + ?Sized>(
    store: &S,
    key: &RecordKey,
    now_ms: i64,
    author: Option<&Identity>,
) -> Result<Revision>
```

1. `store.get_raw(key)` — must be `Some(t)` with `t.is_tombstone()` and
   `t.deleted_kind == Some(kind)`. Anything else is an error that says why.
2. Verify every blob the retained body names still exists, via
   `BlobStore::has_blob`. If any is gone, refuse and name the missing hashes.
3. Build the restored record:
   - `kind` from `deleted_kind`, `body: t.body.clone()`
   - `revision: Revision { counter: t.revision.counter + 1, hash: ContentHash::of(t.body.bytes()) }`
   - `parent: Some(t.revision.clone())`
   - `meta: Meta { created: t.meta.created, updated: now_ms, .. }`, author from
     `author` or the tombstone's
   - `deleted_at: None`, `deleted_blob: None`, `deleted_kind: None`
   - `ancestors: Vec::new()` — the planner folds them
4. `store.put_raw(restored, Some(t.revision))`. `plan_put_raw`'s
   `(Some(c), Some(e)) if e == c.revision` arm writes it, folding the tombstone
   into `ancestors` and **re-stamping nothing**, which is what preserves
   `created`.

Step 2 before step 4 is deliberate: a record for an index that cannot be opened
is worse than a refusal, and the check reuses what #325 added.

### Why `put_raw` rather than `put`

`plan_put` treats a tombstone as absent — a create recreates past it, and
`meta.created` is reset because "a recreation starts a new life at this key".
That is the right default for `put`, which cannot distinguish restoring a deleted
record from reusing a recycled key, and which must not change meaning depending
on whether `collect` has run. An explicit `undelete` can say "this is the same
record returning", and `put_raw` is the path that does not re-stamp.

It is worth stating plainly in the ADR that undelete rides the replication
planner, so no one is surprised: "raw" means a record that already happened, and
a restoration is exactly that.

### The questions this raises, and their answers

- **The key was re-created meanwhile.** The CAS fails: `plan_put_raw` returns
  `Conflict` carrying the live record, and undelete refuses rather than
  clobbering it.
- **The horizon has passed.** `collect` removed the tombstone, so step 1 finds
  nothing and the error says there is nothing to restore. The time bound is
  explicit in the operation instead of hidden in `put`.
- **Replication.** The restored record replicates like any other, and because its
  parent is the tombstone, peers order it after the delete rather than racing it.
  `plan_put_raw`'s existing guard — a replication *create* over a tombstone is a
  `Conflict`, "so sync re-reads instead of turning a copy into a recreation that
  would resurrect a deleted record" — is untouched, because undelete passes the
  tombstone's revision explicitly rather than creating blind.
- **Authorization.** `Write` on the namespace, the same as any `put_raw`. The
  daemon restamps `meta.author` for a non-admin, so a non-admin's restore is
  attributed to them and an admin's keeps the original author. Neither path
  touches `meta.created`, so provenance survives over the daemon.

## Known limits

- **Only manifest kinds can be undeleted.** Every other kind's tombstone has no
  body to restore from. Retaining bodies for all kinds would bloat every
  tombstone, so this asymmetry is deliberate.
- **The horizon still governs.** Past `collect`, a deleted index is gone. That is
  ADR 0024's design, not a gap.
- **A manifest tombstone is no longer small.** It carries the manifest's JSON
  until collected.
- **#325's residual windows still apply.** A sweep that already decided to delete
  a shard will delete it even though the tombstone now pins it, if the delete was
  decided before the tombstone existed. Nothing here changes that; #328 tracks it.
- **`undelete` restores the record, not the world.** If another index referenced
  the same shards and was itself deleted and collected, those blobs are gone and
  step 2 refuses.

## Testing

Red-first wherever a test states a real claim.

| Test | What it proves |
|---|---|
| Delete a vector manifest, sweep with `min_age: ZERO` — shards survive | The bug. Red first: today they are swept |
| Same for a graph manifest's slices | The other retaining kind |
| After `collect` removes the tombstone, the next sweep frees the shards | Pinning did not become leaking; the horizon still ends |
| A non-manifest tombstone is unchanged: empty body, `deleted_blob` set, `deleted_kind: None` | Nothing else moved |
| Two peers deleting the same manifest revision produce byte-identical tombstones | The determinism claim above |
| A tombstone deserialised without `deleted_kind` marks nothing | Every tombstone already on disk |
| A retained body that will not parse errors rather than sweeping | Mirrors the existing undecodable-manifest case |
| Conformance: deleting a manifest kind retains the body and `deleted_kind`, and the substrate round-trips both | Every substrate. A substrate that drops either silently unpins the shards |
| Delete a vector index, `undelete`, reopen and query it | The capability, end to end |
| `undelete` preserves `meta.created`; counter is `t.counter + 1`; parent is the tombstone; `ancestors` contains it | Provenance and lineage |
| `undelete` refuses when the key was re-created | Does not clobber a live record |
| `undelete` refuses when there is no tombstone, or the record is live | Nothing to restore |
| `undelete` refuses, naming the hashes, when a named blob is missing | No record for an unopenable index |
| `undelete` refuses a tombstone with `deleted_kind: None` | The stated limit, with a message that explains it |
| CLI `gonzalo undelete`: happy path and each refusal | The operator surface |

The conformance case matters most of the new ones: the existing
`tombstone_pins_the_deleted_records_blob` lives there precisely because "a
substrate that drops the field on the way to storage frees bytes a peer can still
resurrect the record from", and `deleted_kind` plus the retained body carry the
same hazard.

Two existing conformance cases were checked and are unaffected:
`tombstone_never_collides_with_empty_body` compares revisions, not bodies, and
`tombstone_pins_the_deleted_records_blob` asserts an empty body for a
**blob-backed, non-manifest** record, which stays true. The latter's doc comment
says "The tombstone's body is empty" and must be qualified as kind-specific.

## Documentation

- **ADR 0029**, amending **ADR 0021** (tombstone shape: manifest bodies are
  retained) and **ADR 0024** (the pin now covers a manifest's references).
  Neither is superseded; both stay `accepted`, annotated on both sides.
- **ADR 0027**'s "deleting a vector manifest does not pin its shards"
  consequence is now resolved: rewrite it to point at 0029 instead of at open
  work, and drop the `#327` open-work reference.
- `docs/guide/src/deletion.md` — the manifest case in the delete/collect/gc
  story, and `gonzalo undelete`.
- `docs/guide/src/storage.md` — deleting a vector index is recoverable within the
  horizon.
- `CHANGELOG.md` — the additive `deleted_kind` field, retained manifest bodies,
  GC marking through them, `undelete` in core and the CLI.
- `docs/api/openapi.json` — a `deleted_kind` property on the `Record` schema.
  Additive. Note that #198's drift check compares paths and methods only, so this
  edit is unguarded by a test.

No proto change: records cross gRPC as `bytes record_json`. No new daemon route:
undelete uses `get_raw`, `put_raw` and `has_blob`, all of which already have one.

## Out of scope

- Retaining bodies for non-manifest kinds.
- Exposing tombstone revisions on the consumer read path, which is what allowing
  `put` to CAS against a tombstone would require.
- Closing #325's residual sweep windows (#328).
- Wiring `AncestryStore` in production, which would make any deleted body
  recoverable and is a much larger decision.

## Revisit if

- A kind other than a manifest needs to be undeletable, which would mean
  deciding when a body is worth retaining rather than hard-coding two kinds.
- Manifest tombstones grow large enough that retaining bodies costs real storage
  — a very large shard count, or very many deleted indexes inside one horizon.
- `put` ever needs to express succession explicitly, at which point the
  consumer-path question reopens.
