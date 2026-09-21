# Durable vector index — design

**Ticket:** [#323](https://github.com/caliban-ai/gonzalo/issues/323)
**Date:** 2026-09-20
**Status:** approved, ready for an implementation plan

## Context

Gonzalo's retrieval capability layer has no persistence and, outside its own
unit tests, no callers.

`MemoryVectorIndex` is a `Mutex<HashMap<RecordKey, Vec<f32>>>`
(`crates/gonzalo-vector/src/index.rs:47`) with no save, load, or `Serialize`
anywhere in the crate. Every construction site of an index or a
`KnowledgeStore` in the workspace sits inside a `#[cfg(test)]` module:

| File | Construction sites | `mod tests` begins |
|---|---|---|
| `gonzalo-vector/src/index.rs` | 145–406 | 121 |
| `gonzalo-vector/src/hnsw.rs` | 227–338 | 218 |
| `gonzalo-knowledge/src/lib.rs` | 825, 872 | 353 |

Outside those crates there are no construction sites at all — only the facade's
re-exports at `crates/gonzalo/src/lib.rs:232,251`. Neither `gonzalo-cli` nor
`gonzalo-server` depends on `gonzalo-vector`, so no shipped binary can hold an
index. ADR 0014 already anticipates this under "Revisit if": *"we need on-disk
or distributed scale"*.

This blocks #317 (recall over MCP) and #199 (hybrid retrieval). It became
urgent when #317 settled on **caller-supplied vectors**: with the embedder on
the caller's side of the boundary, an index can never be rebuilt by
re-embedding, so the vectors themselves must persist.

## Decisions

1. **Gonzalo owns durable vectors for the working-set case** — thousands to
   ~100k chunks — and `VectorIndex` remains the seam through which an external
   backend (#202) can later replace that wholesale.
2. **No new persistence trait.** `VectorIndex`'s existing surface — `upsert`,
   `remove`, `query(&[f32], k, &KeyPrefix)` — is already what a remote vector
   store offers, and a remote backend serves queries itself rather than sitting
   behind an in-memory mirror. `RecordVectorIndex` implements `VectorIndex`;
   #202 later adds a sibling implementation. A second trait underneath would
   have exactly one useful implementation.
3. **Sharded blobs under one manifest record.** Vectors are bucketed by a hash
   of their chunk key into a fixed number of shards; each shard is one blob;
   one `VectorManifest` record names them.
4. **An embedding-space tag is required and enforced**, bound when the index is
   opened.
5. **`MergeClass::Opaque`**, not `Derived`.

### Why sharded blobs

At the target scale — 384 dimensions × 4 bytes = 1536 B per vector, ≈150 MB for
100k chunks:

- **One blob per vector** means 100k objects to read at startup. #294 exists
  precisely because per-object reads at that count hurt on S3.
- **One blob for the whole index** rewrites 150 MB on every upsert, so a single
  added memory costs a full rewrite.
- **256 shards** of ~600 KB each: 256 reads at startup, and one ~600 KB rewrite
  per upsert. Bulk loads touch each shard once.

### Why `Opaque` rather than `Derived`

`GraphManifest` is `Derived` on the stated grounds that "the body can be
re-derived from source" (`crates/gonzalo-core/src/record.rs:78`), which lets a
divergence resolve deterministically in favour of side A with no content merge.
Caller-supplied vectors cannot be re-derived — gonzalo never sees the model — so
under `Derived` a divergence discards one side's vectors permanently.
`Structured` is no better: two writers touching the same shard produce different
blob hashes for it, and a field-level merge picks one and drops the other's
writes. `Opaque` surfaces the conflict instead, and the OCC retry on the write
path means callers rarely see one.

## Data model

A new `RecordKind::VectorManifest` with `MergeClass::Opaque`. One manifest
record per index, keyed `<namespace>/<collection>/<index-id>`.

The body type lives in **`gonzalo-core`**, beside `Manifest`, because `gc.rs`
must parse it and core cannot depend on the capability layer (ADR 0008):

```rust
pub struct VectorManifest {
    /// Caller-declared embedding space, e.g. "bge-small-en-v1.5".
    pub space: String,
    /// Vector dimension. Every shard entry carries exactly this many floats.
    pub dim: usize,
    /// Number of shards, fixed when the index is created.
    pub shards: u16,
    /// Shard id -> blob holding that shard's vectors.
    pub entries: BTreeMap<u16, ContentHash>,
}
```

`space` and `dim` are load-bearing and get types rather than being tucked into
`meta.labels`. Like `Manifest`, it gains `to_body()` / `from_body()`.

### Shard assignment

Stable across processes, platforms and releases, and without a new dependency:

```rust
fn shard_of(key: &RecordKey, shards: u16) -> u16 {
    let s = format!("{}/{}/{}", key.namespace, key.collection, key.id);
    let h = ContentHash::of(s.as_bytes());          // blake3, already in core
    u16::from_str_radix(&h.0[..4], 16).unwrap() % shards
}
```

`shards` defaults to 256 at creation and is read from the manifest thereafter,
so the default can change later without breaking existing indexes.

### Shard blob format

Little-endian, entries sorted by `(namespace, collection, id)` so identical
content produces identical bytes and content-addressing dedups an unchanged
shard:

```
magic   "GZVS"        4 bytes
version u8            = 1
dim     u32
count   u32
repeat count times:
    ns_len  u16 | ns  utf8
    col_len u16 | col utf8
    id_len  u32 | id  utf8
    vector  dim * f32
```

A shard whose `dim` disagrees with the manifest's is a corrupt-store error, not
a silent truncation.

## Public API

```rust
impl<S: Store + BlobStore> RecordVectorIndex<S> {
    /// Open the index at `key`, hydrating it from the store.
    ///
    /// Errors if a manifest exists whose `space` or `dim` differs from the
    /// declared one, naming both values. If no manifest exists the index starts
    /// empty and the manifest is created on the first commit.
    pub async fn open(store: S, key: RecordKey, space: &str, dim: usize)
        -> Result<Self>;
}

#[async_trait]
impl<S: Store + BlobStore> VectorIndex for RecordVectorIndex<S> { /* … */ }
```

Two additions to the `VectorIndex` trait. `upsert_many` gets a default so
existing implementations keep compiling unchanged; `keys` cannot — there is no
way to enumerate an arbitrary index — so it is required and implemented for each
of the three in-tree implementations. A default returning an error would turn a
compile-time gap into a runtime one.

```rust
/// Insert or replace many vectors. The default loops over `upsert`;
/// `RecordVectorIndex` overrides it to write each dirty shard once and commit
/// the manifest once.
async fn upsert_many(&self, items: Vec<(RecordKey, Vec<f32>)>) -> Result<()>;

/// Every key in the index matching `filter`. Used to rebuild derived state
/// (see "Chunk counts"); order is unspecified.
async fn keys(&self, filter: &KeyPrefix) -> Result<Vec<RecordKey>>;
```

One addition to `MemoryVectorIndex`, so the write path can read a shard's
members back without a second copy of every vector:

```rust
/// Entries whose key satisfies `pred`. Predicate-shaped rather than
/// shard-shaped, so sharding does not leak into the in-memory index.
pub fn collect_where(&self, pred: impl Fn(&RecordKey) -> bool)
    -> Vec<(RecordKey, Vec<f32>)>;
```

`RecordVectorIndex` composes a `MemoryVectorIndex` rather than duplicating its
map — at 150 MB, holding the vectors twice is not a rounding error — and
delegates `query` to it untouched, keeping cosine scoring and its tie-breaking
in one place.

## Load path

1. `store.get(&key)` for the manifest record.
   - Absent: start empty with the declared `space`/`dim`; the manifest is
     created on first commit.
   - Present with mismatched `space` or `dim`: error naming both values.
2. Read `entries` blobs with bounded concurrency — 16, matching the constant
   `gonzalo-store-s3` settled on for the same reason.
3. Decode each shard and `upsert` its entries into the inner
   `MemoryVectorIndex`.

A missing shard blob is a corrupt-store error naming the shard id and hash. It
means `gc` swept a live blob or a write was lost, and it must be loud.

## Write path

`upsert` / `remove`:

1. Apply to the inner memory index.
2. Compute the dirty shard via `shard_of`.
3. Re-serialise that shard from `collect_where` and `put_blob` it.
4. Read the current manifest, set `entries[shard]`, and `put` with
   `expected = current.revision`.
5. On `PutResult::Conflict`: re-read the manifest, reload **that shard** from
   the winner's blob, re-apply our own delta to it, re-serialise, and retry.
   Bounded at **5 attempts**, then `CoreError::Backend` naming the index key.

Shard blobs orphaned by a lost race are left for `gc`, exactly as the graph
indexer already does (`crates/gonzalo-cli/src/lib.rs:645`).

`upsert_many` groups items by shard, rewrites each dirty shard once, and makes a
single manifest commit. Without it a 100k-chunk bulk load would mean 100k
manifest commits.

**Single writer per index.** OCC detects a concurrent writer and retries; it
does not merge, and a second writer's in-memory view can be stale until it
reopens. The graph views already carry this assumption; here it is explicit.

## Space enforcement, and its limit

The space is bound at `open`, so every query through a handle is in that space
by construction. `VectorIndex::query` keeps its signature, and
`MemoryVectorIndex` — which has no durable space — is unaffected.

What this guarantees: the space a caller **declares** matches what the index was
built with. It catches configuration drift — a swapped embedder, a restored
index, two services disagreeing — which is the realistic failure, and it fails
once at startup rather than as quietly wrong rankings on every later query.

What it cannot guarantee: that a vector actually came from the declared model.
With caller-supplied embeddings gonzalo never sees the model, so a caller that
declares one space and sends vectors from another is undetectable. Dimension is
the only intrinsic check available, and it is weak — all-MiniLM-L6-v2 and
bge-small are both 384.

## Garbage collection

`live_blob_hashes` (`crates/gonzalo-core/src/gc.rs`) gates its manifest arm on
`record.kind == RecordKind::GraphManifest` exactly. Adding `VectorManifest`
without extending that arm means the next `gonzalo gc` deletes every vector
blob while the manifest still points at them — silent data loss, the same shape
as the bug fixed in #292.

The arm gains a `VectorManifest` case extending the mark set with
`entries.into_values()`. This is proven red first: a store with a vector
manifest and its shard blobs, `gc` run, blobs asserted present — failing before
the arm exists.

## Chunk counts

`KnowledgeStore.chunk_counts` exists so a re-ingest that shrinks a record drops
its orphaned high-ordinal chunks (#150). It is held in memory, so it resets on
restart — and durable vectors make that reachable: re-ingest a shrunk record
after a restart and the orphans stay, matching queries and de-duping to a parent
that no longer covers them.

`KeyPrefix` is `{ namespace, collection }` with no id prefix, so per-ingest
lookups would scan the whole map on every ingest — 10k ingests against a 100k
index is 10⁹ operations. Hydrate once instead:

```rust
/// Open against a durable index, rebuilding chunk counts from it.
/// `new` remains for the in-memory case, where counts start empty.
pub async fn open(store: S, index: V, embedder: E) -> Result<Self>;
```

One O(n) scan at construction via `keys`; `ingest` stays as fast as it is today.

This part is **severable** — it fixes a bug reachable only by callers who have
an embedder configured, and #317's caller-supplied path never calls `ingest`.
It is the last task so it can be cut, but shipping durability that makes a known
bug reachable and leaving it is not a good trade.

## Testing

Red-first wherever the test states a real claim:

| Test | What it proves |
|---|---|
| `gc` leaves vector shard blobs alone | The mark-set arm. Red first — the failure is deletion of live data |
| upsert → drop → reopen → query | The point of the ticket: recall survives a process |
| `open` with a different `space` | Errors naming both spaces rather than serving wrong scores |
| `open` with a different `dim` | Same, for dimension |
| identical shard content → identical hash | Content-addressing dedups an untouched shard |
| two writers, same shard | Both vectors present after the OCC retry; neither silently lost |
| `upsert_many` of N vectors | One manifest revision advance, not N |
| shard blob orphaned by a lost race | Reclaimed by a later `gc` |
| missing shard blob on open | Loud corrupt-store error, not a silent short index |
| shrink → reopen → re-ingest | No orphan chunks (the counts fix) |

Acceptance: an integration test that writes 10k vectors, drops the handle,
reopens, and queries — with the open time recorded, so the startup cost of the
shard layout is a measured number rather than a claim.

## Documentation

- **ADR 0027**, superseding nothing but amending ADR 0014's "Revisit if" on
  on-disk scale, with a back-reference added to 0014's index row.
- A section in `docs/guide/src/storage.md`.
- `docs/evaluation/competitors/zep/parity-gap-matrix.md:29` currently claims ✅
  for "Incremental updates (no full recompute)", justified as *"index layers
  (vector/graph) update incrementally, never rebuild the store"*. True for the
  graph; there is no durable vector index to update incrementally. Correct the
  row.
- CHANGELOG.

## Out of scope

- MCP recall tools (#317) — unblocked by this, not part of it.
- Hybrid BM25 ranking (#199).
- An external vector-store backend (#202); this design leaves `VectorIndex` as
  the seam it plugs into.
- Write-behind or deferred flush. Every `upsert` commits. If a workload makes
  that too expensive, a batching mode is a later change behind the same API.
- Re-sharding an existing index. `shards` is fixed at creation; changing it
  means a new index id and a re-load.

## Revisit if

- Indexes routinely exceed ~100k chunks, at which point the startup rebuild
  stops being cheap and #202 becomes the answer rather than an option.
- Multi-writer indexes become real, at which point OCC retry is not enough and
  the shard map needs genuine merge semantics.
- A trustworthy way to bind a vector to its producing model appears, which would
  turn the declared-space guard into a verified one.

## As built

This section records where the shipped implementation departs from the design
above. Where the two disagree, **this section is correct** — the "Write path"
section above still describes the design as approved, before review found that
it silently lost data. It is left unedited above for the historical record,
not because it is still accurate. See [ADR 0027](../../adr/0027-durable-vector-index.md)
for the full, self-contained account; this section only lists the deltas.

- **The OCC `expected` revision is `last_seen` (the manifest revision this
  handle's in-memory state was built from), not a fresh read taken just before
  the `put`**, as "Write path" step 4 describes. A fresh read only proves the
  store hadn't changed a moment ago; it says nothing about whether the shard
  bytes about to be written were staged from memory that had already fallen
  behind another writer's committed change. Under the fresh-read design, two
  writers committing to the same shard could each have their `put` "succeed"
  against a freshly-read `expected` while one of them silently overwrote the
  other's vectors — no conflict, no error, just a missing vector. Found in
  review, reproduced with a test pinning two handles to one shard, and fixed
  before merge by basing `expected` on `last_seen` instead.
- **On a conflict, the handle reloads every shard whose blob hash differs
  between its own last-known manifest and the winner's, not only the shards
  the losing commit itself touched**, as "Write path" step 5 describes.
  Reloading only the touched shards leaves any other shard the winner changed
  stale in memory while the handle's cached revision advances to the winner's
  — the handle believes itself synced when it isn't. The next write that
  happens to touch that stale shard commits cleanly (no conflict, since the
  cached revision is already current) and silently drops the winner's vectors
  in it. Also found in review, reproduced empirically, and fixed before merge.
- **A conflict-time space/dim/shard-count check against the winning manifest**
  was added; the design's "Space enforcement" section only describes the
  check at `open`. Two handles can each `open` the same *missing* key — there
  is no manifest yet to check either declared value against — and each can
  declare a different embedding space or shard count. Without a second check
  at conflict time, the losing handle would reload the winner's shards under
  its own differing shard count (corrupting the manifest by computing
  different shard ids for the same keys) or write into what is now a
  different declared space, with nothing to catch either.
- **The shard count is `std::num::NonZeroU16` in the API** (`shard_of`,
  `DEFAULT_SHARDS`, `open_with_shards`), not the plain `u16` the design's data
  model implies. The manifest's wire format still stores a plain `u16` (it's
  deserialized data, and a non-zero invariant can't be encoded in a byte
  format), so a stored `0` is instead caught explicitly at `open` and
  surfaced as a corrupt-store error before it would otherwise reach
  `shard_of`'s modulo.
- **Opening reads shard blobs with genuine concurrency**, not the
  chunked-but-sequential loop an earlier version of the code had (which
  chunked reads into batches of 16 — the concurrency constant's name — but
  `await`ed each read inside the chunk in turn). The shipped version spawns
  each batch's reads on a `tokio::task::JoinSet` and joins them, with the
  store held as `Arc<S>` so each task owns a cheap handle. A test
  (`opening_reads_shard_blobs_concurrently`) observes more than one read in
  flight at once, so the concurrency claim is checked rather than assumed —
  the design's "Load path" section describes only the intent, not this
  verification.
- **Deltas are staged on a copy of each dirty shard and applied to
  `self.inner` only after `store.put` reports `Committed`.** The design does
  not call this out explicitly; it matters because it is what keeps memory
  consistent with the store when a `put_blob`/`put` call errors or every
  retry attempt conflicts — the handle never serves a vector the caller was
  told failed to write.
- **A handle-level `commit_lock` (`tokio::sync::Mutex<()>`) serializes
  `commit` calls on the same handle.** Also not called out in the design.
  `upsert`/`remove`/`upsert_many` all take `&self`, so without it two
  concurrent calls on one handle could interleave their reads of `last_seen`
  and each other's staged shard writes, including one call's `Committed` arm
  setting `last_seen` back to an older revision than a commit that actually
  finished after it.
