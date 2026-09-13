# Replicated Deletion (Tombstones) — Overview and Shared Contract

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make deletion replicate across gonzalo substrates by writing tombstone records, then build explicit tombstone collection and first-class namespace/collection reset on top (gonzalo#203).

**Architecture:** A delete writes a `RecordKind::Tombstone` record at the key's normal path. Consumer reads (`get`/`list`) hide tombstones and replication reads (`get_raw`/`list_raw`) show them. Every store makes its put/delete/purge decisions through shared pure "planner" functions in `gonzalo-core`, so all substrates behave identically by construction, and the conformance suite proves it. Sync and git pull use raw reads and a bounded in-record ancestor list to tell "behind" apart from "diverged".

**Tech Stack:** Rust 2024 workspace (MSRV 1.95), tokio, async-trait, serde/serde_json, git2 (vendored libgit2), aws-sdk-s3, axum + tonic (daemon), clap (CLI).

**Spec:** `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md`. Read it before any slice. Section references below (§N) point into it.

## Global Constraints

- Verification gate before every push and PR, matching CI exactly:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo build --workspace --all-targets --all-features`
  - `cargo test --workspace --all-features`
- Run the gate as bare commands, one per line, not joined with `&&`. `set -e` does not abort on a failure to the left of `&&`, and a failure there once passed silently.
- Always open a PR and merge it after CI is green. Never push to `main`.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`. PR bodies end with `https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.
- PR bodies say `Part of #203`. Only slice 6 says `Closes #203`.
- Tombstone hash domain string, verbatim: `gonzalo:tombstone:v1`.
- Default ancestor cap: `32`. A cap of `0` is rejected at construction.
- `deleted_at` unit: milliseconds since the Unix epoch, `i64`.
- New `Record` fields are `#[serde(default, skip_serializing_if = ...)]`. Never add `deny_unknown_fields`.
- `delete_as`, `get_raw`, `list_raw`, `put_raw` and `purge` are required trait methods. **Never** give them a default implementation. Never fall back from a raw read or write to a consumer one. `delete` is the only provided method.
- Workspace lints: `unsafe_code = "forbid"`, clippy `all = warn`, promoted to errors by `-D warnings`.
- **No release is tagged until slices 1–5 have all merged** (§7). Slices merge to `main` one at a time.

---

## Slices

| # | Plan file | Delivers | Depends on |
|---|---|---|---|
| 1 | `2026-09-13-tombstones-01-core-model-and-trait.md` | Record fields, `Tombstone` kind, planners, trait methods, reference `MemStore`, `run_tombstone_conformance`; all impls compile with interim raw methods | — |
| 2 | `2026-09-13-tombstones-02-fs-and-git-stores.md` | fs and git stores write tombstones, filter consumer reads, purge; pass tombstone conformance | 1 |
| 3 | `2026-09-13-tombstones-03-s3-store.md` | s3 store does the same | 1 |
| 4 | `2026-09-13-tombstones-04-daemon-and-client.md` | raw and purge routes/RPCs, auth, `ServerStore`, upgrade error; server-client conformance; tombstone cases folded into `run_store_conformance` | 1, 2, 3 (daemon tests run over `FsStore`; `gonzalod` also builds `S3Store::with_ancestor_cap`) |
| 5 | `2026-09-13-tombstones-05-sync-and-pull.md` | sync on raw reads with ancestor-based ordering; git pull tombstone handling | 1, 2 |
| 6 | `2026-09-13-tombstones-06-reset-collect-cli.md` | `reset`, `collect`, CLI `delete`/`reset`/`collect` | 1, 2 (CLI tests over fs), 5 not required |
| 7 | `2026-09-13-tombstones-07-soak-and-docs.md` | soak delete/recreate ops and invariant; ADR 0021; ADR 0018 back-reference; guide; CHANGELOG | 1–6 |

Slices 2 and 3 can run in parallel once slice 1 has merged. So can slices 5 and 6 after slice 2.

### States between slices (safe on `main`, never released)

- **After slice 1:** no store writes tombstones yet. The real stores' interim `get_raw`/`list_raw` delegate to `get`/`list`, which is correct because no tombstones exist. Interim `purge` delegates to today's conditional physical delete.
- **After slice 2 or 3, before slice 5:** that store writes tombstones, but sync still uses consumer reads. A synced peer's copy comes back as a recreation, which is exactly today's ADR 0018 behaviour. Not a regression, and not released. **Git pull is worse in this window:**
`merge_non_ff` merges a live record against a remote tombstone by the live
record's merge class, so an append-only kind silently drops the remote delete.
Slice 5 fixes this. It's another reason nothing is tagged before slice 5.
- **After slice 4:** `run_store_conformance` calls the tombstone cases itself, so every future store gets them automatically.
- **Before slice 4 merges:** `ServerStore::get_raw` / `list_raw` / `put_raw` are interim consumer delegations, same as the other stores' interim raw methods. Running slice 5's sync against a daemon before slice 4 merges would therefore still use consumer reads under the hood. Merge slice 4 before relying on daemon sync.
- **For slice 7's ADR 0021:** independent deletes of the same revision converge on byte-identical tombstone revisions (§3.1), but each peer stamps its own `deleted_at` at its own local delete time, so peers may become eligible to purge — and actually purge — at different times. That's harmless under the explicit-horizon collection model (§3.8): collection never runs automatically, and a purge only removes the exact revision named, so a peer that purges later never resurrects or diverges from one that purged earlier.

---

## Shared Contract (produced by slice 1, consumed by all later slices)

Use these names and signatures exactly. If a later slice needs something that isn't here, add it in that slice's plan as a new item and list it under **Interfaces → Produces**. Never rename an item from this contract.

### `gonzalo-core/src/record.rs`

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordKind {
    MemoryTier, Topic, Session, Checkpoint, Ticket, TicketEvent, GraphManifest,
    /// A deletion marker. Hidden from consumer reads; replicated by sync and
    /// pull; physically removed only by `Store::purge`. See ADR 0021.
    Tombstone, // serializes as "Tombstone"; merge_class() == MergeClass::Opaque
}

pub struct Record {
    pub key: RecordKey,
    pub kind: RecordKind,
    pub revision: Revision,
    pub parent: Option<Revision>,
    pub body: Body,
    pub meta: Meta,
    pub links: Vec<RecordKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
}

impl Record {
    pub fn is_tombstone(&self) -> bool; // self.kind == RecordKind::Tombstone
}
```

### `gonzalo-core/src/tombstone.rs` (re-exported from the crate root)

```rust
pub const DEFAULT_ANCESTOR_CAP: usize = 32;
pub const TOMBSTONE_DOMAIN: &[u8] = b"gonzalo:tombstone:v1";

/// ContentHash::of(TOMBSTONE_DOMAIN).
pub fn tombstone_hash() -> ContentHash;
/// Wall-clock ms since the Unix epoch.
pub fn now_ms() -> i64;
/// Reject a zero cap: Err(CoreError::Backend("ancestor cap must be at least 1")).
pub fn validate_ancestor_cap(cap: usize) -> Result<usize>;

/// dedup(incoming ∪ {current.revision} ∪ current.ancestors) minus {stored},
/// sorted by (counter desc, hash desc), truncated to `cap`.
pub fn fold_ancestors(
    stored: &Revision,
    incoming: &[Revision],
    current: Option<&Record>,
    cap: usize,
) -> Vec<Revision>;

/// The tombstone that deleting live `current` produces (spec §3.1 table).
/// `Some(author)` replaces `meta.author` with the deleter.
pub fn tombstone_of(
    current: &Record,
    now_ms: i64,
    cap: usize,
    author: Option<&Identity>,
) -> Record;

pub enum PutPlan {
    /// Persist exactly this record (revision may be re-stamped on recreation;
    /// ancestors already folded). Return PutResult::Committed(record.revision).
    Write(Record),
    Conflict(Box<Conflict>),
    /// Return Err(CoreError::NotFound(key)).
    NotFound,
    /// The record may not be written through this path. Return
    /// Err(CoreError::Backend(reason.to_string())). Only consumer `plan_put`
    /// produces this, for a `RecordKind::Tombstone` record.
    Rejected(&'static str),
}
pub fn plan_put(
    current: Option<&Record>,
    record: Record,
    expected: Option<Revision>,
    cap: usize,
) -> PutPlan;
/// Replication write: never re-stamps; a tombstone is an ordinary record.
pub fn plan_put_raw(
    current: Option<&Record>,
    record: Record,
    expected: Option<Revision>,
    cap: usize,
) -> PutPlan;

pub enum DeletePlan {
    /// Persist this tombstone, then return DeleteResult::Deleted.
    Write(Record),
    /// Write nothing; return DeleteResult::Deleted.
    Noop,
    Conflict(Box<Conflict>),
}
pub fn plan_delete(
    current: Option<&Record>,
    expected: Option<Revision>,
    now_ms: i64,
    cap: usize,
    author: Option<&Identity>,
) -> DeletePlan;

pub enum PurgePlan {
    /// Physically remove the stored record; return DeleteResult::Deleted.
    Remove,
    /// Nothing stored; return DeleteResult::Deleted.
    Noop,
    Conflict(Box<Conflict>),
}
pub fn plan_purge(current: Option<&Record>, expected: &Revision) -> PurgePlan;
```

Planner decision tables (these are the single source of truth; spec §3.2):

| `plan_put` current | expected | Plan |
|---|---|---|
| any | any | record is `RecordKind::Tombstone` → `Rejected(CONSUMER_TOMBSTONE_REJECTED)` |
| `None` | `None` | `Write(record with ancestors folded)` |
| `None` | `Some(_)` | `NotFound` |
| live `c` | `Some(c.revision)` | `Write(record with ancestors folded against c)` |
| live `c` | `None` or `Some(other)` | `Conflict { current: c }` |
| tombstone `t` | `None` | `Write(recreated)`: revision = `{ t.revision.counter + 1, ContentHash::of(record.body.bytes()) }`, `parent = Some(t.revision)`, `deleted_at = None`, ancestors folded against `t` |
| tombstone `t` | `Some(_)` (any, including `t.revision`) | `NotFound` |

| `plan_put_raw` current | expected | Plan |
|---|---|---|
| `None` | `None` | `Write(record, ancestors folded)` (tombstones included) |
| `None` | `Some(_)` | `NotFound` |
| any `c` (live or tombstone) | `Some(c.revision)` | `Write(record verbatim, ancestors folded against c)` |
| any `c` (live or tombstone) | `None` or `Some(other)` | `Conflict { current: c }` (may carry a tombstone) |

| `plan_delete` current | expected | Plan |
|---|---|---|
| `None` | any | `Noop` |
| tombstone | any | `Noop` |
| live `c` | `None` or `Some(c.revision)` | `Write(tombstone_of(c, now_ms, cap, author))` |
| live `c` | `Some(other)` | `Conflict { current: c }` |

| `plan_purge` current | expected | Plan |
|---|---|---|
| `None` | any | `Noop` |
| any `c` | `c.revision` | `Remove` |
| any `c` | other | `Conflict { current: c }` |

### `gonzalo-core/src/store.rs`: `Store` trait additions

```rust
/// Required. Conditional delete that writes a tombstone; `Some(author)`
/// replaces the tombstone's `meta.author`. See `plan_delete`.
async fn delete_as(
    &self,
    key: &RecordKey,
    expected: Option<Revision>,
    author: Option<Identity>,
) -> Result<DeleteResult>;
/// Provided (unchanged signature): `self.delete_as(key, expected, None)`.
async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult> {
    self.delete_as(key, expected, None).await
}

/// Like `get`, but returns tombstones. Replication only (sync, pull, collect).
async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>>;
/// Like `list`, but includes tombstoned keys. Replication only.
async fn list_raw(&self, prefix: &crate::KeyPrefix) -> Result<Vec<RecordKey>>;
/// Replication write. Never re-stamps; a create over a tombstone is a
/// Conflict carrying the tombstone. See `plan_put_raw`.
async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>;
/// Physically remove the record at `key` iff its current revision is
/// `expected`. The only physical removal in the system. See `plan_purge`.
async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult>;
```

The consumer methods keep their signatures. From slice 2/3/4 onward per store: `get` returns `None` for a tombstone, `list` excludes tombstoned keys, `put` follows `plan_put`, `delete_as` follows `plan_delete`, and `put_raw` follows `plan_put_raw`. Stores implement `delete_as`, never `delete`.

**Why `put_raw` exists.** Sync's one-sided copy used to call consumer `put(rec, None)`. If a tombstone landed on the destination between sync's raw read and that put, `plan_put` treated the copy as a recreation, re-stamped it past the tombstone, and brought the deleted record back. Every replication write goes through `put_raw`, which never re-stamps.

**Rules every store follows:**
- A `PutPlan::Rejected(reason)` maps to `Err(CoreError::Backend(reason.to_string()))`.
- A conformance factory returns a fresh, empty store on every call.
- A stored object that fails to deserialize is still included by consumer `list`, as today, so `get` surfaces the parse error.
- s3 only: a lost conditional write (HTTP 412) re-reads and re-plans, up to 8 attempts, then returns `CoreError::Backend`. That gives the same outcomes as the lock-based stores.
- Daemon only: a `put`/`put_raw` that the backing store rejects with `CoreError::NotFound` crosses the wire as HTTP `412` / gRPC `FailedPrecondition`. `ServerStore` maps it back to `CoreError::NotFound`, because conformance asserts that variant. It is never `404`, which on raw routes means "old daemon", and never an opaque `500`.
- Daemon only: `put_raw` from a non-admin principal restamps `meta.author` to the caller (ADR 0015). Admin and open mode keep the replicated author.

### `gonzalo-core/src/memstore.rs`, `#[cfg(any(test, feature = "conformance"))]`

```rust
/// Reference in-memory Store built entirely on the planners. Used by core
/// tests (sync, reset, collect) and as the conformance self-test.
pub struct MemStore { /* Mutex<BTreeMap<RecordKey, Record>>, cap, clock */ }
impl Default for MemStore { /* == MemStore::new(); not derived, a derived cap would be 0 */ }
impl MemStore {
    pub fn new() -> Self;                              // cap = DEFAULT_ANCESTOR_CAP, clock = now_ms
    pub fn with_ancestor_cap(self, cap: usize) -> Self; // panics on 0 (test helper)
    pub fn with_clock(self, now_ms: i64) -> Self;       // fixed deleted_at for deterministic tests
    pub fn raw_snapshot(&self) -> BTreeMap<RecordKey, Record>;
}
impl Store for MemStore { /* all 7 methods via planners */ }
```

### `gonzalo-core/src/conformance.rs`, `#[cfg(feature = "conformance")]`

```rust
/// The tombstone cases from spec §6.1. `factory` must build stores whose
/// ancestor cap is `cap` (cases assert against it).
pub async fn run_tombstone_conformance<S, F, Fut>(factory: F, cap: usize)
where
    S: Store,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = S>;
```

### Per-store builder (slices 2–4)

Every real store gains `pub fn with_ancestor_cap(self, cap: usize) -> gonzalo_core::Result<Self>` (validates via `validate_ancestor_cap`) and stores `cap: usize`, which defaults to `DEFAULT_ANCESTOR_CAP` in the existing constructors.
