# ADR 0028 · A grace period for blob garbage collection

- **Status:** accepted
- **Date:** 2026-09-30
- **Source:** [`docs/superpowers/plans/2026-09-30-blob-gc-grace-period.md`](../superpowers/plans/2026-09-30-blob-gc-grace-period.md)
- **Amends:** [ADR 0024](0024-blob-garbage-collection.md), whose sweep deleted
  every unreferenced blob regardless of age; this ADR adds an age rule to it.
  Its marking rules (what counts as live) are unchanged.
- **Amends:** [ADR 0027](0027-durable-vector-index.md), whose consequence "GC is
  not safe during a commit" is now addressed for live writers. What remains of
  that consequence is a writer that crashes mid-commit, stated below.

Neither ADR is superseded; both stay `accepted`.

## Context

Both manifest writers upload blobs **before** they commit the record that names
them: the code-graph indexer (`crates/gonzalo-cli/src/lib.rs`) writes slice
blobs and then the graph manifest, and `RecordVectorIndex::commit`
(`crates/gonzalo-vector/src/record_index.rs`) writes shard blobs and then the
vector manifest. The sweep marked blobs against the records that exist *now*.
So a blob uploaded before the sweep but named by a manifest that commits after
it looked unreferenced and was deleted, and the manifest then committed naming
a blob that no longer existed.

The damage differs by writer. A missing graph slice is recoverable, because
slices regenerate by re-parsing source. A missing vector shard is not:
`RecordVectorIndex::open` fails with "shard N names blob … but it is absent",
so one missing shard makes the whole index unopenable, and with
caller-supplied embeddings (#317) gonzalo never saw the model and cannot
regenerate the vectors.

This is reachable with no operator involved. `gonzalo index --gc` sweeps after
every debounced re-index under `--watch` (`crates/gonzalo-cli/src/watch.rs`),
so a GC runs on a timer driven by file edits while a writer may be mid-commit.
ADR 0024's statement that GC "stays explicit and operator-run" describes the
one-shot `gonzalo gc` command, not that trigger.

There is a second path, through content addressing. `put_blob` is
write-if-absent on every substrate and refreshes no timestamp, so a write that
re-references an **old** blob gets no protection from any age rule. For
example, `upsert x` followed by `remove x` returns a shard to its earlier
content, re-referencing the blob that the first write orphaned. That blob is
old, unreferenced, and about to be named again.

## Decision

**A sweep deletes a blob only when it is unreferenced and at least `min_age`
old, and each writer re-checks the old blobs it newly referenced after its
manifest commits.** The two halves cover the two paths above: age protects a
freshly uploaded blob, and the re-check protects an old blob that a write has
just referenced again.

### Age rule

- `BlobStore::list_blobs` returns `Vec<BlobEntry>` instead of hashes, each
  entry carrying the blob's modified time. This is a breaking change to the
  trait and to the daemon wire (below).
- `min_age` defaults to one hour (`DEFAULT_MIN_AGE`,
  `crates/gonzalo-core/src/gc.rs`). `gc_blobs(store)` and
  `sweep_blobs(blobs, live)` keep their signatures and use that default, so the
  safe behaviour is the default behaviour. `gc_blobs_with` and
  `sweep_blobs_with` take an explicit `SweepPolicy`. `now` is a field of the
  policy rather than a clock read, so the rule is testable without backdating
  files. `gonzalo gc --min-age <duration>` exposes it, parsed by the same
  `parse_duration` as `collect --horizon`; `gonzalo index --gc`, including
  under `--watch`, always uses the default.
- `BlobEntry` stores `modified_unix_ms: i64`, not a `SystemTime`
  (`crates/gonzalo-core/src/store.rs`), with `from_system_time` and
  `age(&self, now) -> Option<Duration>`. The type crosses the daemon's HTTP and
  gRPC surfaces, where milliseconds are the wire form, so one type serves core,
  client and server instead of three near-identical ones, and age arithmetic
  stays in integers.
- **Clock disagreements an operator can plausibly hit err toward keeping
  data.** `age` returns `None` for a future-dated blob (the GC host's clock is
  behind the store's) and when the subtraction cannot be represented, and the
  sweep treats every `None` as too young. A blob's own timestamp that overflows
  saturates to `i64::MAX`, which reads as future and hence too young. Two
  pathological cases go the other way: a blob timestamped before the Unix epoch
  maps to a negative value that reads as very old and collectable, and a `now`
  that overflows saturates to `i64::MAX`, which also reads as very old. Neither
  is reachable with a sane clock, but they are exceptions, not kept data.
- **A hash listed more than once resolves to its newest timestamp**, in a
  separate pass before any decision is made, so a stale duplicate cannot make a
  young blob collectable.
- `GcReport` has three outcomes, `freed`, `retained` and `deferred` (an
  unreferenced blob held back for being too young), counted in one loop so the
  partition is structural rather than derived by subtraction. `freed` now comes
  back in `ContentHash` order, not listing order, because the sweep walks a
  `BTreeMap`. The CLI prints `deferred` from both `gonzalo gc` and the
  `index --gc` path, so an operator can tell why a sweep reclaimed nothing.

### Writers re-check after committing

The age rule cannot protect an old blob that was just referenced again, so each
writer asks the store whether the blobs it **newly** referenced are still
present, and re-uploads any that are missing.

- Vector: `RecordVectorIndex::verify_shard_blobs`, called from `commit` once the
  manifest `put` reports `Committed`. No staged bytes are retained (a 256-shard
  batch would hold about 150 MB); the shard is re-encoded on demand from
  memory, and a re-encode that hashes differently from what the manifest names
  is a corrupt-state error.
- Graph: `ensure_slices_present` (`crates/gonzalo-cli/src/lib.rs`), called after
  the manifest commits and before staging consumes the slice values. It checks
  only added and modified paths, since an unchanged path was referenced by both
  the old and new manifest and so was never exposed.
- The check uses `BlobStore::has_blob`, a defaulted trait method
  (`get_blob(..).is_some()`), overridden by the filesystem store with
  `try_exists` and by S3 with `HeadObject`. A HEAD failure that is not "no such
  key" returns `Err`, never `false`: `false` would make a writer re-upload on
  every transient fault and hide a broken store. `ServerStore` keeps the
  default, so over the daemon the check downloads the blob.

### Daemon wire (breaking, unreleased 0.8.0)

HTTP `GET /v1/blobs` returns `[{"hash": …, "modified_unix_ms": …}]` instead of
`["…"]`. gRPC `ListBlobsResponse` **reserves field 1** and adds
`repeated BlobEntry entries = 2`. Reserving rather than reusing the field means
an old client decodes an empty list and so an old GC client deletes nothing: it
fails safe. `docs/api/openapi.json` gains a `BlobEntry` schema.

### Why one hour

The window to defend is a writer's upload phase, which is minutes. `min_age`
must exceed that, and it must also absorb clock skew between the GC host and
the store. It is both at once, so the skew tolerance *is* `min_age`: a GC host
running **ahead** of the store is the direction that loses data, because it
makes a fresh blob look old. Git's `gc.pruneExpire` uses two weeks for the same
mechanism, but git has no writer re-check and so needs the margin to carry the
whole guarantee. Here the re-check carries the old-blob case, and one hour is
enough margin for upload and ordinary skew without leaving garbage for days.

### Alternatives rejected

- **Sleep through the interval inside the sweep.** Every `gc` would block for
  minutes and `--watch` would stall behind it.
- **Two strikes across runs** (delete only what two consecutive sweeps found
  unreferenced). A first `gc` would free nothing, and GC would gain persisted
  state to carry the first strike between runs.
- **Durable leases** (writers record intent before uploading). This is the only
  option that survives a writer crash, but it is a new record kind with its own
  lifecycle and expiry, and deserves its own ADR rather than riding on this
  one. It is the named revisit trigger below.
- **Refresh the timestamp when a blob is re-put.** This still races: GC can list
  a blob as old, the writer then refreshes it, and GC deletes on a decision it
  already made. Closing that needs a conditional delete ("delete only if still
  this old"), which the filesystem substrate cannot do atomically.

## Consequences

- **Positive:** the race is narrowed for live writers on every substrate: a
  blob a writer is about to reference is no longer exposed for the whole
  upload-to-commit interval, only for the much shorter interval described in
  the second negative consequence below.
  `gonzalo index --gc` under `--watch` is covered with no flag, because the
  production callers pass `DEFAULT_MIN_AGE`. `deferred` tells an operator why a
  sweep reclaimed nothing, instead of leaving "nothing to free" and "everything
  too young" indistinguishable.
- **Negative:** **a writer that crashes between its manifest commit and its
  re-check can still lose a newly referenced old blob.** This design does not
  close that window. A sweep that lands in it deletes the blob, the process
  dies before re-uploading, and for a vector shard the index is left
  unopenable. Durable leases are the only fix, and they are out of scope here.
- **Negative:** **a live writer can still lose a blob to a sweep that has
  already decided to delete it.** GC computes its mark set, lists blobs,
  decides an old blob is unreferenced and collectable, and then deletes. If a
  writer commits a manifest naming that blob after the decision but before the
  delete, the writer's re-check sees the blob still present and does nothing,
  and GC then deletes it, leaving a manifest that names a missing blob. No
  crash is involved. This is the same flaw as the one that rules out freshening
  on re-put: GC deletes on a decision it already made. The re-check narrows the
  exposure from the whole upload-to-commit interval to GC's own mark-to-delete
  interval; it does not remove it. Closing it entirely needs durable leases, or
  a GC that re-marks immediately before each delete, and neither is part of
  this change.
- **Negative:** **clock skew beyond `min_age`**, in the losing direction (GC host
  *ahead* of the store), defeats the age filter. `min_age` is the skew
  tolerance; it is not a separate setting, and lowering it for faster
  reclamation lowers the tolerance with it.
- **Negative:** **unreferenced blobs now linger for at least `min_age`** before
  they can be reclaimed, so disk is freed later than before and a `gc` right
  after a `delete` and `collect` may report `deferred` rather than `freed`.
- **Negative:** **`ServerStore::has_blob` downloads the blob**, so the daemon
  path pays bandwidth for each newly referenced blob. A `HEAD` route is a
  follow-up.
- **Negative:** **#198's drift check compares paths and methods, not response
  schemas**, so the changed `GET /v1/blobs` response in `docs/api/openapi.json`
  is not guarded by a test. What pins it is the `http.rs` test that reads the
  response body as untyped JSON; a future schema change here can drift from the
  document without CI noticing.
- **Negative:** **S3's `has_blob` and the S3 conformance cases are exercised
  only by CI's HA soak job**, which sets `GONZALO_S3_TEST_REQUIRED=1`. They are
  skipped locally, so a local green run says nothing about them.
- **Negative:** `list_blobs` returning entries is a breaking change for any
  out-of-tree `BlobStore` implementor, and for any client of `GET /v1/blobs`.
  A gRPC client built against the old schema sees an empty list, so it fails
  safe, but an old HTTP client sees objects where it expected strings.

## Revisit if

- A writer's upload phase can exceed an hour, which makes the default
  `min_age` too short and leaves the re-check as the only defence.
- The writer-crash window is observed in practice, which would justify
  durable leases.
- GC becomes something a daemon runs continuously rather than a CLI trigger, at
  which point the `ServerStore` download cost and the clock-skew question both
  sharpen.
