# Replicated deletion: tombstones, collection, and namespace reset

- **Ticket:** gonzalo#203 (namespace reset), which pulls in the deferred half of
  ADR 0018 (replicated deletion)
- **Date:** 2026-09-13
- **Status:** Proposed
- **Refs:** `crates/gonzalo-core/src/{store,record,revision,sync,conformance}.rs`,
  `crates/gonzalo-store-{fs,git,s3,server}/src/lib.rs`,
  `crates/gonzalo-server/src/{http,grpc,service}.rs`,
  `crates/gonzalo-proto/proto/gonzalo.proto`, `crates/gonzalo-soak/`,
  ADR 0006 (conformance suite), ADR 0015 (namespace-scoped auth), ADR 0016
  (three-way merge ancestry), ADR 0017 (non-fast-forward pull), ADR 0018
  (record deletion, local-only).
- **Decision brief:** the options comparison for #203 (local-only reset vs.
  tombstones vs. hybrid). Option B, tombstones, was chosen. The reasoning is
  restated below so this spec stands on its own.

## 1. Problem

### 1.1 What deletion does today

`Store::delete(key, expected)` (ADR 0018) **physically removes** a record. The
fs store unlinks the file, the git store commits the removal, and s3 issues a
conditional `DeleteObject`. Deletes are OCC-aware in the same way `put` is: a
stale `expected` revision returns `DeleteResult::Conflict`.

ADR 0018 deliberately made deletion **local-only**. Gonzalo replicates records
between substrates in two ways:

- **`sync(a, b)`** (`gonzalo-core/src/sync.rs`) takes the union of both sides'
  keys, copies records that exist on only one side, and merges records that
  exist on both with different revisions.
- **git pull** (`gonzalo-store-git`, ADR 0017) runs a three-way merge against
  git's real merge base.

A physical delete leaves no trace. When sync later meets a peer that still holds
the record, "deleted here" and "never existed here" look the same, so sync
copies the record back. **A deleted record resurrects on the next sync.** ADR
0018 documents this as a known sharp edge and names tombstones as the fix when
a consumer needs deletes to stick.

### 1.2 Why #203 forces the issue

#203 asks for a first-class **namespace reset**: clear a namespace (or a
collection within it) in one call, with the same semantics on every substrate.
A reset built on local-only delete inherits the resurrection problem at scale.
You reset a namespace on your laptop, sync with the team daemon, and every
record comes back.

Gonzalo is designed to scale from a single local directory up to replicated
daemons over shared object storage. A reset that only works when you never
sync doesn't meet that goal. So this spec designs **replicated deletion** first
and builds reset on top of it.

### 1.3 Scope

This spec covers all of the following, which are designed and shipped
together:

1. **Tombstones:** deletion becomes a replicated write.
2. **Collection:** an explicit, operator-run way to physically remove old
   tombstones.
3. **Namespace/collection reset:** bulk deletion built on tombstones.

These are **out of scope** and get their own tickets:

- Garbage collection of orphaned out-of-line blobs (see §8.3).
- Automatic or background collection (explicitly rejected, see §5.2).
- Causality metadata that is exact under every topology (version vectors,
  rejected in §3.4).
- Populating `Meta.created` / `Meta.updated`, which are always 0 today.

## 2. Vocabulary

- **Live record:** a normal record with content.
- **Tombstone:** a record whose `kind` is `RecordKind::Tombstone`. It marks a
  key as deleted, has no content, and replicates like any other write.
- **Consumer read:** `get` / `list`, used by applications (CLI, MCP, memory
  tiers, tickets). **Tombstones are hidden**, so a deleted key looks absent.
- **Raw read:** `get_raw` / `list_raw`, used only by replication (sync, pull,
  collection). **Tombstones are visible.**
- **Revision chain:** the sequence of revisions a key has been through. Today a
  record carries only its current `revision` and one `parent`. This spec adds a
  bounded list of recent `ancestors` so sync can tell "this side is behind"
  apart from "the two sides diverged".
- **Recreation:** writing a live record to a key whose current state is a
  tombstone.
- **Purge:** physically removing a tombstone. This is the only physical removal
  left in the system.
- **Collection:** purging tombstones older than a horizon the operator chooses.
- **Horizon:** the minimum tombstone age collection will purge. It must be
  longer than the longest time any peer might go without syncing (see §8.2).

## 3. Design

### 3.1 The tombstone record

A tombstone is an ordinary `Record` stored at the key's normal path
(`<ns>/<col>/<id>.json` on fs and s3; the same path in the git tree). No new
storage layout or side index is needed.

```rust
pub enum RecordKind {
    // existing variants unchanged
    /// A deletion marker. Hidden from consumer reads; replicated by sync and
    /// pull; physically removed only by `purge`.
    Tombstone,
}

pub struct Record {
    // existing fields unchanged
    /// Recent revisions this record descends from, newest first, bounded by
    /// the store's ancestor cap. Advisory: a missing or truncated list makes
    /// sync report a conflict instead of guessing. Empty on legacy records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<Revision>,
    /// Tombstones only: when the delete happened, in ms since the Unix epoch.
    /// Stamped by the store. `None` means collection never purges it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
}
```

A tombstone written by `delete` over a live record `cur` has these fields:

| Field | Value |
|---|---|
| `kind` | `RecordKind::Tombstone` |
| `body` | `Body::Inline(vec![])` |
| `revision` | `Revision { counter: cur.revision.counter + 1, hash: TOMBSTONE_HASH }` |
| `parent` | `Some(cur.revision)` |
| `ancestors` | `[cur.revision] ++ cur.ancestors`, capped |
| `deleted_at` | `Some(now_ms)` |
| `meta` | `cur.meta`, with `author` restamped by the daemon when authenticated (as `put` is today, `grpc.rs:142`) |
| `links` | empty |

**`TOMBSTONE_HASH` is a fixed domain-separated hash,
`ContentHash::of(b"gonzalo:tombstone:v1")`, not the hash of the empty body.**
If tombstones used the empty-body hash, a live record edited to an empty body at
the same counter would get a revision identical to a concurrent tombstone. Sync
treats equal revisions as "already in sync", so that divergence would pass
silently. The domain string makes a tombstone's revision impossible to collide
with any live revision.

A useful side effect: two peers that independently delete the same revision
produce **byte-identical revisions**, so sync sees them as already in sync with
no special case.

`RecordKind::Tombstone.merge_class()` returns `MergeClass::Opaque`. Sync and
pull handle tombstones before any body merge runs, so the class is never
actually used for merging. It is set to the most conservative value in case a
future code path forgets that check.

### 3.2 Store trait

```rust
#[async_trait]
pub trait Store: Send + Sync {
    // ---- consumer surface (tombstones hidden) ----
    /// `None` for an absent key AND for a tombstoned key.
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>>;
    /// Excludes tombstoned keys.
    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>>;
    /// See "put over a tombstone" below.
    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>;
    /// Same signature, new meaning: writes a tombstone instead of removing.
    async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult>;

    // ---- replication surface (tombstones visible) ----
    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>>;
    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>>;
    /// Physically remove the record at `key` only if its current revision is
    /// `expected`. The only physical removal in the system.
    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult>;
}
```

`get_raw`, `list_raw` and `purge` are **required methods with no default
implementation.** A default of `get_raw = get` would compile, pass every test
that doesn't involve deletion, and silently resurrect records in production.
Requiring them is a breaking change to `gonzalo-core`, so this ships in 0.7.0.

#### `delete` semantics

| Current state | `expected` | Result |
|---|---|---|
| live `cur` | `None` | tombstone written (§3.1) → `Deleted` |
| live `cur` | `Some(cur.revision)` | tombstone written → `Deleted` |
| live `cur` | `Some(other)` | nothing written → `Conflict { current: cur }` |
| tombstone | any | nothing written → `Deleted` (the chain does not advance) |
| absent | any | nothing written → `Deleted` (unchanged from ADR 0018) |

Deleting an **absent** key still writes nothing. The store has no revision to
build a tombstone on, and a tombstone with no parent can't be ordered against a
peer's live copy. If a peer holds a record this store has never seen, delete
it where it exists, or sync first.

A `Conflict` from `delete` never carries a tombstone as `current`. Tombstones
don't leak through the consumer surface.

#### `put` over a tombstone

Consumers see a tombstoned key as absent, so `put` treats it as absent too,
with one exception for replication:

| Current state | `expected` | Result |
|---|---|---|
| tombstone `t` | `None` | **Recreation.** The store writes the caller's record with its `revision` **re-stamped** to `Revision { counter: t.revision.counter + 1, hash: ContentHash::of(body) }`, `parent = Some(t.revision)`, and `t.revision` folded into `ancestors` → `Committed(new_revision)` |
| tombstone `t` | `Some(t.revision)` | **Replication overwrite.** Written with the caller's revision unchanged → `Committed` |
| tombstone `t` | `Some(other)` | same as absent + `Some`: `Err(CoreError::NotFound)` |

Recreation is **the one case where `put` does not store the caller's revision
unchanged.** A consumer building a "new" record after a delete will typically
use `Revision::initial` (counter 0). If that were stored as-is, the recreated
record would look *older* than the tombstone and could lose to it in sync. The
re-stamp keeps the chain moving forward. `PutResult::Committed` already returns
the stored revision, so callers learn the real value without any API change.

Consumers never see a tombstone's revision, so they can't pass
`Some(t.revision)` by accident. That argument is only used by sync and pull
after a raw read.

#### Ancestor maintenance on every committed `put` or `delete`

All stores must apply the same rule, which lives in a shared core helper
(`gonzalo_core::ancestry::fold_ancestors`) so no store re-implements it:

```text
stored.ancestors =
  dedup( caller.ancestors ∪ {current.revision} ∪ current.ancestors )
  minus {stored.revision}
  sorted by (counter desc, hash desc)
  truncated to the store's ancestor cap
```

where `current` is the record being replaced, if there is one. A record's own
revision is never one of its ancestors. That matters when sync writes a
tombstone winner back onto the side that already holds it, with the same
revision. When sync
replicates a record, the incoming ancestors already include the replaced
revision, so the union changes nothing. When an application does an ordinary
update, the union adds the revision being replaced. The sort order is total,
so every substrate produces byte-identical lists.

Every store already reads the current record inside its OCC critical section
(fs: the per-key flock; git: the repo lock; s3: the `GetObject` whose ETag
gates the `If-Match` write; daemon: the backing store). The helper runs inside
that same critical section.

### 3.3 Where each substrate changes

| Substrate | `delete` | `get` / `list` | `get_raw` / `list_raw` | `purge` |
|---|---|---|---|---|
| **fs** | atomic tombstone write under the per-key flock (temp + fsync + rename, the `put_locked` path) | read and filter by kind | today's `get` / `list` | today's `delete_locked` with `expected` required |
| **git** | commit the tombstone file | read and filter | today's reads | today's `commit_removal` |
| **s3** | conditional `PutObject` with `If-Match` on the read ETag | `GetObject` per key and filter (see §8.4) | today's reads | today's conditional `DeleteObject` |
| **daemon client** (`ServerStore`) | existing endpoint | existing endpoints | new endpoints (§3.6) | new endpoint |
| **`AncestryStore`** | passes through | passes through | passes through | passes through |

Every in-workspace `Store` implementation, including test doubles, gets the new
methods. At the time of writing that is 14: the 4 real stores (fs, git, s3,
server client), the `AncestryStore` wrapper, and 9 test doubles (`MemStore`,
`FlakyOnceStore`, `AlwaysRacyStore`, ancestry's `Mem`, soak's `MockStore` and
`Conflicter`, the two server `DownStore`s, and ticket's `ConflictStore`).

The only exhaustive `match` on `RecordKind` outside core is in
`gonzalo-knowledge/src/lib.rs:246`. `Tombstone` joins the
`Checkpoint | GraphManifest => return Ok(None)` arm. Knowledge indexing only
sees records through consumer reads anyway, so this is belt-and-braces.

### 3.4 Sync

`sync_pass` switches from consumer reads to **raw reads** (`list_raw`,
`get_raw`). Otherwise it can't see tombstones, and a key tombstoned on A but
live on B would be copied back to A as a one-sided record.

The decision for a key held by both sides changes from "equal revision → skip,
otherwise merge" to:

```text
(ra, rb) both present:
  ra.revision == rb.revision                 → skip (already in sync)
  rb.revision ∈ ra.ancestors                 → A is ahead: overwrite B with A
                                               (expected = rb.revision)
  ra.revision ∈ rb.ancestors                 → B is ahead: overwrite A with B
  otherwise (diverged, or chain unknown):
    both tombstones                          → winner = higher (counter, hash);
                                               write winner, with ancestors
                                               folded from both, to both sides
    exactly one tombstone                    → SyncConflict (neither side written)
    both live                                → today's merge path; merged record's
                                               ancestors = fold(both sides + both
                                               revisions)
(ra, None) / (None, rb)                      → copy, unchanged (tombstones included)
```

What this changes for records that have nothing to do with deletion:

- **Fast-forward becomes exact.** Today, when one side is simply behind, sync
  still runs a body merge. For `Opaque` kinds such as `Checkpoint`, that merge
  reports a conflict even though nothing actually diverged. With ancestors, the
  store that's behind is just overwritten. This is a real behaviour
  improvement, and it gets its own test.
- **Legacy records** (empty `ancestors`) never match the ancestor checks and
  take today's merge path, so existing data behaves exactly as before.

**Rejected alternatives for detecting "ahead":**

- *Counter comparison* (`ra.revision.counter > rb.revision.counter` ⇒ A is
  ahead). A peer that edits many times offline ends up with a higher counter
  than a peer that deleted once, so a stale edit would beat a newer delete.
  Counters measure how many edits a peer made, not what order the edits
  happened in.
- *Version vectors.* These are exact, but they need a per-writer entry that
  grows with every replica and every principal. That doesn't fit a record
  format that is also a human-readable git file.

The bounded ancestor list is exact within the cap and **fails safe** beyond it:
a chain longer than the cap looks like a divergence, which reports a conflict
or merges, never silently overwrites.

### 3.5 Git pull

`merge_non_ff` already works out which paths changed on each side against git's
real merge base. Changes:

- **Changed only on the remote:** apply the remote file as-is (unchanged). A
  remote tombstone is just a modified file, so it replaces the local live
  record. A remote *purge* is a git deletion and removes the local file, which
  is correct (the remote already collected it).
- **Changed on both sides:** before the body comparison, look at kinds:
  - both tombstones, equal revisions → no-op
  - both tombstones, different revisions → keep the higher `(counter, hash)`,
    with ancestors folded from both
  - exactly one tombstone → `PullConflict`, keep local (the same policy as an
    unmergeable body today)
  - both live → today's path; `merged_record` folds both sides' ancestors
- The existing `(Some, Some) if local.body != remote.body` guard compares
  revisions for tombstones, because two tombstones always have equal (empty)
  bodies.

Git `delete` now commits a tombstone instead of `commit_removal`, and `purge`
takes over `commit_removal`.

### 3.6 Daemon surface

New routes. The existing `get` / `put` / `delete` / `keys` routes keep their
paths and gain the hide-tombstones meaning through the backing store.

| HTTP | gRPC | Store call | Auth (ADR 0015) |
|---|---|---|---|
| `GET /v1/raw/records/{ns}/{col}/{id}` | `GetRaw` | `get_raw` | `read` on `ns` |
| `GET /v1/raw/keys?namespace=&collection=` | `ListRaw` | `list_raw` | `read` on `ns`; admin when unscoped |
| `POST /v1/purge/{ns}/{col}/{id}` (body: expected revision JSON) | `Purge` | `purge` | **admin** |

Existing `DELETE /v1/records/...` keeps requiring `write` on the namespace.

**`ServerStore` against an old daemon.** HTTP 404 on the raw routes, or gRPC
`Unimplemented`, maps to
`CoreError::Backend("daemon predates replication reads (gonzalo#203); upgrade gonzalod")`.
The client **never falls back** to consumer reads, because that fallback would
bring back the resurrection bug this spec exists to fix.

### 3.7 Reset

A free function in core, next to `sync`:

```rust
pub struct ResetReport {
    pub deleted: Vec<RecordKey>,
    pub conflicts: Vec<RecordKey>,
}

/// Tombstone every live record under `prefix`. `prefix.namespace` is required.
pub async fn reset(store: &dyn Store, prefix: &KeyPrefix) -> Result<ResetReport>;
```

1. Refuse with an error if `prefix.namespace` is `None`. Resetting every
   namespace in a store is not a reset, and callers who really want that can
   loop over namespaces explicitly.
2. `list(prefix)` (consumer: live keys only).
3. For each key: `get`, then `delete(key, Some(rev))`. A `Conflict` (someone
   edited the key in between) goes into `conflicts` and is not retried. A key
   that vanished between `list` and `get` is skipped.

**Reset is not atomic.** No substrate offers multi-key transactions (s3 in
particular), and pretending otherwise would mean a lock, which the local-first
tier can't rely on. Instead it's **idempotent**: running it again tombstones
what the first run missed and skips what's already gone. Reset needs only
`write` on the namespace, because it is built entirely from `delete` calls.

### 3.8 Collection

```rust
pub struct CollectReport {
    pub purged: Vec<RecordKey>,
    /// Tombstones kept because they have no `deleted_at`.
    pub unstamped: usize,
    /// Purge lost an OCC race (the key was recreated during collection).
    pub conflicts: Vec<RecordKey>,
}

/// Purge tombstones under `prefix` whose `deleted_at` is at least `horizon` old.
pub async fn collect(
    store: &dyn Store,
    prefix: &KeyPrefix,
    horizon: std::time::Duration,
    now_ms: i64,
) -> Result<CollectReport>;
```

1. `list_raw(prefix)`, then `get_raw` each key and keep only tombstones.
2. Skip tombstones with `deleted_at == None` (count them) and those with
   `now_ms - deleted_at < horizon`. A future-dated `deleted_at` (clock skew)
   gives a negative age and is skipped, so skew can delay collection but can't
   trigger it early.
3. `purge(key, tombstone.revision)`. A `Conflict` means the key was recreated
   in between and is recorded, leaving the live record intact.

`now_ms` is a parameter so tests can control time. The CLI passes the system
clock.

**No default horizon.** The CLI requires `--older-than <duration>`. The right
value depends on how long peers go between syncs, which gonzalo can't know
(see §8.2).

### 3.9 Ancestor cap configuration

Stores have no configuration mechanism today (`FsStore::new(root)` is the only
constructor).

- `pub const DEFAULT_ANCESTOR_CAP: usize = 32;` in `gonzalo-core`.
- Each store gets a builder method, `.with_ancestor_cap(n: usize) -> Self`, so
  existing constructors keep compiling. `n == 0` is rejected at construction.
- The CLI and `gonzalod` each get `--ancestor-cap <n>`.
- **Size:** about 90 bytes per serialized ancestor, so roughly 2.9 KB at the
  default cap, and only on records edited 32 or more times.
- **Mixed caps across peers** are safe. Each store truncates to its own cap on
  write, and a shorter list only turns some fast-forwards into merges or
  conflicts.

### 3.10 CLI

| Command | Behaviour | Exit code |
|---|---|---|
| `gonzalo delete --namespace N --collection C --id I [--expected REV]` | one tombstone | 0 deleted; 1 conflict |
| `gonzalo reset --namespace N [--collection C]` | §3.7, prints `N deleted, M conflicts` | 0 if M == 0; 1 otherwise |
| `gonzalo collect --older-than 30d [--namespace N [--collection C]]` | §3.8, prints purged / unstamped / conflicts, and the horizon it used | 0 unless an I/O error |

`reset` without `--namespace` is a clap validation error, not a runtime
prompt. `collect` without `--namespace` runs across the whole store (admin on a
daemon).

## 4. Compatibility

### 4.1 Mixed binary versions

| Scenario | Outcome | Handling |
|---|---|---|
| Pre-0.7 binary reads a store directly (fs/git/s3) that holds a tombstone | `RecordKind` has no catch-all variant, so reading that key fails with `CoreError::Serde`. `list` still returns the key. Old `sync` stops with that error. | Can't be retrofitted. Release notes: **upgrade every binary that reads a store directly at the same time.** The failure is loud and loses no data. |
| Pre-0.7 `ServerStore` → 0.7 daemon, normal `get`/`put`/`list`/`delete` | Tombstoned keys look absent, and `delete` writes a tombstone. | Works. |
| Pre-0.7 binary **runs `sync`** with one side on a 0.7 daemon | Old sync uses consumer reads, can't see the tombstone, and copies the peer's live record back as a recreation. **Resurrection.** | Known limitation, in the release notes: upgrade every binary that runs sync together. The store can't tell this from a real recreation, so it can't block it. |
| 0.7 `ServerStore` → pre-0.7 daemon | Raw routes return 404 / `Unimplemented`. | Clear upgrade error (§3.6), no fallback. Consumer operations still work. |
| New optional fields `ancestors` / `deleted_at` | `Record` does not use `deny_unknown_fields`, so old binaries read new live records fine. An old binary rewriting a record drops `ancestors`. | Safe. The truncated chain reports a conflict or merges later (§3.4), never a silent wrong winner. |

### 4.2 Semver

`gonzalo-core` gains required trait methods and a new `RecordKind` variant, so
the release is **0.7.0** across the lockstep workspace. Third-party `Store`
implementers get a compile error pointing at exactly what to add, which is the
intended outcome.

### 4.3 ADR changes

- **New ADR 0021, "Replicated deletion with tombstones":** records §3.1–§3.8
  as a decision, with the rejected alternatives (local-only reset, counter
  ordering, version vectors, automatic collection).
- **ADR 0018** stays `accepted`. Its OCC semantics still hold; only its
  local-only decision is superseded. Index row:
  `accepted (local-only deletion superseded by [0021])`. 0021's Context names
  the part of 0018 it supersedes, so the link goes both ways, as
  `adr-validate` requires.

## 5. Decisions and rejected alternatives

### 5.1 Tombstones rather than local-only reset

A local-only reset is simple and resurrects on sync, which makes it unusable as
soon as a namespace is replicated. A hybrid (local reset now, tombstones later)
ships a reset whose meaning changes later, which is worse than waiting. Gonzalo
targets deployments from local to enterprise, so deletion has to replicate.

### 5.2 Explicit collection, never automatic

A tombstone is what stops resurrection. Purging one early is data loss that
shows up **later, on a different machine**, when a peer that was offline
syncs. Only the operator knows how long peers go offline. An automatic
collector would have to guess a horizon, and a wrong guess fails silently.
Explicit collection keeps that decision with the operator and shows it in
command output.

### 5.3 Correct under ad-hoc pairwise sync, with hub collection as a later optimisation

The base design makes no assumption about topology: any two stores can sync in
any order and converge. A hub-assisted scheme (a central daemon tracks when
each peer last synced, making collection safe automatically) is a valid future
optimisation for the enterprise tier. It isn't required for correctness and
isn't built here.

### 5.4 Delete vs. concurrent edit reports a conflict

When one peer deletes and another edits the same parent revision, neither can
be said to win without losing someone's intent. Surfacing it matches how
gonzalo already treats unmergeable divergence.

### 5.5 Same path, filtered reads

A tombstone replaces the record file at its normal path. There's no separate
tombstone index to keep consistent, and git history shows the delete as an
ordinary modification. The cost is that `list` has to read records to filter
them (see §8.4).

## 6. Testing

### 6.1 Conformance (`gonzalo-core/src/conformance.rs`, all four substrates)

The four existing delete cases are rewritten for the new meaning (for example,
`delete_unconditional_removes` becomes "hides from `get`"). New cases:

| Case | Asserts |
|---|---|
| `delete_hides_from_get_and_list` | After `delete`: `get` → `None`, `list` excludes the key |
| `delete_visible_to_raw_reads` | `get_raw` returns `Tombstone` kind, empty body, `deleted_at` set, `revision.counter == prior + 1`, `revision.hash == TOMBSTONE_HASH`, prior revision in `ancestors`. `list_raw` includes the key. |
| `delete_conflicts_on_stale_expected` | Stale `expected` → `Conflict` with the live `current`, no tombstone written |
| `delete_of_absent_key` | `Deleted`, and `get_raw` is still `None` |
| `delete_of_tombstone_is_noop` | Second `delete` → `Deleted`, and the raw revision is unchanged |
| `independent_deletes_are_identical` | Deleting the same revision on two fresh stores gives equal revisions |
| `tombstone_never_collides_with_empty_body` | Tombstone revision ≠ an empty-body live edit at the same counter |
| `recreate_continues_chain` | `put(initial_rec, None)` over a tombstone → `Committed(r)` with `r.counter == tomb.counter + 1`, `parent == tomb.revision`, tombstone in `ancestors` |
| `put_some_over_tombstone_is_not_found` | `put(rec, Some(random))` over a tombstone → `NotFound` |
| `replication_overwrite_of_tombstone` | `put(rec, Some(tomb.revision))` stores `rec.revision` unchanged |
| `purge_removes_physically` | After `purge`: `get_raw` → `None`, `list_raw` excludes the key |
| `purge_conflicts_after_recreation` | `purge(tomb.revision)` after a recreation → `Conflict`, live record intact |
| `ancestors_capped_and_ordered` | After cap + 5 updates: `ancestors.len() == cap`, newest first, matching the fold rule |

### 6.2 Sync (`gonzalo-core/src/sync.rs` tests, `MemStore`)

- stale peer takes the tombstone (A deleted, B holds the parent)
- the same case with sync direction reversed ends deleted (order doesn't matter)
- recreation after delete propagates as live
- delete vs. concurrent edit → `SyncConflict`, neither side written
- concurrent tombstones at different counters converge on the higher one
- **fast-forward of an `Opaque` kind** (Checkpoint) no longer conflicts when
  one side is simply behind
- a legacy record with no ancestors still takes the merge path
- a structured merge folds both sides' ancestors
- a truncated chain (cap 2, divergence deeper than 2) → conflict or merge,
  never overwrite
- mixed caps (4 and 32) converge
- tombstone variants of `FlakyOnceStore` / `AlwaysRacyStore` still terminate
  and converge

### 6.3 Git pull (`gonzalo-store-git/tests/pull.rs`)

- `nonff_pull_applies_remote_tombstone`
- `nonff_pull_keeps_local_tombstone`
- `nonff_pull_surfaces_delete_vs_edit_conflict`
- `nonff_pull_converges_concurrent_tombstones`
- `pull_fast_forwards_tombstone`
- `nonff_pull_applies_remote_purge`

### 6.4 Reset and collect (core tests)

- reset refuses a prefix without a namespace
- reset with a concurrent edit reports the key in `conflicts`, and a second run
  deletes nothing more and conflicts on nothing new (idempotent)
- reset scoped to a collection leaves sibling collections alone
- collect skips unstamped, too-young and future-dated tombstones, and never
  touches live records
- collect racing a recreation → the key lands in `conflicts` and the live
  record survives

### 6.5 Daemon (`gonzalo-server` http + grpc tests)

- raw reads need `read`, purge needs admin, and unscoped `list_raw` needs admin
- `delete` over the wire returns a tombstone on a later `get_raw`
- `ServerStore` maps 404 / `Unimplemented` on raw routes to the upgrade error,
  and never calls consumer `get` as a fallback (asserted with a wiremock that
  fails the test on any consumer-route hit)

### 6.6 CLI (integration tests)

- `delete`, `reset` and `collect` exit codes and summary lines
- `reset` without `--namespace` is rejected by argument parsing
- `collect` without `--older-than` is rejected by argument parsing

### 6.7 Soak (`gonzalo-soak`)

- The workload gains `Delete` and `Recreate` operations mixed into the
  concurrent writers.
- New oracle invariant: after writers stop and replicas settle, **every key is
  either live on all replicas or a tombstone on all replicas**, never a mix.
- The existing "conflicts were actually exercised" check extends to delete
  conflicts.

## 7. Delivery

Slices, in dependency order. Slices 1–5 are only safe **together**: a release
containing tombstones without tombstone-aware sync would resurrect records. They
can merge to `main` one by one, but no release is tagged until slice 5 lands.

1. **Core model and trait:** `RecordKind::Tombstone`, `ancestors`,
   `deleted_at`, `TOMBSTONE_HASH`, `fold_ancestors`, new trait methods, cap
   constant, all test doubles, the knowledge match arm, conformance cases (§6.1).
2. **fs and git stores:** §3.3, passing conformance.
3. **s3 store:** §3.3, passing conformance (RustFS-qualified per ADR 0019).
4. **Daemon and client:** routes, RPCs, auth, the upgrade error (§3.6, §6.5).
5. **Replication:** sync (§3.4) and git pull (§3.5), with their tests.
6. **Reset, collect and CLI:** §3.7–§3.10, §6.4, §6.6. Closes #203.
7. **Soak:** §6.7.
8. **Docs:** ADR 0021, ADR 0018 index back-reference, guide pages for the
   three commands with a prominent "choosing a horizon" section, CHANGELOG
   with the upgrade-together warning.

Follow-up tickets filed alongside:

- orphaned blob garbage collection (§8.3)
- populating `Meta.created` / `Meta.updated`
- s3 list performance for large namespaces (§8.4)

## 8. Risks

### 8.1 Mixed-version sync resurrects

A pre-0.7 binary running sync against 0.7 data brings deleted records back
(§4.1). This can't be prevented from the new side. Mitigation is documentation
and the upgrade-together warning in the CHANGELOG and release notes.

### 8.2 Collection shorter than a peer's offline window

Once a tombstone is purged, a peer that still holds the live record and hasn't
synced since the delete will copy it back as new. This is inherent to any
tombstone design with collection. It's why collection is explicit, has no
default horizon, and prints the horizon it used. The guide will recommend a
horizon comfortably longer than the longest expected gap between syncs.

### 8.3 Orphaned blobs

Deleting a record whose body is `Body::Blob` leaves the blob in the
`BlobStore`. Blobs are content-addressed and may be shared between records (the
point of ADR 0012), so purge can't safely remove them. Storage for blob-backed
records isn't reclaimed until blob GC exists (follow-up ticket).

### 8.4 `list` must read to filter

Consumer `list` has to read each record to exclude tombstones. On fs and git
that's a local read per key. On s3 it's a `GetObject` per key, which is
expensive for large namespaces. It's acceptable for current namespace sizes and
tracked as a follow-up. Candidate fixes are a kind marker in the object key
suffix, or a per-collection tombstone index. Either is a layout change, so it
needs its own design.

### 8.5 Recreation re-stamps the caller's revision

A caller that ignores the returned `Committed(revision)` and reuses its own
locally built revision on the next conditional `put` gets a conflict. Today
`put` always stores the caller's revision, so this is a new way to hit a
conflict. It's recoverable. It's documented on `put`, and the conformance case
§6.1 `recreate_continues_chain` pins the behaviour down.
