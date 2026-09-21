# ADR 0027 · A durable vector index

- **Status:** accepted
- **Date:** 2026-09-20
- **Source:** [`docs/superpowers/specs/2026-09-20-durable-vector-index-design.md`](../superpowers/specs/2026-09-20-durable-vector-index-design.md)
  (see that spec's "As built" section for where the shipped code departs from
  it)
- **Amends:** [ADR 0014](0014-approximate-vector-index-backend.md), whose
  "Revisit if" named "on-disk or distributed scale" as a future trigger. This
  ADR is that revisit: it does not change the backend 0014 chose
  (`MemoryVectorIndex` exact, `HnswVectorIndex` approximate) or supersede it —
  it gives either one a durable home behind the same `VectorIndex` trait.

## Context

Before this change `gonzalo-vector`'s indexes had no persistence and, outside
their own unit tests, no callers at all: `MemoryVectorIndex`
(`crates/gonzalo-vector/src/index.rs`) is a `Mutex<HashMap<RecordKey, Vec<f32>>>`
with no save, load, or `Serialize` implementation anywhere in the crate, every
construction site of an index or a `KnowledgeStore` in the workspace sat inside
a `#[cfg(test)]` module, and neither `gonzalo-cli` nor `gonzalo-server`
depended on `gonzalo-vector`. Losing the process meant losing every vector.

That was tolerable while embeddings could always be regenerated: recall from
lost vectors, restart the embedder, re-ingest. #317 removed that option by
choosing **caller-supplied vectors** for MCP recall — the embedder lives on the
caller's side of the boundary, gonzalo never runs it, so an index that only
exists in memory can never be rebuilt. Persistence stopped being an
optimization and became a prerequisite for #317 and for #199's hybrid
retrieval.

ADR 0014 had already anticipated this and named it explicitly, under "Revisit
if": *"we need on-disk or distributed scale"*. This ADR is that revisit.

## Decision

**Vectors are bucketed into a fixed number of content-addressed shard blobs,
named by one `VectorManifest` record, and `VectorIndex` remains the only seam
a caller writes or queries against.**

### Record kind and merge class

`RecordKind::VectorManifest` is a new record body,
`VectorManifest { space, dim, shards: u16, entries: BTreeMap<u16, ContentHash> }`
(`crates/gonzalo-core/src/vector_manifest.rs`). It lives in `gonzalo-core`
rather than `gonzalo-vector` because `gonzalo gc`'s mark-set builder
(`crates/gonzalo-core/src/gc.rs`) must be able to parse every manifest kind,
and core cannot depend upward on a capability layer (ADR 0008).

Its merge class is `MergeClass::Opaque`, not `Derived`. `GraphManifest` is
`Derived` because a graph manifest can always be rebuilt by re-parsing source —
if two writers diverge, gonzalo can just recompute the right answer. A vector
manifest cannot be rebuilt this way: gonzalo never sees the embedding model,
only the vectors a caller hands it, so treating it as `Derived` would let a
divergence be silently discarded and the corresponding vectors permanently
lost. `Structured` (field-level merge) fails the same way from a different
angle: two writers touching the same shard produce two different blob hashes
for that field, and a structured merge keeps only one, again losing a writer's
vectors without any signal that it happened. `Opaque` is the only class that
turns a divergence into an explicit, surfaced conflict instead of a silent
loss.

### Sharded blobs

A vector's shard is `shard_of(key, shards)`
(`crates/gonzalo-vector/src/shard.rs`): blake3 of `"<namespace>/<collection>/<id>"`
via `ContentHash`, first 4 hex digits parsed as a `u16`, modulo the shard
count. Blake3 is used instead of `std::hash::DefaultHasher` because the
standard library explicitly does not guarantee `DefaultHasher`'s output is
stable across releases — a drift there would silently strand every vector in
every existing index behind the wrong shard id. The default shard count is
256.

Each shard is one blob, written with a small binary format: 4-byte magic
`GZVS`, a version byte, little-endian lengths, and entries sorted by key before
encoding. Sorting means identical shard contents always produce identical
bytes, so an unchanged shard content-addresses to the blob already stored and
a write that touches other shards costs nothing extra for this one.

The shard count is `std::num::NonZeroU16` everywhere in the API, so a caller
cannot construct a zero-shard index — dividing by it would panic. The manifest
itself still stores a plain `u16`, because it is deserialized data and a
non-zero invariant can't be encoded in the wire format; a stored `0` is instead
caught explicitly at `open` and surfaced as a corrupt-store error rather than
reaching `shard_of`'s modulo. Decoding a shard bounds every allocation against
the bytes actually remaining in the input, so a corrupt or hostile length field
produces an ordinary error instead of an allocator abort.

### No new persistence trait

`RecordVectorIndex<S>` (`crates/gonzalo-vector/src/record_index.rs`)
implements the existing `VectorIndex` trait rather than introducing a
persistence-specific one. `VectorIndex` is already the seam an external vector
backend (#202) is meant to plug into, and a second trait underneath it would
have had exactly one real implementation. `VectorIndex` gained two methods:
`upsert_many` (defaulted to loop over `upsert`, so no existing implementor
breaks) and `keys` (required, with no default, because there is no way to
enumerate an arbitrary index's contents without one — this is a breaking
change for any out-of-tree `VectorIndex` implementor). `impl VectorIndex for
Arc<T>` forwards every method, including `upsert_many` explicitly rather than
inheriting the default — a shared `Arc<RecordVectorIndex<S>>` handed to
multiple callers must still batch a bulk load into one manifest commit, not
silently fall back to one commit per vector.

### The embedding-space tag

`RecordVectorIndex::open(store, key, space, dim)` declares an embedding space
and dimension. Opening against an existing manifest whose stored `space`,
`dim`, or shard count differs is an error naming both the stored and the
declared value — a swapped embedder is caught once, loudly, at startup, rather
than silently producing wrong-but-plausible rankings on every later query.

That check alone is not enough, because it only runs when a manifest already
exists. Two handles can each `open` the *same key* while it is still missing —
there is nothing yet to check either declared value against — and each can
declare a different space or shard count. If only one of them ever commits,
the other never has cause to look. So the same check also runs on every OCC
conflict, against the *winning* manifest, before anything from it is reloaded
into memory: the losing handle must discover the disagreement and refuse to
proceed, rather than reload the winner's shards under its own (different)
shard count — which would compute different shard ids for the same keys and
corrupt the manifest — or write its own vectors into what is now a different
declared space.

The limit is stated plainly rather than implied: gonzalo can verify that the
space a caller **declares** at `open` matches the space recorded in the
manifest. It cannot verify that a vector actually came from that model, because
with caller-supplied embeddings gonzalo never runs the model or sees anything
but the floats. Dimension is the only intrinsic check available, and it is a
weak one — all-MiniLM-L6-v2 and bge-small both produce 384-dimensional
vectors, so a dimension match proves nothing about which of the two produced a
given vector.

### The write path

Every `upsert`, `remove`, and `upsert_many` funnels through one `commit`
method that stages dirty shards, writes their blobs, and then `put`s an
updated manifest under OCC, retrying up to 5 times before giving up with an
error naming the index key.

**The OCC `expected` revision is the manifest revision this handle's
in-memory state was actually built from — a cached `last_seen` — never a fresh
read taken immediately before the `put`.** This is the least obvious rule in
the whole design, and it exists because the more natural-looking alternative
loses data silently. A fresh read taken right before the write only proves
that the store hadn't changed a moment ago; it says nothing about whether the
shard bytes being written were computed from state that had *already* fallen
behind. Concretely: if writer A commits first, and writer B then takes a fresh
read of the manifest revision as its `expected`, B's `expected` matches the
store's true current state — because B just read it — even though the shard
bytes B is about to write were staged from B's *old*, pre-A in-memory copy of
that shard. The `put` succeeds, because `expected` matches, and B's stale
shard content overwrites A's committed vectors with no conflict ever reported.
Basing `expected` on `last_seen` instead closes this: `last_seen` is the
revision that B's in-memory shard content actually reflects, so if the store
has moved past it, `put` reports a real, unavoidable conflict, and B is forced
to reload before it can write.

**On a conflict, the handle reloads every shard whose blob hash differs
between its own last-known manifest and the winner's — not only the shards
this commit touches.** This is the second non-obvious rule, and it exists for
the same reason as the first: reloading only the touched shards looks correct
and loses data anyway. Suppose the winner changed a shard this commit never
touched (because a *different* earlier commit changed it). Reloading only the
touched shards leaves that other shard stale in memory while `last_seen`
advances to the winner's revision — the handle now believes itself fully
synced when it is not. The next write that happens to touch that stale shard
stages from the stale in-memory copy, produces a `put` whose `expected`
matches (because `last_seen` says it's current), and commits cleanly —
silently deleting the winner's vectors in that shard, with nothing to catch
it because no conflict occurred. Diffing every shard's blob hash between the
old and new manifest, not just the touched set, is what prevents this. The
shard(s) this commit itself touches are always included in the reload too,
even when their content happens not to have changed, because it costs nothing
extra and removes a case from the reasoning.

Two more properties round out the write path. First, **deltas are staged on a
copy of each dirty shard's current in-memory content and applied to
`self.inner` only after `store.put` reports `Committed`** — memory never
reflects a write that the caller was told failed or that lost a race and is
retrying, so a `put_blob`/`put` error or an exhausted retry budget leaves the
handle serving exactly what the store holds, nothing more. Second, a
handle-level `commit_lock` (a `tokio::sync::Mutex<()>`) serializes `commit`
calls made on the *same* handle, because `upsert`/`remove`/`upsert_many` all
take `&self` — without it, two concurrent calls on one handle could interleave
their reads of `last_seen` and each other's staged shard writes.

Both of these rules — basing `expected` on `last_seen`, and reloading every
differing shard rather than only the touched ones — were found in code review
after the original design (a fresh read for `expected`, reload-only-dirty on
conflict) was implemented, reproduced empirically as real data loss, and fixed
before merge. They are the parts of this design most likely to look like
unnecessary complexity to a future refactor; both are load-bearing.

Shard blobs orphaned by a lost race (the loser's shard write that never made
it into a winning manifest) are left unreferenced for `gonzalo gc` to reclaim,
exactly as the code-graph indexer already does for its own orphaned slices.
`upsert_many` validates every vector's dimension before staging anything, then
commits the whole batch as a single manifest revision — a 100k-vector bulk
load is one commit, not 100k.

### Opening is genuinely concurrent

`RecordVectorIndex::open` fetches shard blobs in batches of 16 concurrent
reads via `tokio::task::JoinSet`, decoding and applying each batch's results
to memory sequentially once the batch's reads all land. The store is wrapped
in `Arc<S>` so each spawned read task gets a cheap, independently-owned
handle — `FsStore` is not `Clone`, and a task needs to own what it reads for
its own lifetime. An earlier version of this code chunked reads into batches
of 16 but `await`ed each one in turn inside the chunk — sequential I/O
wearing a concurrency constant's name. A test
(`opening_reads_shard_blobs_concurrently`,
`crates/gonzalo-vector/src/record_index.rs`) now observes more than one read
in flight at once, so the concurrency claim is checked rather than assumed.

### Garbage collection

`live_blob_hashes` (`crates/gonzalo-core/src/gc.rs`) gained a
`VectorManifest` arm that marks every blob hash in `entries` as live. Without
it, the first `gonzalo gc` run after a vector index existed would delete every
shard blob while the manifest still pointed at them — an unrecoverable loss,
since caller-supplied vectors cannot be regenerated the way a graph manifest
can.

### Chunk counts across a restart

`KnowledgeStore::open(store, index, embedder)`
(`crates/gonzalo-knowledge/src/lib.rs`) rebuilds the per-record chunk counts
that drive orphan-chunk eviction (#150) from a durable index at construction,
scanning it once via `keys`. Without this, a re-ingest of a shrunk record
after a process restart would leave its high-ordinal chunks behind forever,
because `chunk_counts` lived only in memory and reset to empty on every
restart. `KnowledgeStore::new` is unchanged, because a fresh in-memory index
correctly starts with empty counts.

## Consequences

**Positive:**

- Recall survives the process that built it — the entire point of the ticket.
  An acceptance test (`crates/gonzalo-vector/tests/durability.rs`) writes
  10,000 vectors, drops the handle, reopens, and queries. Measured reopen
  time: **258 ms in a release build, 3.43 s in debug**, at **dimension 16**.
  A real embedding is 384-dimensional — 24× the bytes per vector — so this
  proves the path works at realistic *count* and understates its cost at
  realistic *width*; it is not the number to quote for opening a real
  10,000-chunk index.
- `gonzalo gc`'s mark set already covers vector shard blobs, so the existing
  GC path needs no separate opt-in or follow-up to be safe against this
  change.
- An external vector backend (#202) still plugs in at the unchanged
  `VectorIndex` trait, without anything in this design standing in its way.

**Negative:**

- **One writer per index.** OCC detects a second writer and retries; it never
  merges. A second writer's in-memory view of shards it hasn't touched can be
  stale until it reopens for *reads* — writes are now safe against this, per
  the write-path rules above, but a query can still be served from a
  not-yet-refreshed shard.
- **The whole index is held in memory once opened.** There is no partial or
  paged load; opening an index means every one of its vectors lives in the
  process's memory for as long as the handle does.
- **`gonzalo-vector` now requires a tokio runtime.**
  `RecordVectorIndex::open` uses `JoinSet::spawn`, which panics outside a
  tokio runtime. `tokio` (feature `rt`) moved from a dev-dependency to a real
  one. Every gonzalo binary already runs on tokio, so nothing breaks today —
  but a future non-tokio consumer of `gonzalo-vector` would break on `open`.
- **The space tag records what a caller declared, not what is true.** It
  catches configuration drift — a swapped embedder, a restored index, two
  services disagreeing — but a caller that mislabels its vectors is
  undetectable, and dimension alone is too weak a check to catch most
  mislabelings (see "The embedding-space tag" above).
- **`keys` is a required trait method**, so this is a breaking `VectorIndex`
  change for any out-of-tree implementor — there was no way to add it as a
  default that means anything for an arbitrary index.
- `upsert_many(vec![])` against an already-existing manifest still performs a
  real commit that advances the revision counter with byte-identical shard
  content, rather than being a no-op.

## Revisit if

- Indexes routinely exceed roughly 100k chunks, at which point the
  whole-index-in-memory model and the full startup rebuild stop being cheap
  and #202 (an external backend behind the same `VectorIndex` seam) becomes
  the answer rather than an option.
- Multi-writer indexes become a real requirement — OCC-and-retry is not
  enough on its own, and the shard map would need genuine merge semantics.
- A trustworthy way to bind a vector to the model that produced it appears,
  which would let the declared space tag become a verified one.
- A non-tokio consumer of `gonzalo-vector` appears, which would force
  `open`'s concurrency to be built some other way.
