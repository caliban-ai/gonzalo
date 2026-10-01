# Blob GC grace period — design

**Ticket:** [#325](https://github.com/caliban-ai/gonzalo/issues/325)
**Date:** 2026-09-30
**Status:** approved, ready for an implementation plan

## Context

Blob GC can delete a blob that a writer is about to reference.

Both manifest writers upload blobs *before* committing the manifest that names
them:

- the graph indexer writes slice blobs, then commits the `GraphManifest`
  (`crates/gonzalo-cli/src/lib.rs`);
- `RecordVectorIndex::commit` writes a shard blob, then commits the
  `VectorManifest` (`crates/gonzalo-vector/src/record_index.rs`).

`sweep_blobs` computes its mark set from the records that exist *now*, so a blob
uploaded before the sweep but named by a manifest that commits after it looks
unreferenced and is deleted. The manifest then commits naming a blob that is
gone.

For a graph manifest the damage is recoverable: slices are regenerable from
source, so the next index run rebuilds them. For a vector manifest it is not.
`RecordVectorIndex::open` fails with "shard N names blob … but it is absent",
one missing shard makes the whole index unopenable, and with caller-supplied
embeddings (#317) gonzalo never saw the model, so the vectors cannot be
regenerated.

**This is reachable without operator error today.** `gonzalo index --gc` sweeps
after every debounced re-index under `--watch` (`crates/gonzalo-cli/src/watch.rs`),
so a machine running one watch loop while anything else writes to the same root
races continuously. A single `~/.gonzalo` root commonly holds several views.
ADR 0024's "GC stays explicit and operator-run" describes the one-shot CLI, not
the watch trigger.

### A second path: content-addressed dedup

`put_blob` is write-if-absent on every substrate — FS returns early when the
path exists, S3 sends `If-None-Match: *` and treats the 412 as success — and
neither refreshes the blob's timestamp. So a write that *re-references an old
blob* gets no protection from anything based on blob age. That is not
hypothetical: `upsert x` followed by `remove x` returns a shard to its earlier
content, re-referencing the blob the first write orphaned.

## Decisions

1. **Guarantee: no loss while writers are alive.** A writer that stays up never
   loses a blob it referenced. A writer that *crashes* inside a millisecond
   window can still lose one; closing that needs durable leases, which is
   deliberately out of scope (see "Rejected alternatives").
2. **GC sweeps by blob age.** `list_blobs` reports each blob's modified time and
   the sweep deletes only unreferenced blobs at least `min_age` old.
3. **Writers re-check after committing.** A writer confirms the blobs it newly
   referenced still exist and re-uploads any that are missing.
4. **Breaking the published blob-list surface** in the unreleased 0.8.0, rather
   than adding a parallel route, consistent with ADR 0026's no-deprecation-window
   decision.

### Rejected alternatives

- **Sleeping through the interval** (list, wait, mark, sweep). Needs no
  timestamps, but the interval must exceed a writer's whole *upload phase* — a
  full graph walk or a 256-shard bulk vector load over S3, both minutes — not one
  blob's latency. Every `gonzalo gc` would block for minutes and `--watch` would
  stall on every cycle.
- **Two strikes across runs** (persist candidates; delete only when a later run
  ≥ `min_age` later still finds them unreferenced). No timestamps and no waiting,
  but a single `gonzalo gc` frees nothing on its first run, and GC gains
  persisted state it must keep correct.
- **Durable leases** (a writer records the hashes it is about to reference; GC
  honours unexpired leases). The only option that survives a writer crash, but it
  needs a new record kind, expiry rules and its own ADR. Filed as the heavier
  path this design explicitly does not take.
- **Freshening on re-put** (touch the timestamp when `put_blob` finds the blob
  present), git's answer to the dedup path. Still races: GC can list the blob as
  old, then the writer freshens, then GC deletes on a decision already made.
  Closing it needs a conditional delete, which FS cannot do atomically. The
  writer re-check covers the same case substrate-agnostically.

## The trait and the wire

```rust
/// One stored blob and when it was last written.
pub struct BlobEntry {
    pub hash: ContentHash,
    pub modified: SystemTime,
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    // CHANGED: was `Result<Vec<ContentHash>>`.
    async fn list_blobs(&self) -> Result<Vec<BlobEntry>>;

    /// Whether `hash` is stored. Defaulted, so this is not a breaking addition;
    /// substrates override it with a cheap existence check.
    async fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        Ok(self.get_blob(hash).await?.is_some())
    }

    // unchanged: put_blob, get_blob, delete_blob
}
```

`modified` is required, not `Option<SystemTime>`. An optional age would have to
mean either "never sweep this", which is a silent space leak, or "sweep it
anyway", which is not safe. Every in-tree substrate can answer:

| Substrate | `modified` | `has_blob` |
|---|---|---|
| `FsStore` | file mtime | `try_exists` |
| `S3Store` | `LastModified`, already in the `ListObjectsV2` response it makes | `HeadObject` |
| `ServerStore` | whatever the daemon's backing store reports | defaulted (downloads) — a `HEAD` route is a follow-up |

A `ServerStore` `has_blob` downloads the blob, so the daemon path pays bandwidth
per newly referenced blob. That is accepted here so this change touches the
published surface once rather than twice.

### Daemon wire (breaking, 0.8.0)

- **HTTP.** `GET /v1/blobs` returns `[{"hash": "…", "modified_unix_ms": 1234}]`
  instead of `["…"]`. A JSON array of strings cannot gain a field, and a second
  parallel route would leave the old one as dead weight in the published schema.
- **gRPC.** `ListBlobsResponse` becomes:
  ```proto
  message BlobEntry {
    string hash = 1;
    int64 modified_unix_ms = 2;
  }
  message ListBlobsResponse {
    reserved 1;                       // was `repeated string hashes`
    repeated BlobEntry entries = 2;
  }
  ```
  Field 1 is reserved rather than reused: reusing it with a new type would make
  an old client misparse silently. Reserving it means an old client sees an empty
  list, so an old GC client deletes **nothing** — it fails safe.
- `SERVED_OPERATIONS` keeps the same four blob routes; only the response schema
  of `GET /v1/blobs` changes.

## GC semantics

```rust
pub struct SweepPolicy {
    pub min_age: Duration,
    pub now: SystemTime,
}

pub const DEFAULT_MIN_AGE: Duration = Duration::from_secs(3600);
```

A blob is deleted when **both** hold:

- no record references it (the mark set, unchanged), and
- `now.duration_since(entry.modified) >= min_age`.

`duration_since` errors when `modified` is in the future — the GC host's clock
behind the store's — and that case is treated as **too young**, so the blob is
kept. Errors toward keeping data.

The age filter makes the old mark-then-list ordering irrelevant: a blob uploaded
at any point during a run is young and held back either way. No reordering, and a
smaller diff.

`list_blobs` does not promise uniqueness — today's `sweep_blobs` already dedups
because "listing order and uniqueness are unspecified". With ages attached, a
repeated hash could carry two different times, so a duplicate resolves to the
**newest** `modified`. That errs toward deferring, which keeps data.

### `min_age` defaults to one hour

It must exceed the longest writer upload phase (minutes) and absorb clock skew
between the GC host and the store's clock, which is the one direction that loses
data: a GC host running *ahead* of the store by more than `min_age` sees fresh
blobs as old. `min_age` is therefore also the skew tolerance. Git uses two weeks
for the same mechanism, but git has no writer-side re-check; since we have one,
an hour is a safe margin without hoarding garbage for a fortnight.

### Report

`GcReport` gains a third outcome:

```rust
pub struct GcReport {
    pub freed: Vec<ContentHash>,
    pub retained: usize,   // referenced, kept
    pub deferred: usize,   // unreferenced but younger than min_age
}
```

The three partition the distinct listed blobs: `freed + retained + deferred ==
distinct`. Today `retained` is computed as `distinct - freed`; that arithmetic
must be restated, not extended. Without `deferred`, an operator cannot tell
"nothing to reclaim" from "reclaiming held back", which is the first question
this change will produce.

### API surface

- `gc_blobs(store)` keeps its signature and uses `DEFAULT_MIN_AGE` with
  `SystemTime::now()`, so a facade caller is safe without opting in.
- `gc_blobs_with(store, policy)` and `sweep_blobs_with(blobs, live, policy)` take
  an explicit policy. Existing tests that assert an immediate sweep move to the
  `_with` form with `min_age: Duration::ZERO`; any that are not migrated fail
  loudly rather than quietly.
- CLI: `gonzalo gc [--min-age <duration>]`, parsed with the existing
  `parse_duration` and echoed in the operator's own spelling, as
  `collect --horizon` already does via `Horizon { raw, duration }`. The gc
  summary prints the deferred count. `gonzalo index --gc` and `--watch --gc`
  inherit the default, so the continuous sweeps that make this bug reachable are
  covered with no flag.

## The writer-side re-check

After a manifest commit succeeds, the writer asks `has_blob` for each hash it
**newly** referenced and re-uploads anything missing. Unchanged entries are not
checked: they were referenced by both the previous manifest and the new one, so
GC never saw them as garbage.

This is what closes the dedup path. The age filter protects freshly uploaded
blobs; it cannot protect an old blob that a write newly references.

**Vector index.** In `commit()`'s `Committed` arm, after `apply` and the
`last_seen` update. No staged bytes are retained — holding them for a 256-shard
batch would add roughly 150 MB of peak memory. After `apply`, the in-memory shard
*is* the committed content, so a missing blob is re-encoded from memory on
demand, for that blob only. The re-encode is idempotent with respect to the
deltas (an upsert sets the same value, a remove of an absent key is a no-op), and
the re-encoded bytes must hash to the manifest's entry — a mismatch is a
corrupt-state error, not a silent re-put.

The helper is separate and the call inside `commit()` is one line. That function
has had two independent reviews on the most capable model after losing data
twice; it does not get restructured here.

**Graph indexer.** After the manifest commits, check the hashes for
`recon.added` and `recon.modified`, re-uploading from the `Slice` still held in
`staging.inserts` — `staging` outlives the commit (it is consumed by
`staging.apply(&mut graph)` afterwards), and `Slice::to_slice_bytes()`
regenerates the bytes.

**When repair fails**, the writer returns an error even though the commit
succeeded, naming the shard or path and the hash and saying that the manifest
committed but a blob is missing. Returning `Ok` would leave an unopenable index
and nothing to explain why.

## Known limits

- **A writer that crashes between its commit and its re-check**, having newly
  referenced an old blob that a concurrent GC sweeps in that window, still loses
  it. Durable leases are the only fix and are out of scope.
- **Clock skew beyond `min_age`** in the losing direction (GC host ahead of the
  store) defeats the age filter. The writer re-check still covers a live writer.
- **A `ServerStore` `has_blob` downloads the blob.** Correct, but costs
  bandwidth per newly referenced blob on the daemon path.
- **The #198 drift check compares paths and methods, not response schemas**, so
  it will not catch the changed `GET /v1/blobs` response. That edit to
  `docs/api/openapi.json` is hand-made and unguarded by a test.

## Testing

Red-first wherever a test states a real claim. Neither race needs a sleep or a
genuine GC race to reproduce.

| Test | What it proves |
|---|---|
| Young unreferenced blob is not swept | The primary race. Red first: today's sweep deletes it |
| …and is counted in `deferred` | The operator can tell held-back from nothing-to-do |
| Old unreferenced blob is swept | The grace period did not simply disable GC |
| Referenced blob is retained whatever its age | The mark set still dominates |
| Future-dated blob is deferred, not freed | Skew errors toward keeping data |
| Vector: manifest `put` deletes the shard blob first, via a test store wrapper | The re-check restores it and the reopen succeeds. Red first: reopen fails with the absent-blob error |
| Graph: helper restores a slice blob deleted behind its back | Same, at the helper |
| `freed + retained + deferred == distinct` | The restated arithmetic |
| Conformance: `list_blobs` reports a `modified` near the put | Every substrate — catches an epoch or a client-side `now` |
| Conformance: `has_blob` true after put, false for an unknown hash | Every substrate |
| HTTP `GET /v1/blobs` carries `modified_unix_ms`; gRPC populates `entries` | The wire change |
| `--min-age` parsing, and the summary printing `deferred` | The operator surface |

The graph repair is tested **at the helper, not through `index`**: `index`
constructs its own `FsStore` from a path, so no wrapper can be injected without a
larger refactor. The helper takes a store, so a test puts a blob, deletes it,
calls the helper, and asserts restoration — deterministic, no wrapper.

The vector test uses a wrapper because `RecordVectorIndex` is generic over its
store and that file already has `CountingStore` and `FailingBlobStore`
precedents.

Four test fakes need the new `list_blobs` signature: `CountingStore` and
`FailingBlobStore` (`gonzalo-vector`), `Mem` (`gonzalo-core::ancestry`), and
`FakeBlobs` (`gonzalo-core::gc`).

## Documentation

- **ADR 0028**, amending **ADR 0024** (blob GC gains an age rule) and **ADR 0027**
  (whose "do not run `gonzalo gc` while vector writes are in flight" consequence
  is now addressed for live writers). Both annotated on both sides; both stay
  `accepted`.
- `docs/guide/src/deletion.md` — the age rule, `--min-age`, the deferred count.
- `docs/guide/src/storage.md` — replace the don't-run-GC warning with the
  residual crash window.
- ADR 0027's negative consequence points at 0028 instead of telling operators to
  avoid GC.
- `CHANGELOG.md` — both breaking changes (`list_blobs`; the `GET /v1/blobs`
  response and the gRPC field), plus `has_blob`, `--min-age` and `deferred`.
- `docs/api/openapi.json` and the `.proto` updated.

## Out of scope

- Durable leases, and therefore the writer-crash window.
- A daemon `HEAD /v1/blobs/{hash}` route to make `ServerStore::has_blob` cheap.
- Freshening a blob's timestamp on re-put.
- Whether a deleted `VectorManifest`'s tombstone should pin its shards (#325's
  "related" note) — a separate decision about tombstone semantics.

## Revisit if

- A writer's upload phase can exceed an hour, making the default `min_age` too
  short and the re-check the only line of defence.
- The writer-crash window is observed in practice, which would justify leases.
- GC becomes something a daemon runs continuously rather than a CLI trigger, at
  which point the `ServerStore` cost and the skew question both get sharper.
