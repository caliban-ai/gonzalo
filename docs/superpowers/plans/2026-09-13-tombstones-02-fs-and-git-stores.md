# Tombstones Slice 2: fs and git Stores Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `FsStore` and `GitStore` write tombstones on `delete_as` (and therefore on the trait-provided `delete`), take replication writes through `put_raw`, hide tombstones from consumer reads, expose them through raw reads, physically remove them only through `purge`, and pass `run_tombstone_conformance` at the default cap and at a small cap.

**Architecture:** Each store keeps its existing OCC critical section (fs: the per-key `flock` on `<id>.json.lock`; git: the repo-wide `flock` on `<root>/.gonzalo-git.lock`). Inside that section it reads the current record, asks the shared core planner (`plan_put` / `plan_put_raw` / `plan_delete` / `plan_purge`) what to do, and carries out the plan: write the record (fs: temp + fsync + rename, git: write + commit), write nothing, return a conflict, or physically remove the file. The store makes no decisions of its own. `put` and `put_raw` share one locked write path that takes the planner as a function pointer. Consumer `get`/`list` filter out tombstones, and `get_raw`/`list_raw` keep today's unfiltered behaviour. Stores implement `delete_as`, never `delete`, which the trait provides as `delete_as(key, expected, None)`.

**Tech Stack:** Rust 2024 (MSRV 1.95), tokio `spawn_blocking`, rustix `flock`, serde_json, git2 (vendored libgit2), tempfile (tests).

**Spec:** `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` (§3.2, §3.3, §3.9, §6.1). Shared contract: `docs/superpowers/plans/2026-09-13-tombstones-00-overview.md`. Read both before starting.

## Global Constraints

Copied from the overview. Every task implicitly includes these.

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
- `get_raw`, `list_raw` and `purge` are required trait methods. **Never** give them a default implementation, and never fall back from a raw read to a consumer read.
- Workspace lints: `unsafe_code = "forbid"`, clippy `all = warn`, promoted to errors by `-D warnings`.
- **No release is tagged until slices 1–5 have all merged** (§7). Slices merge to `main` one at a time.

Rules specific to this slice:

- **Precondition:** slice 1 (`2026-09-13-tombstones-01-core-model-and-trait.md`) has merged to `main`. The following names are importable from the `gonzalo_core` crate root: `DEFAULT_ANCESTOR_CAP`, `tombstone_hash`, `now_ms`, `validate_ancestor_cap`, `plan_put`, `plan_put_raw`, `plan_delete`, `plan_purge`, `PutPlan`, `DeletePlan`, `PurgePlan`, `Identity`. `Record::is_tombstone` exists. `gonzalo_core::conformance::run_tombstone_conformance` exists behind the `conformance` feature, which both store crates already enable in `[dev-dependencies]`.
- **Reconciled contract used by this slice** (these names win over any older text):
  - Required trait methods: `get_raw`, `list_raw`, `purge`, `async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>`, and `async fn delete_as(&self, key: &RecordKey, expected: Option<Revision>, author: Option<Identity>) -> Result<DeleteResult>`.
  - `delete` is a **provided** trait method calling `self.delete_as(key, expected, None)`. Stores implement `delete_as` and **must not** implement `delete`.
  - `plan_put_raw(current: Option<&Record>, record: Record, expected: Option<Revision>, cap: usize) -> PutPlan`. It never re-stamps. `None`/`None` → `Write` (ancestors folded). `None`/`Some` → `NotFound`. `Some(c)`/`Some(c.revision)` → `Write` (verbatim, folded against `c`). `Some(c)`/`None` or another revision → `Conflict { current: c }`, where `c` may be a tombstone.
  - `plan_put` (consumer) over a tombstone: `expected == None` → recreation. `expected == Some(anything)`, including the tombstone's own revision → `NotFound`.
  - `plan_delete(current, expected, now_ms, cap, author: Option<&Identity>) -> DeletePlan`. `Some(author)` replaces `meta.author` on the tombstone.
  - A conformance factory returns a fresh, empty store on every call.
  - A stored file that fails to deserialize is still included by consumer `list`.
- **Do not touch** `git_pull`, `merge_non_ff`, `merged_record` or anything else on the pull path in `crates/gonzalo-store-git/src/lib.rs`. That is slice 5.
- **Keep the fs durability pattern.** Every record publish is temp file, `sync_all`, `rename`, then `fsync_dir(parent)`. Every physical removal is `remove_file`, then `fsync_dir(parent)`. Keep the existing explanatory comments when moving code.
- **Line numbers** below are from commit `e155d25` (before slice 1). Slice 1 adds interim `get_raw`/`list_raw`/`purge` methods and new `Record` fields, which shift the numbers. Find each spot by the quoted text, not by the number.
- **Memory is tight on the dev machine.** Always scope in-task test runs with `-p <crate> --test <file> <name>`. Run the whole workspace only in the final task.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/gonzalo-store-fs/src/lib.rs` | Modify | `FsStore { root, cap }`, `with_ancestor_cap`. The critical-section helpers `lock_record`, `read_current`, `write_durable` and `remove_durable`. `put_locked` (shared by `put` and `put_raw` through a `PutPlanner` fn pointer), `delete_locked` (behind `delete_as`) and `purge_locked` delegate to the planners. `get`/`list` filter tombstones through `listed_live`. `get_raw`/`list_raw` are unfiltered. No `delete` impl. |
| `crates/gonzalo-store-fs/tests/tombstones.rs` | Create | fs-specific tests: cap wiring through `put` and `put_raw`, tombstone file at the record path with the stamped author, no leftover temp file, `put`/`put_raw` over a tombstone, purge unlinks the file, unparseable-file listing, concurrent deletes. |
| `crates/gonzalo-store-fs/tests/conformance.rs` | Modify | Also run `run_tombstone_conformance` at `DEFAULT_ANCESTOR_CAP` and at cap 3. |
| `crates/gonzalo-store-git/src/lib.rs` | Modify | `GitStore { root, cap }`, `with_ancestor_cap`, `handle`, `put_locked` (shared by `put` and `put_raw`), `write_and_commit`, `remove_and_commit`, `is_listed`, the free fn `rel_path` and the `PutPlanner` type alias. The `Store` impl goes through the planners. `delete_as` commits a tombstone file, `purge` takes over `commit_removal`, and there is no `delete` impl. |
| `crates/gonzalo-store-git/tests/tombstones.rs` | Create | git-specific tests: cap wiring through `put` and `put_raw`, the tombstone is committed at the record path with the stamped author, a no-op delete makes no commit, `put`/`put_raw` over a tombstone, purge commits the removal, unparseable-file listing. |
| `crates/gonzalo-store-git/tests/conformance.rs` | Modify | Also run `run_tombstone_conformance` at `DEFAULT_ANCESTOR_CAP` and at cap 3. |

### Legacy test audit (done while writing this plan)

I grepped the whole workspace for `.delete(` calls and for tests that assert on-disk or git-tree state after a delete.

| Location | Uses delete? | Needs change in this slice? |
|---|---|---|
| `crates/gonzalo-store-fs/tests/list_ignores_stray_files.rs` | no (put + list) | No. Stray non-directory files are still skipped by `collect_keys`. The consumer filter only looks at `*.json` files that the walk already yields. |
| `crates/gonzalo-store-fs/tests/concurrent_put_no_lost_update.rs` | no | No. `plan_put(live c, Some(c.revision))` is `Write`, and a stale expected revision is `Conflict`, the same as today. |
| Any test calling `store.delete(..)` | — | No. `delete` is now a provided trait method, so existing call sites compile unchanged and reach `delete_as(.., None)`. |
| `crates/gonzalo-store-fs/tests/{blob_store,slice_gc}.rs` | `delete_blob` only | No. `BlobStore` is untouched. |
| `crates/gonzalo-store-git/tests/put_and_push.rs` | no | No. Two racing `put(_, None)` calls on an absent key still produce one `Write` and one `Conflict`. |
| `crates/gonzalo-store-git/tests/pull.rs` | no | No. Conditional puts still store the caller's revision. The files now carry `ancestors`, but `merge_non_ff` compares `body` only. Slice 5 adds the tombstone pull tests. |
| `crates/gonzalo-core/src/conformance.rs` delete cases | yes | Rewritten by slice 1 (spec §6.1). This slice only has to pass them. |
| `crates/gonzalo-server/src/{http,grpc}.rs` tests | record delete only through the `DownStore` test doubles; `http.rs:744` covers blobs | No. |
| `crates/gonzalo-integration-tests`, `gonzalo-cli`, `gonzalo-mcp`, `gonzalo-ticket` | no store `.delete(` calls (ticket's `ConflictStore::delete` is a double) | No. Task 7 runs `server_store_conformance` explicitly, because its backing `FsStore` now writes tombstones. |

Since none of the existing tests needs a rewrite, there is no separate "fix legacy tests" task. Task 7 re-runs every one of them by name.

---

### Task 1: fs ancestor cap, `put`/`put_raw` through the put planners, `purge` through `plan_purge`

**Files:**
- Modify: `crates/gonzalo-store-fs/src/lib.rs` (imports `:9-12`, struct and `new` `:19-27`, `impl Store` `:43-78`, `put_locked` `:193-257`)
- Create: `crates/gonzalo-store-fs/tests/tombstones.rs`

**Interfaces:**
- Consumes (slice 1): `gonzalo_core::{DEFAULT_ANCESTOR_CAP, validate_ancestor_cap, plan_put, plan_put_raw, PutPlan, plan_purge, PurgePlan, Identity}`, plus the required trait methods `get_raw` / `list_raw` / `put_raw` / `delete_as` / `purge`.
- Produces:
  - `pub fn FsStore::with_ancestor_cap(self, cap: usize) -> gonzalo_core::Result<FsStore>`
  - Private, in `gonzalo-store-fs/src/lib.rs`, used by Task 2:
    - `type PutPlanner = fn(Option<&Record>, Record, Option<Revision>, usize) -> PutPlan;`
    - `fn lock_record(path: &Path) -> Result<std::fs::File>`
    - `fn read_current(path: &Path) -> Result<Option<Record>>`
    - `fn write_durable(path: &Path, record: &Record) -> Result<()>`
    - `fn remove_durable(path: &Path) -> Result<()>`
    - `fn put_locked(root: &Path, record: Record, expected: Option<Revision>, cap: usize, plan: PutPlanner) -> Result<PutResult>`
    - `fn purge_locked(root: &Path, key: &RecordKey, expected: &Revision) -> Result<DeleteResult>`
  - `FsStore` implements `delete_as`, not `delete`. In this task `delete_as` still calls the old physical `delete_locked(&root, &key, expected)` and ignores `author`; Task 2 replaces it.

- [ ] **Step 0: Create the slice branch from up-to-date `main`**

```bash
git checkout main
git pull --ff-only
git log --oneline -1 -- crates/gonzalo-core/src/tombstone.rs
git checkout -b feat/203-tombstones-fs-git
```

Expected: the `git log` line shows the slice 1 merge commit. If it prints nothing, slice 1 hasn't merged yet. Stop.

- [ ] **Step 1: Write the failing tests**

Create `crates/gonzalo-store-fs/tests/tombstones.rs`:

```rust
//! fs-specific tombstone behaviour (gonzalo#203, spec §3.3): where tombstones
//! live on disk, how `purge` unlinks them, and how the ancestor cap is wired.
//! The substrate-independent semantics are covered by
//! `run_tombstone_conformance` in `tests/conformance.rs`.

use gonzalo_core::{
    Body, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;

fn rec(key: &RecordKey, payload: &[u8], revision: Revision) -> Record {
    Record {
        key: key.clone(),
        kind: RecordKind::Topic,
        revision,
        parent: None,
        body: Body::Inline(payload.to_vec()),
        meta: Meta {
            author: Identity::new("t"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        ancestors: Vec::new(),
        deleted_at: None,
    }
}

fn committed(result: PutResult) -> Revision {
    match result {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    }
}

#[test]
fn with_ancestor_cap_rejects_zero() {
    assert!(
        FsStore::new("/nonexistent-gonzalo-root")
            .with_ancestor_cap(0)
            .is_err()
    );
    assert!(
        FsStore::new("/nonexistent-gonzalo-root")
            .with_ancestor_cap(1)
            .is_ok()
    );
}

/// Six writes to `key`, each conditional on the previous revision, through
/// `put_raw` when `raw` is true and consumer `put` otherwise. Returns every
/// committed revision, oldest first.
async fn write_chain(store: &FsStore, key: &RecordKey, raw: bool) -> Vec<Revision> {
    let mut history: Vec<Revision> = Vec::new();
    for i in 0..=5u32 {
        let body = format!("v{i}");
        let (revision, expected) = match history.last() {
            None => (Revision::initial(body.as_bytes()), None),
            Some(prev) => (prev.next(body.as_bytes()), Some(prev.clone())),
        };
        let record = rec(key, body.as_bytes(), revision.clone());
        let result = if raw {
            store.put_raw(record, expected).await.unwrap()
        } else {
            store.put(record, expected).await.unwrap()
        };
        let stored = committed(result);
        // Neither path re-stamps a write over a live record.
        assert_eq!(stored, revision);
        history.push(stored);
    }
    history
}

#[tokio::test]
async fn put_folds_ancestors_to_the_configured_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path()).with_ancestor_cap(3).unwrap();
    let key = RecordKey::new("ns", "col", "capped");
    let history = write_chain(&store, &key, false).await;

    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw.revision, history[5]);
    // Newest first, own revision excluded, truncated to the cap of 3.
    assert_eq!(
        raw.ancestors,
        vec![history[4].clone(), history[3].clone(), history[2].clone()]
    );
}

#[tokio::test]
async fn put_raw_folds_ancestors_to_the_configured_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path()).with_ancestor_cap(3).unwrap();
    let key = RecordKey::new("ns", "col", "capped-raw");
    let history = write_chain(&store, &key, true).await;

    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw.revision, history[5]);
    assert_eq!(
        raw.ancestors,
        vec![history[4].clone(), history[3].clone(), history[2].clone()]
    );
}

#[tokio::test]
async fn purge_removes_the_record_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "gone");
    let rev = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    let path = dir.path().join("ns").join("col").join("gone.json");
    assert!(path.exists());

    assert_eq!(store.purge(&key, rev).await.unwrap(), DeleteResult::Deleted);
    assert!(!path.exists(), "purge must unlink the record file");
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
    assert!(
        !store
            .list_raw(&KeyPrefix::default())
            .await
            .unwrap()
            .contains(&key)
    );
}

#[tokio::test]
async fn purge_with_stale_expected_conflicts_and_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "kept");
    let rev = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    let wrong = Revision::initial(b"never-current");

    match store.purge(&key, wrong).await.unwrap() {
        DeleteResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, rev);
        }
        DeleteResult::Deleted => panic!("stale purge must conflict"),
    }
    assert!(dir.path().join("ns").join("col").join("kept.json").exists());
}

#[tokio::test]
async fn purge_of_absent_key_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "never");
    assert_eq!(
        store
            .purge(&key, Revision::initial(b"anything"))
            .await
            .unwrap(),
        DeleteResult::Deleted
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-store-fs --test tombstones`
Expected: compile FAIL with `error[E0599]: no method named `with_ancestor_cap` found for struct `FsStore``. Once that compiles, `put_raw_folds_ancestors_to_the_configured_cap` would still fail against slice 1's interim `put_raw`, which stores the record verbatim with empty `ancestors`. The three `purge_*` tests describe today's physical conditional delete, which slice 1's interim `purge` already provides. They are characterization tests that keep that behaviour pinned while `purge` moves onto `plan_purge`.

- [ ] **Step 3: Implement the cap, the shared critical-section helpers, `put_locked` (for `put` and `put_raw`) and `purge_locked`**

In `crates/gonzalo-store-fs/src/lib.rs`, add these names to the existing `use gonzalo_core::{ ... };` list (keep `store::Conflict`, which the old `delete_locked` still uses until Task 2): `DEFAULT_ANCESTOR_CAP, Identity, PurgePlan, PutPlan, plan_purge, plan_put, plan_put_raw, validate_ancestor_cap`.

Add this type alias directly below the `use` block:

```rust
/// A put planner: `gonzalo_core::plan_put` (consumer write) or
/// `gonzalo_core::plan_put_raw` (replication write). Both run inside the same
/// locked read→plan→write path, `put_locked`.
type PutPlanner = fn(Option<&Record>, Record, Option<Revision>, usize) -> PutPlan;
```

Replace the struct and `new` (`:19-27`):

```rust
/// A `Store` backed by JSON files under a root directory.
pub struct FsStore {
    root: PathBuf,
    /// Upper bound on `Record::ancestors` for every record this store writes
    /// (spec §3.9). Defaults to `DEFAULT_ANCESTOR_CAP`.
    cap: usize,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cap: DEFAULT_ANCESTOR_CAP,
        }
    }

    /// Bound the ancestor list this store keeps on every write (spec §3.9).
    /// A cap of `0` is rejected.
    pub fn with_ancestor_cap(mut self, cap: usize) -> Result<Self> {
        self.cap = validate_ancestor_cap(cap)?;
        Ok(self)
    }
```

(`read_record` and the closing `}` of the `impl FsStore` block stay as they are.)

Replace the whole `#[async_trait] impl Store for FsStore { ... }` block, including slice 1's interim `get_raw`/`list_raw`/`purge`, with:

```rust
#[async_trait]
impl Store for FsStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.read_record(key).await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // The OCC read-check-write-rename is a critical section: without
        // serialization a concurrent writer can commit between our read and our
        // rename, silently losing an update. Hold a per-record advisory file
        // lock (flock) across the whole section so writers — in this process or
        // another — serialize. flock is blocking, so run it on a blocking
        // thread rather than stalling the async runtime.
        let root = self.root.clone();
        let cap = self.cap;
        tokio::task::spawn_blocking(move || put_locked(&root, record, expected, cap, plan_put))
            .await
            .map_err(|e| CoreError::Backend(format!("put task panicked: {e}")))?
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Replication write: the same per-record flock critical section as
        // `put`, decided by `plan_put_raw`, which never re-stamps. A create
        // (`expected == None`) over a tombstone is a Conflict, never a
        // recreation.
        let root = self.root.clone();
        let cap = self.cap;
        tokio::task::spawn_blocking(move || put_locked(&root, record, expected, cap, plan_put_raw))
            .await
            .map_err(|e| CoreError::Backend(format!("put_raw task panicked: {e}")))?
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let mut out = Vec::new();
        collect_keys(&self.root, prefix, &mut out).await?;
        Ok(out)
    }

    // No `delete` here: the trait provides it as `delete_as(key, expected, None)`.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        _author: Option<Identity>,
    ) -> Result<DeleteResult> {
        // Still the physical conditional delete until Task 2 switches to
        // tombstones (which is where the author gets stamped). Mirror `put`'s
        // critical section: hold the per-record flock so the read→check→remove
        // is atomic against a concurrent writer. Blocking, so run it on a
        // blocking thread rather than stalling the async runtime.
        let root = self.root.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || delete_locked(&root, &key, expected))
            .await
            .map_err(|e| CoreError::Backend(format!("delete task panicked: {e}")))?
    }

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        // Replication read: returns whatever is stored, tombstones included.
        self.read_record(key).await
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Replication listing: every `<id>.json` under the prefix, tombstones
        // included, without reading any file.
        let mut out = Vec::new();
        collect_keys(&self.root, prefix, &mut out).await?;
        Ok(out)
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        // The only physical removal in the system. It runs in the same
        // per-record flock critical section as `put`, so a concurrent
        // recreation either lands before our read (→ Conflict) or after our
        // unlink (→ a fresh record). Blocking, so run it on a blocking thread.
        let root = self.root.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || purge_locked(&root, &key, &expected))
            .await
            .map_err(|e| CoreError::Backend(format!("purge task panicked: {e}")))?
    }
}
```

Replace `put_locked` (`:193-257`, from its doc comment `/// Perform the conditional `put` under a per-record advisory lock.` through its closing `}`) with the helpers plus the new `put_locked` and `purge_locked`:

```rust
/// Take the exclusive per-record advisory lock for the record file at `path`,
/// creating its collection directory first. Blocking by design; call from
/// `spawn_blocking`.
///
/// The lock is a sibling `<id>.json.lock` file held exclusively via `flock`,
/// released when the returned handle drops. It guards only writers — `get`/
/// `list` stay lock-free — which is sufficient: the lost update is a
/// write/write race, and every publish is an atomic `rename` (or `unlink`), so
/// readers never observe a torn file. The `.lock` file is left in place and
/// reused by the next writer.
fn lock_record(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
    }
    let lock_path = path.with_extension("json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    flock(&lock, FlockOperation::LockExclusive).map_err(|e| CoreError::Backend(e.to_string()))?;
    Ok(lock)
}

/// The record stored at `path` right now, tombstones included. Called inside
/// the caller's critical section.
fn read_current(path: &Path) -> Result<Option<Record>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Record>(&bytes)
            .map(Some)
            .map_err(|e| CoreError::Serde(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CoreError::Backend(e.to_string())),
    }
}

/// Durably and atomically publish `record` at `path`. Called under
/// `lock_record`.
fn write_durable(path: &Path, record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(|e| CoreError::Serde(e.to_string()))?;
    // Durable atomic write: write the temp file and `sync_all` it so its bytes
    // reach disk BEFORE the rename, then fsync the parent directory AFTER the
    // rename so the new directory entry survives a crash too. `rename` is atomic
    // against concurrent readers but not against power loss — on ext4 delayed
    // allocation a crash just after a reported Committed can otherwise leave a
    // zero-length or truncated record.
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::File::create(&tmp).map_err(|e| CoreError::Backend(e.to_string()))?;
    f.write_all(&bytes)
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    f.sync_all()
        .map_err(|e| CoreError::Backend(e.to_string()))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| CoreError::Backend(e.to_string()))?;
    if let Some(parent) = path.parent() {
        fsync_dir(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
    }
    Ok(())
}

/// Durably unlink the record file at `path`: remove it, then fsync the parent
/// directory so the removal survives a crash. Called under `lock_record`.
fn remove_durable(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        // A concurrent remover won under the lock hand-off — still absent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CoreError::Backend(e.to_string())),
    }
    if let Some(parent) = path.parent() {
        fsync_dir(parent).map_err(|e| CoreError::Backend(e.to_string()))?;
    }
    Ok(())
}

/// Perform a conditional `put` or `put_raw` under the per-record lock
/// (`lock_record`). Blocking by design (held across read→plan→write→rename);
/// call from `spawn_blocking`.
///
/// The decision is the planner's: `plan_put` for consumer writes, which
/// re-stamps a recreation over a tombstone, or `plan_put_raw` for
/// replication writes, which never re-stamps. Both fold ancestors to `cap`, so
/// every substrate behaves identically (spec §3.2). This function only carries
/// the plan out.
fn put_locked(
    root: &Path,
    record: Record,
    expected: Option<Revision>,
    cap: usize,
    plan: PutPlanner,
) -> Result<PutResult> {
    let key = record.key.clone();
    let path = layout::record_path(root, &key);
    // Acquire the exclusive lock; it lives until `_lock` drops at function end.
    let _lock = lock_record(&path)?;

    // Critical section: the read, the decision and the write are serialized
    // per record.
    let current = read_current(&path)?;
    match plan(current.as_ref(), record, expected, cap) {
        PutPlan::Write(stored) => {
            write_durable(&path, &stored)?;
            Ok(PutResult::Committed(stored.revision))
        }
        PutPlan::Conflict(conflict) => Ok(PutResult::Conflict(conflict)),
        // `expected` named a revision, but nothing live (and no tombstone at
        // that revision) is stored.
        PutPlan::NotFound => Err(CoreError::NotFound(key)),
    }
}

/// Perform the conditional `purge` under the per-record lock: physically
/// unlink the record file (live or tombstone) only if its current revision is
/// `expected` (`gonzalo_core::plan_purge`). This is the only physical removal
/// of a record file. Blocking; call from `spawn_blocking`.
fn purge_locked(root: &Path, key: &RecordKey, expected: &Revision) -> Result<DeleteResult> {
    let path = layout::record_path(root, key);
    // Acquire the exclusive lock; it lives until `_lock` drops at function end.
    let _lock = lock_record(&path)?;

    // Critical section: the revision check and the unlink are serialized per
    // record.
    let current = read_current(&path)?;
    match plan_purge(current.as_ref(), expected) {
        PurgePlan::Remove => {
            remove_durable(&path)?;
            Ok(DeleteResult::Deleted)
        }
        PurgePlan::Noop => Ok(DeleteResult::Deleted),
        PurgePlan::Conflict(conflict) => Ok(DeleteResult::Conflict(conflict)),
    }
}
```

Leave `delete_locked` (`:259-318`) unchanged in this task. Task 2 rewrites it.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-store-fs --test tombstones`
Expected: PASS, `test result: ok. 6 passed; 0 failed`.

Run: `cargo test -p gonzalo-store-fs --test conformance --test concurrent_put_no_lost_update --test list_ignores_stray_files`
Expected: PASS for all three binaries.

- [ ] **Step 5: Format, lint the crate, commit**

```bash
cargo fmt --all
cargo clippy -p gonzalo-store-fs --all-targets -- -D warnings
git add crates/gonzalo-store-fs/src/lib.rs crates/gonzalo-store-fs/tests/tombstones.rs
git commit -m "feat(store-fs): ancestor cap; put and purge via core planners (#203)" -m "Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

Expected: clippy prints no warnings, and the commit succeeds.

---

### Task 2: fs `delete_as` writes tombstones; consumer reads hide them

**Files:**
- Modify: `crates/gonzalo-store-fs/src/lib.rs` (imports, `impl Store` methods `get` / `list` / `delete_as`, `delete_locked` `:259-318`, and a new `listed_live` placed before `collect_keys`)
- Modify: `crates/gonzalo-store-fs/tests/tombstones.rs`

**Interfaces:**
- Consumes: Task 1's `lock_record`, `read_current`, `write_durable`, `put_locked` / `put_raw` and `FsStore.cap`, plus slice 1's `gonzalo_core::{plan_delete, DeletePlan, now_ms, tombstone_hash, Identity}`, `Record::is_tombstone`, and the trait-provided `Store::delete`.
- Produces:
  - `fn delete_locked(root: &Path, key: &RecordKey, expected: Option<Revision>, cap: usize, author: Option<Identity>) -> Result<DeleteResult>`
  - `async fn listed_live(root: &Path, key: &RecordKey) -> Result<bool>`
  - Final fs `Store` semantics per the overview contract.

- [ ] **Step 1: Write the failing tests**

In `crates/gonzalo-store-fs/tests/tombstones.rs`, replace the `use` block at the top with:

```rust
use gonzalo_core::{
    Body, CoreError, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey,
    RecordKind, Revision, Store, tombstone_hash,
};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;
use std::sync::Arc;
```

Append these tests to the end of the file:

```rust
#[tokio::test]
async fn delete_writes_a_tombstone_at_the_record_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "doomed");
    let rev0 = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );

    assert_eq!(
        store
            .delete_as(&key, Some(rev0.clone()), Some(Identity::new("deleter")))
            .await
            .unwrap(),
        DeleteResult::Deleted
    );

    let path = dir.path().join("ns").join("col").join("doomed.json");
    let bytes = std::fs::read(&path).expect("the tombstone stays at the record path");
    let on_disk: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(on_disk["kind"], "Tombstone");
    assert!(on_disk["deleted_at"].is_i64());

    let tomb: Record = serde_json::from_slice(&bytes).unwrap();
    assert!(tomb.is_tombstone());
    assert_eq!(
        tomb.revision,
        Revision {
            counter: rev0.counter + 1,
            hash: tombstone_hash(),
        }
    );
    assert_eq!(tomb.parent, Some(rev0.clone()));
    assert_eq!(tomb.ancestors, vec![rev0]);
    // `delete_as` stamps the deleting principal onto the tombstone.
    assert_eq!(tomb.meta.author, Identity::new("deleter"));
    // The atomic temp+rename publish leaves no temp file behind.
    assert!(!path.with_extension("json.tmp").exists());
}

#[tokio::test]
async fn consumer_reads_hide_a_tombstone_and_raw_reads_show_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let dead = RecordKey::new("ns", "col", "dead");
    let live = RecordKey::new("ns", "col", "live");
    committed(
        store
            .put(rec(&dead, b"d", Revision::initial(b"d")), None)
            .await
            .unwrap(),
    );
    committed(
        store
            .put(rec(&live, b"l", Revision::initial(b"l")), None)
            .await
            .unwrap(),
    );
    assert_eq!(
        store.delete(&dead, None).await.unwrap(),
        DeleteResult::Deleted
    );

    assert_eq!(store.get(&dead).await.unwrap(), None);
    assert!(store.get_raw(&dead).await.unwrap().unwrap().is_tombstone());

    let all = KeyPrefix::default();
    assert_eq!(store.list(&all).await.unwrap(), vec![live.clone()]);
    let raw = store.list_raw(&all).await.unwrap();
    assert_eq!(raw.len(), 2);
    assert!(raw.contains(&dead) && raw.contains(&live));
}

#[tokio::test]
async fn purge_removes_a_tombstone_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "collected");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    let tomb = store.get_raw(&key).await.unwrap().unwrap();
    let path = dir.path().join("ns").join("col").join("collected.json");
    assert!(path.exists());

    assert_eq!(
        store.purge(&key, tomb.revision).await.unwrap(),
        DeleteResult::Deleted
    );
    assert!(!path.exists());
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
}

/// Consumer `list` must read each file to filter tombstones, but a `*.json`
/// that doesn't parse as a `Record` stays listed exactly as it was before
/// tombstones, and `get` keeps surfacing the parse error loudly.
#[tokio::test]
async fn list_keeps_an_unparseable_record_file_listed() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let good = RecordKey::new("ns", "col", "good");
    committed(
        store
            .put(rec(&good, b"g", Revision::initial(b"g")), None)
            .await
            .unwrap(),
    );
    std::fs::write(
        dir.path().join("ns").join("col").join("garbage.json"),
        b"not json",
    )
    .unwrap();
    let garbage = RecordKey::new("ns", "col", "garbage");

    let listed = store.list(&KeyPrefix::default()).await.unwrap();
    assert!(listed.contains(&good));
    assert!(listed.contains(&garbage));
    assert!(matches!(
        store.get(&garbage).await,
        Err(CoreError::Serde(_))
    ));
}

/// Deletes go through the per-record lock: N racing deletes of one revision
/// all report `Deleted` with no errors. Exactly one tombstone is written; the
/// rest see it and no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deletes_of_one_revision_write_one_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsStore::new(dir.path()));
    let key = RecordKey::new("ns", "col", "raced");
    let base = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );

    let mut handles = Vec::new();
    for _ in 0..16 {
        let store = Arc::clone(&store);
        let key = key.clone();
        let expected = base.clone();
        handles.push(tokio::spawn(async move {
            store.delete(&key, Some(expected)).await
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap().unwrap(), DeleteResult::Deleted);
    }

    let tomb = store.get_raw(&key).await.unwrap().unwrap();
    assert!(tomb.is_tombstone());
    assert_eq!(tomb.revision.counter, base.counter + 1);
    assert_eq!(tomb.parent, Some(base));
}

/// Consumer `put` treats a tombstoned key as absent: a conditional write
/// naming any revision (even the tombstone's own) is `NotFound`, and an
/// unconditional write is a re-stamped recreation.
#[tokio::test]
async fn consumer_put_over_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "reborn");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    let tomb = store.get_raw(&key).await.unwrap().unwrap();

    assert!(matches!(
        store
            .put(rec(&key, b"y", Revision::initial(b"y")), Some(tomb.revision.clone()))
            .await,
        Err(CoreError::NotFound(_))
    ));

    let recreated = committed(
        store
            .put(rec(&key, b"y", Revision::initial(b"y")), None)
            .await
            .unwrap(),
    );
    assert_eq!(recreated.counter, tomb.revision.counter + 1);
    let stored = store.get(&key).await.unwrap().unwrap();
    assert_eq!(stored.parent, Some(tomb.revision));
}

/// Replication `put_raw` never re-stamps: a create over a tombstone conflicts
/// with the tombstone as `current`, and a write conditional on the
/// tombstone's revision stores the caller's revision verbatim.
#[tokio::test]
async fn put_raw_over_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let key = RecordKey::new("ns", "col", "replicated");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    let tomb = store.get_raw(&key).await.unwrap().unwrap();

    match store
        .put_raw(rec(&key, b"y", Revision::initial(b"y")), None)
        .await
        .unwrap()
    {
        PutResult::Conflict(c) => assert!(c.current.is_tombstone()),
        PutResult::Committed(rev) => panic!("create over a tombstone must conflict, got {rev:?}"),
    }
    // The conflicting create wrote nothing.
    assert_eq!(store.get_raw(&key).await.unwrap().unwrap(), tomb);

    let incoming = Revision {
        counter: 7,
        hash: gonzalo_core::ContentHash::of(b"peer"),
    };
    assert_eq!(
        committed(
            store
                .put_raw(rec(&key, b"peer", incoming.clone()), Some(tomb.revision.clone()))
                .await
                .unwrap()
        ),
        incoming
    );
    let stored = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(stored.revision, incoming);
    assert!(stored.ancestors.contains(&tomb.revision));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-store-fs --test tombstones`
Expected: FAIL.
- `delete_writes_a_tombstone_at_the_record_path` panics with `the tombstone stays at the record path`.
- `consumer_reads_hide_a_tombstone_and_raw_reads_show_it`, `purge_removes_a_tombstone_file`, `concurrent_deletes_of_one_revision_write_one_tombstone`, `consumer_put_over_a_tombstone` and `put_raw_over_a_tombstone` panic on `unwrap()` of `None` from `get_raw`, because the interim `delete_as` still removes the file.
- `list_keeps_an_unparseable_record_file_listed` already passes. It is a guard for the filter added in Step 3.

- [ ] **Step 3: Implement tombstone deletes and consumer filtering**

In the `use gonzalo_core::{ ... };` list, add `DeletePlan, now_ms, plan_delete` and **remove** `store::Conflict`, which nothing uses after this step.

In `impl Store for FsStore`, replace the `get`, `list` and `delete_as` methods with the following. There must be no `delete` method in the impl, because the trait provides it.

```rust
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        // Consumer read: a tombstoned key looks absent (spec §3.2).
        Ok(self
            .read_record(key)
            .await?
            .filter(|rec| !rec.is_tombstone()))
    }
```

```rust
    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Consumer listing excludes tombstoned keys, which means reading each
        // record file under the prefix (spec §8.4: a local read per key on fs).
        let mut keys = Vec::new();
        collect_keys(&self.root, prefix, &mut keys).await?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if listed_live(&self.root, &key).await? {
                out.push(key);
            }
        }
        Ok(out)
    }
```

```rust
    // No `delete` here: the trait provides it as `delete_as(key, expected, None)`.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        // Mirror `put`'s critical section: hold the per-record flock so the
        // read→plan→tombstone-write is atomic against a concurrent writer.
        // Blocking, so run it on a blocking thread rather than stalling the
        // async runtime.
        let root = self.root.clone();
        let key = key.clone();
        let cap = self.cap;
        tokio::task::spawn_blocking(move || delete_locked(&root, &key, expected, cap, author))
            .await
            .map_err(|e| CoreError::Backend(format!("delete task panicked: {e}")))?
    }
```

(`put`, `put_raw`, `get_raw`, `list_raw` and `purge` stay as Task 1 left them.)

Check that no `delete` override remains:

Run: `rg -n 'async fn delete\(' crates/gonzalo-store-fs/src/lib.rs`
Expected: no output.

Replace `delete_locked` (`:259-318`, from `/// Perform the conditional `delete` under the same per-record advisory lock` through its closing `}`) with:

```rust
/// Perform the conditional `delete` under the per-record lock. Blocking by
/// design; call from `spawn_blocking`.
///
/// A delete no longer removes anything (spec §3.3). Over a live record it
/// durably publishes the tombstone `gonzalo_core::plan_delete` builds, at the
/// record's normal path, through the same temp+fsync+rename as `put`. Over an
/// absent key or an existing tombstone it writes nothing and reports `Deleted`.
/// A stale `expected` over a live record is a `Conflict`. `Some(author)`
/// replaces `meta.author` on the tombstone. Physical removal is
/// `purge_locked`'s job alone.
fn delete_locked(
    root: &Path,
    key: &RecordKey,
    expected: Option<Revision>,
    cap: usize,
    author: Option<Identity>,
) -> Result<DeleteResult> {
    let path = layout::record_path(root, key);
    // Acquire the exclusive lock; it lives until `_lock` drops at function end.
    let _lock = lock_record(&path)?;

    // Critical section: the read, the decision and the tombstone write are
    // serialized per record.
    let current = read_current(&path)?;
    match plan_delete(current.as_ref(), expected, now_ms(), cap, author.as_ref()) {
        DeletePlan::Write(tombstone) => {
            write_durable(&path, &tombstone)?;
            Ok(DeleteResult::Deleted)
        }
        DeletePlan::Noop => Ok(DeleteResult::Deleted),
        DeletePlan::Conflict(conflict) => Ok(DeleteResult::Conflict(conflict)),
    }
}
```

Insert `listed_live` directly above `/// Walk `<root>/<ns>/<col>/<id>.json` and collect keys matching `prefix`.`:

```rust
/// Whether consumer `list` reports `key`. A tombstone is hidden. A file that
/// vanished since the directory walk (a concurrent `purge`) is dropped. A file
/// that doesn't parse as a `Record` stays listed, exactly as before tombstones,
/// so `get` keeps surfacing the `Serde` error instead of the key silently
/// disappearing.
async fn listed_live(root: &Path, key: &RecordKey) -> Result<bool> {
    match tokio::fs::read(layout::record_path(root, key)).await {
        Ok(bytes) => Ok(serde_json::from_slice::<Record>(&bytes)
            .map(|rec| !rec.is_tombstone())
            .unwrap_or(true)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(CoreError::Backend(e.to_string())),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-store-fs --test tombstones`
Expected: PASS, `test result: ok. 13 passed; 0 failed`.

Run: `cargo test -p gonzalo-store-fs`
Expected: PASS for every test binary in the crate, including `conformance` (slice 1's rewritten delete cases now exercise real tombstones), `list_ignores_stray_files` and `concurrent_put_no_lost_update`.

- [ ] **Step 5: Format, lint the crate, commit**

```bash
cargo fmt --all
cargo clippy -p gonzalo-store-fs --all-targets -- -D warnings
git add crates/gonzalo-store-fs/src/lib.rs crates/gonzalo-store-fs/tests/tombstones.rs
git commit -m "feat(store-fs): delete writes tombstones; consumer reads hide them (#203)" -m "Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

Expected: no clippy warnings (in particular no `unused import: store::Conflict`), and the commit succeeds.

---

### Task 3: Wire tombstone conformance into the fs store

**Files:**
- Modify: `crates/gonzalo-store-fs/tests/conformance.rs` (whole file, `:1-14`)

**Interfaces:**
- Consumes: `gonzalo_core::conformance::run_tombstone_conformance<S, F, Fut>(factory: F, cap: usize)`, `gonzalo_core::DEFAULT_ANCESTOR_CAP`, and `FsStore::with_ancestor_cap` (Task 1).
- Produces: the test functions `fs_store_passes_tombstone_conformance_default_cap` and `fs_store_passes_tombstone_conformance_small_cap`.

- [ ] **Step 1: Write the conformance tests**

Replace `crates/gonzalo-store-fs/tests/conformance.rs` in full:

```rust
use gonzalo_core::DEFAULT_ANCESTOR_CAP;
use gonzalo_core::conformance::{run_store_conformance, run_tombstone_conformance};
use gonzalo_store_fs::FsStore;

/// A small cap makes `ancestors_capped_and_ordered` exercise truncation after a
/// handful of updates instead of 32+.
const SMALL_CAP: usize = 3;

/// A fresh, empty store root. The TempDir is leaked so the directory survives
/// for the store's lifetime within a single factory invocation; the OS
/// reclaims /tmp on reboot.
fn fresh_root() -> std::path::PathBuf {
    tempfile::tempdir().expect("tempdir").keep()
}

#[tokio::test]
async fn fs_store_passes_conformance() {
    run_store_conformance(|| async { FsStore::new(fresh_root()) }).await;
}

#[tokio::test]
async fn fs_store_passes_tombstone_conformance_default_cap() {
    run_tombstone_conformance(
        || async { FsStore::new(fresh_root()) },
        DEFAULT_ANCESTOR_CAP,
    )
    .await;
}

#[tokio::test]
async fn fs_store_passes_tombstone_conformance_small_cap() {
    run_tombstone_conformance(
        || async {
            FsStore::new(fresh_root())
                .with_ancestor_cap(SMALL_CAP)
                .expect("cap 3 is valid")
        },
        SMALL_CAP,
    )
    .await;
}
```

- [ ] **Step 2: Run the conformance tests**

Run: `cargo test -p gonzalo-store-fs --test conformance`
Expected: PASS, `test result: ok. 3 passed; 0 failed`. Tasks 1–2 already implemented the behaviour, so these pass on the first run. They prove that the fs store follows the planner decision tables. If a case fails, its panic message names the §6.1 case. Fix `src/lib.rs` (never the suite, which belongs to slice 1) and re-run.

To confirm the wiring really runs the suite, temporarily change `SMALL_CAP` to `4` in the `run_tombstone_conformance(..., SMALL_CAP)` argument only (leave the builder at 3), then run
`cargo test -p gonzalo-store-fs --test conformance fs_store_passes_tombstone_conformance_small_cap`.
Expected: FAIL in `ancestors_capped_and_ordered` (length 3 ≠ 4). Revert the change and re-run. Expected: PASS.

- [ ] **Step 3: Format, commit**

```bash
cargo fmt --all
git add crates/gonzalo-store-fs/tests/conformance.rs
git commit -m "test(store-fs): run tombstone conformance at default and small caps (#203)" -m "Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 4: git ancestor cap, `put`/`put_raw` through the put planners, `purge` takes over `commit_removal`

**Files:**
- Modify: `crates/gonzalo-store-git/src/lib.rs` (imports `:12-21`, `GitStore` struct and `open` `:47-63`, new helpers inside `impl GitStore`, `lock_repo` doc `:170-178`, the whole `impl gonzalo_core::Store for GitStore` `:513-608`)
- Create: `crates/gonzalo-store-git/tests/tombstones.rs`

**Interfaces:**
- Consumes (slice 1): `gonzalo_core::{DEFAULT_ANCESTOR_CAP, validate_ancestor_cap, plan_put, plan_put_raw, PutPlan, plan_purge, PurgePlan, Identity}`, plus the required trait methods `get_raw` / `list_raw` / `put_raw` / `delete_as` / `purge`.
- Produces:
  - `pub fn GitStore::with_ancestor_cap(self, cap: usize) -> gonzalo_core::Result<GitStore>`
  - Private crate-level `type PutPlanner = fn(Option<&Record>, Record, Option<Revision>, usize) -> PutPlan;`
  - `GitStore` implements `delete_as`, not `delete`. In this task `delete_as` still does the physical conditional removal and ignores `author`; Task 5 replaces it.
  - Private, in `impl GitStore`, used by Task 5:
    - `fn handle(&self) -> GitStore`
    - `fn put_locked(&self, record: Record, expected: Option<Revision>, plan: PutPlanner) -> Result<PutResult>`
    - `fn write_and_commit(&self, record: &Record, message: &str) -> Result<()>`
    - `fn remove_and_commit(&self, key: &RecordKey, message: &str) -> Result<()>`
  - Private free fn `fn rel_path(key: &RecordKey) -> PathBuf`
  - Commit message convention: `put {key}`, `delete {key}`, `purge {key}`

- [ ] **Step 1: Write the failing tests**

Create `crates/gonzalo-store-git/tests/tombstones.rs`:

```rust
//! git-specific tombstone behaviour (gonzalo#203, spec §3.3): a delete commits
//! a tombstone file at the record path, a no-op delete makes no commit, and
//! `purge` commits the removal. The substrate-independent semantics are
//! covered by `run_tombstone_conformance` in `tests/conformance.rs`.

use std::collections::BTreeMap;
use std::path::Path;

use gonzalo_core::{
    Body, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
};
use gonzalo_store_git::GitStore;

fn rec(key: &RecordKey, payload: &[u8], revision: Revision) -> Record {
    Record {
        key: key.clone(),
        kind: RecordKind::Topic,
        revision,
        parent: None,
        body: Body::Inline(payload.to_vec()),
        meta: Meta {
            author: Identity::new("t"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        ancestors: Vec::new(),
        deleted_at: None,
    }
}

fn committed(result: PutResult) -> Revision {
    match result {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    }
}

/// `(HEAD commit id, HEAD commit message)` of the repo at `root`.
fn head_commit_of(root: &Path) -> (git2::Oid, String) {
    let repo = git2::Repository::open(root).unwrap();
    let commit = repo.head().unwrap().peel_to_commit().unwrap();
    (commit.id(), commit.message().unwrap_or_default().to_string())
}

/// The bytes committed at `rel` in HEAD's tree, or `None` if the path is absent.
fn head_tree_bytes(root: &Path, rel: &str) -> Option<Vec<u8>> {
    let repo = git2::Repository::open(root).unwrap();
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    let entry = tree.get_path(Path::new(rel)).ok()?;
    let object = entry.to_object(&repo).unwrap();
    Some(object.peel_to_blob().unwrap().content().to_vec())
}

#[test]
fn with_ancestor_cap_rejects_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        GitStore::open(dir.path())
            .unwrap()
            .with_ancestor_cap(0)
            .is_err()
    );
    assert!(
        GitStore::open(dir.path())
            .unwrap()
            .with_ancestor_cap(1)
            .is_ok()
    );
}

/// Six writes to `key`, each conditional on the previous revision, through
/// `put_raw` when `raw` is true and consumer `put` otherwise. Returns every
/// committed revision, oldest first.
async fn write_chain(store: &GitStore, key: &RecordKey, raw: bool) -> Vec<Revision> {
    let mut history: Vec<Revision> = Vec::new();
    for i in 0..=5u32 {
        let body = format!("v{i}");
        let (revision, expected) = match history.last() {
            None => (Revision::initial(body.as_bytes()), None),
            Some(prev) => (prev.next(body.as_bytes()), Some(prev.clone())),
        };
        let record = rec(key, body.as_bytes(), revision.clone());
        let result = if raw {
            store.put_raw(record, expected).await.unwrap()
        } else {
            store.put(record, expected).await.unwrap()
        };
        let stored = committed(result);
        // Neither path re-stamps a write over a live record.
        assert_eq!(stored, revision);
        history.push(stored);
    }
    history
}

#[tokio::test]
async fn put_raw_folds_ancestors_to_the_configured_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path())
        .unwrap()
        .with_ancestor_cap(3)
        .unwrap();
    let key = RecordKey::new("ns", "col", "capped-raw");
    let history = write_chain(&store, &key, true).await;

    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw.revision, history[5]);
    assert_eq!(
        raw.ancestors,
        vec![history[4].clone(), history[3].clone(), history[2].clone()]
    );
    let (_, message) = head_commit_of(dir.path());
    assert!(message.starts_with("put "), "got commit message {message:?}");
}

#[tokio::test]
async fn put_folds_ancestors_to_the_configured_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path())
        .unwrap()
        .with_ancestor_cap(3)
        .unwrap();
    let key = RecordKey::new("ns", "col", "capped");
    let history = write_chain(&store, &key, false).await;

    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw.revision, history[5]);
    assert_eq!(
        raw.ancestors,
        vec![history[4].clone(), history[3].clone(), history[2].clone()]
    );
    // The committed file carries the same folded ancestors as the worktree.
    let committed_rec: Record =
        serde_json::from_slice(&head_tree_bytes(dir.path(), "ns/col/capped.json").unwrap())
            .unwrap();
    assert_eq!(committed_rec.ancestors, raw.ancestors);
}

#[tokio::test]
async fn purge_commits_the_removal() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "gone");
    let rev = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert!(head_tree_bytes(dir.path(), "ns/col/gone.json").is_some());

    assert_eq!(store.purge(&key, rev).await.unwrap(), DeleteResult::Deleted);

    assert!(
        head_tree_bytes(dir.path(), "ns/col/gone.json").is_none(),
        "purge must commit the removal"
    );
    assert!(!dir.path().join("ns/col/gone.json").exists());
    let (_, message) = head_commit_of(dir.path());
    assert!(message.starts_with("purge "), "got commit message {message:?}");
    assert!(
        !store
            .list_raw(&KeyPrefix::default())
            .await
            .unwrap()
            .contains(&key)
    );
}

#[tokio::test]
async fn purge_with_stale_expected_conflicts_without_committing() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "kept");
    let rev = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    let (before, _) = head_commit_of(dir.path());

    match store
        .purge(&key, Revision::initial(b"never-current"))
        .await
        .unwrap()
    {
        DeleteResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, rev);
        }
        DeleteResult::Deleted => panic!("stale purge must conflict"),
    }
    assert_eq!(head_commit_of(dir.path()).0, before);
    assert!(head_tree_bytes(dir.path(), "ns/col/kept.json").is_some());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-store-git --test tombstones`
Expected: compile FAIL with `error[E0599]: no method named `with_ancestor_cap` found for struct `GitStore``. Once that compiles, `put_raw_folds_ancestors_to_the_configured_cap` would still fail against slice 1's interim `put_raw` (no ancestor folding). After Step 3 compiles, a still-wrong `purge` would fail `purge_commits_the_removal` on the `purge ` message prefix, because slice 1's interim purge reuses delete's `delete {key}` commit.

- [ ] **Step 3: Implement the cap, the commit helpers, and the planner-driven `put` and `purge`**

In `crates/gonzalo-store-git/src/lib.rs`:

- Add `DEFAULT_ANCESTOR_CAP, PurgePlan, PutPlan, plan_purge, plan_put, plan_put_raw, validate_ancestor_cap` to the existing `use gonzalo_core::{ ... };` list. `Identity` is already imported (`merged_record` uses it). Keep `store::Conflict`, which the interim `delete_as` still uses until Task 5.
- Directly below `pub use diff::{ChangedPaths, changed_paths, head_commit, is_git_repo};`, add:

```rust
/// A put planner: `gonzalo_core::plan_put` (consumer write) or
/// `gonzalo_core::plan_put_raw` (replication write). Both run inside the same
/// locked read→plan→write→commit path, `GitStore::put_locked`.
type PutPlanner = fn(Option<&Record>, Record, Option<Revision>, usize) -> PutPlan;
```
- Delete the line `use std::sync::Arc;` (`:21`). Only the old `get` used it.

Replace the struct and `open` (`:47-63`):

```rust
pub struct GitStore {
    root: PathBuf,
    /// Upper bound on `Record::ancestors` for every record this store commits
    /// (spec §3.9). Defaults to `DEFAULT_ANCESTOR_CAP`.
    cap: usize,
}

impl GitStore {
    /// Open an existing git repo at `root`, or initialize one if absent.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| CoreError::Backend(e.to_string()))?;
        match git2::Repository::open(&root) {
            Ok(_) => {}
            Err(_) => {
                git2::Repository::init(&root).map_err(|e| CoreError::Backend(e.to_string()))?;
            }
        }
        Ok(Self {
            root,
            cap: DEFAULT_ANCESTOR_CAP,
        })
    }

    /// Bound the ancestor list this store keeps on every write (spec §3.9).
    /// A cap of `0` is rejected.
    pub fn with_ancestor_cap(mut self, cap: usize) -> Result<Self> {
        self.cap = validate_ancestor_cap(cap)?;
        Ok(self)
    }

    /// An owned copy of this handle, to move into a `spawn_blocking` closure.
    fn handle(&self) -> Self {
        Self {
            root: self.root.clone(),
            cap: self.cap,
        }
    }

    /// Perform a conditional `put` or `put_raw`: take the repo lock, read the
    /// current record, let `plan` (`plan_put` or `plan_put_raw`) decide, and
    /// commit the planned record. Serializes the read→plan→write→commit
    /// critical section over the shared index+HEAD; the lock releases when
    /// `_lock` drops (all paths). Blocking; call from `run_blocking`.
    fn put_locked(
        &self,
        record: Record,
        expected: Option<Revision>,
        plan: PutPlanner,
    ) -> Result<PutResult> {
        let _lock = lock_repo(&self.root)?;
        let key = record.key.clone();
        let current = self.read(&key)?;
        // The decision (recreation re-stamping for `put`, verbatim for
        // `put_raw`, ancestor folding for both) is the shared core planner's
        // (spec §3.2).
        match plan(current.as_ref(), record, expected, self.cap) {
            PutPlan::Write(stored) => {
                self.write_and_commit(&stored, &format!("put {key}"))?;
                Ok(PutResult::Committed(stored.revision))
            }
            PutPlan::Conflict(conflict) => Ok(PutResult::Conflict(conflict)),
            PutPlan::NotFound => Err(CoreError::NotFound(key)),
        }
    }

    /// Write `record` to its worktree file and commit it with `message`.
    /// Call only while holding `lock_repo`.
    fn write_and_commit(&self, record: &Record, message: &str) -> Result<()> {
        let rel = rel_path(&record.key);
        let abs = self.root.join(&rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(be)?;
        }
        let bytes =
            serde_json::to_vec_pretty(record).map_err(|e| CoreError::Serde(e.to_string()))?;
        std::fs::write(&abs, &bytes).map_err(be)?;
        self.commit_file(&rel, message)
    }

    /// Remove `key`'s worktree file and commit the removal with `message`.
    /// Call only while holding `lock_repo`.
    fn remove_and_commit(&self, key: &RecordKey, message: &str) -> Result<()> {
        let rel = rel_path(key);
        match std::fs::remove_file(self.root.join(&rel)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(be(e)),
        }
        self.commit_removal(&rel, message)
    }
```

(`path_for`, `read`, `commit_file`, `commit_removal`, `pull`, `push` and the closing `}` of `impl GitStore` stay unchanged. The block above replaces only the struct through the end of `open`, and adds the four new methods right after `open`.)

Add this free function directly above `fn be<E: std::fmt::Display>(e: E) -> CoreError {`:

```rust
/// The repo-relative path of `key`'s record file: `<ns>/<col>/<id>.json`.
fn rel_path(key: &RecordKey) -> PathBuf {
    let (ns, col, file) = record_components(key);
    Path::new(&ns).join(&col).join(&file)
}
```

Replace the doc comment on `lock_repo` (`:170-178`) with:

```rust
/// Acquire the repo-level exclusive lock guarding the OCC critical section of
/// `put`, `delete` and `purge`.
///
/// Unlike `FsStore`, whose per-record lock suffices, every `GitStore` write
/// mutates the *shared* on-disk index and HEAD (via `commit_file` /
/// `commit_removal`), so serialization must be repo-wide: two writes on
/// different keys still race on the same index+HEAD. The lock is a
/// `<root>/.gonzalo-git.lock` file held exclusively via `flock`; it is released
/// when the returned handle drops, which covers every exit path (the
/// `Conflict`/`NotFound`/no-op early returns and any error). Blocking by
/// design — call only from the `spawn_blocking` section.
```

Replace the whole `#[async_trait] impl gonzalo_core::Store for GitStore { ... }` block (`:513-608`, plus slice 1's interim raw/purge methods) with:

```rust
#[async_trait]
impl gonzalo_core::Store for GitStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || store.read(&key)).await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let store = self.handle();
        run_blocking(move || store.put_locked(record, expected, plan_put)).await
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Replication write: same repo-wide critical section as `put`, decided
        // by `plan_put_raw`, which never re-stamps. A create over a tombstone is
        // a Conflict, never a recreation.
        let store = self.handle();
        run_blocking(move || store.put_locked(record, expected, plan_put_raw)).await
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let store = self.handle();
        let prefix = prefix.clone();
        run_blocking(move || {
            let mut out = Vec::new();
            collect_keys(&store.root, &prefix, &mut out)?;
            Ok(out)
        })
        .await
    }

    // No `delete` here: the trait provides it as `delete_as(key, expected, None)`.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        _author: Option<Identity>,
    ) -> Result<DeleteResult> {
        // Still the physical conditional delete until Task 5 switches to
        // tombstones (which is where the author gets stamped).
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || {
            // Serialize the read→check→remove→commit critical section over the
            // shared index+HEAD, exactly as `put`; the lock releases when `_lock`
            // drops (all paths).
            let _lock = lock_repo(&store.root)?;
            let current = store.read(&key)?;
            match (current, &expected) {
                // Absent: nothing to remove — idempotent `Deleted`.
                (None, _) => Ok(DeleteResult::Deleted),
                // Unconditional, or the expected revision matches: remove + commit.
                (Some(cur), exp) if exp.is_none() || exp.as_ref() == Some(&cur.revision) => {
                    store.remove_and_commit(&key, &format!("delete {key}"))?;
                    Ok(DeleteResult::Deleted)
                }
                // Present but the expected revision differs: surface a Conflict.
                (Some(cur), _) => Ok(DeleteResult::Conflict(Box::new(Conflict {
                    key: key.clone(),
                    expected,
                    current: cur,
                }))),
            }
        })
        .await
    }

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        // Replication read: whatever the worktree holds, tombstones included.
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || store.read(&key)).await
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Replication listing: every `<id>.json` under the prefix, tombstones
        // included, without reading any file.
        let store = self.handle();
        let prefix = prefix.clone();
        run_blocking(move || {
            let mut out = Vec::new();
            collect_keys(&store.root, &prefix, &mut out)?;
            Ok(out)
        })
        .await
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || {
            // The only physical removal in the system, in the same repo-wide
            // critical section as `put`: read, decide (`plan_purge`), then
            // remove the file and commit the removal.
            let _lock = lock_repo(&store.root)?;
            let current = store.read(&key)?;
            match plan_purge(current.as_ref(), &expected) {
                PurgePlan::Remove => {
                    store.remove_and_commit(&key, &format!("purge {key}"))?;
                    Ok(DeleteResult::Deleted)
                }
                PurgePlan::Noop => Ok(DeleteResult::Deleted),
                PurgePlan::Conflict(conflict) => Ok(DeleteResult::Conflict(conflict)),
            }
        })
        .await
    }
}
```

Check that no struct literal still builds a `GitStore` without `cap`:

Run: `rg -n 'GitStore \{' crates/gonzalo-store-git/src/lib.rs`
Expected: exactly one hit, `pub struct GitStore {`. The constructors use `Self { ... }`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-store-git --test tombstones`
Expected: PASS, `test result: ok. 5 passed; 0 failed`.

Run: `cargo test -p gonzalo-store-git --test conformance --test put_and_push --test pull`
Expected: PASS for all three binaries.

- [ ] **Step 5: Format, lint the crate, commit**

```bash
cargo fmt --all
cargo clippy -p gonzalo-store-git --all-targets -- -D warnings
git add crates/gonzalo-store-git/src/lib.rs crates/gonzalo-store-git/tests/tombstones.rs
git commit -m "feat(store-git): ancestor cap; put via plan_put; purge commits removal (#203)" -m "Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 5: git `delete_as` commits a tombstone; consumer reads hide it

**Files:**
- Modify: `crates/gonzalo-store-git/src/lib.rs` (imports, `impl GitStore` gains `is_listed`, `Store` methods `get` / `list` / `delete_as`)
- Modify: `crates/gonzalo-store-git/tests/tombstones.rs`

**Interfaces:**
- Consumes: Task 4's `handle`, `put_locked`, `write_and_commit`, `lock_repo` and `GitStore.cap`, plus slice 1's `gonzalo_core::{plan_delete, DeletePlan, now_ms, tombstone_hash, Identity}`, `Record::is_tombstone`, and the trait-provided `Store::delete`.
- Produces: `fn GitStore::is_listed(&self, key: &RecordKey) -> Result<bool>`, and final git `Store` semantics per the overview contract.

- [ ] **Step 1: Write the failing tests**

In `crates/gonzalo-store-git/tests/tombstones.rs`, replace the `use gonzalo_core::{ ... };` block with:

```rust
use gonzalo_core::{
    Body, CoreError, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey,
    RecordKind, Revision, Store, tombstone_hash,
};
```

Append these tests to the end of the file:

```rust
#[tokio::test]
async fn delete_commits_a_tombstone_file_at_the_record_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "doomed");
    let rev0 = committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );

    assert_eq!(
        store
            .delete_as(&key, Some(rev0.clone()), Some(Identity::new("deleter")))
            .await
            .unwrap(),
        DeleteResult::Deleted
    );

    let bytes = head_tree_bytes(dir.path(), "ns/col/doomed.json")
        .expect("the tombstone is committed at the record path");
    let on_disk: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(on_disk["kind"], "Tombstone");
    let tomb: Record = serde_json::from_slice(&bytes).unwrap();
    assert!(tomb.is_tombstone());
    assert_eq!(
        tomb.revision,
        Revision {
            counter: rev0.counter + 1,
            hash: tombstone_hash(),
        }
    );
    assert!(tomb.deleted_at.is_some());
    assert_eq!(tomb.ancestors, vec![rev0]);
    // `delete_as` stamps the deleting principal onto the committed tombstone.
    assert_eq!(tomb.meta.author, Identity::new("deleter"));

    let (_, message) = head_commit_of(dir.path());
    assert!(message.starts_with("delete "), "got commit message {message:?}");
    // The worktree matches the commit.
    assert_eq!(
        std::fs::read(dir.path().join("ns/col/doomed.json")).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn noop_deletes_make_no_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "once");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    let (after_tombstone, _) = head_commit_of(dir.path());

    // Deleting a tombstone writes nothing.
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    // Deleting a key that never existed writes nothing.
    assert_eq!(
        store
            .delete(&RecordKey::new("ns", "col", "never"), None)
            .await
            .unwrap(),
        DeleteResult::Deleted
    );
    assert_eq!(head_commit_of(dir.path()).0, after_tombstone);
}

#[tokio::test]
async fn consumer_reads_hide_a_tombstone_and_raw_reads_show_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let dead = RecordKey::new("ns", "col", "dead");
    let live = RecordKey::new("ns", "col", "live");
    committed(
        store
            .put(rec(&dead, b"d", Revision::initial(b"d")), None)
            .await
            .unwrap(),
    );
    committed(
        store
            .put(rec(&live, b"l", Revision::initial(b"l")), None)
            .await
            .unwrap(),
    );
    assert_eq!(
        store.delete(&dead, None).await.unwrap(),
        DeleteResult::Deleted
    );

    assert_eq!(store.get(&dead).await.unwrap(), None);
    assert!(store.get_raw(&dead).await.unwrap().unwrap().is_tombstone());

    let all = KeyPrefix::default();
    assert_eq!(store.list(&all).await.unwrap(), vec![live.clone()]);
    let raw = store.list_raw(&all).await.unwrap();
    assert_eq!(raw.len(), 2);
    assert!(raw.contains(&dead) && raw.contains(&live));
}

#[tokio::test]
async fn purge_removes_a_committed_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "collected");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    let tomb = store.get_raw(&key).await.unwrap().unwrap();

    assert_eq!(
        store.purge(&key, tomb.revision).await.unwrap(),
        DeleteResult::Deleted
    );
    assert!(head_tree_bytes(dir.path(), "ns/col/collected.json").is_none());
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
}

/// A `*.json` in the worktree that doesn't parse as a `Record` stays listed
/// by consumer `list`, exactly as before tombstones, and `get` surfaces the
/// parse error.
#[tokio::test]
async fn list_keeps_an_unparseable_record_file_listed() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let good = RecordKey::new("ns", "col", "good");
    committed(
        store
            .put(rec(&good, b"g", Revision::initial(b"g")), None)
            .await
            .unwrap(),
    );
    std::fs::write(dir.path().join("ns/col/garbage.json"), b"not json").unwrap();
    let garbage = RecordKey::new("ns", "col", "garbage");

    let listed = store.list(&KeyPrefix::default()).await.unwrap();
    assert!(listed.contains(&good));
    assert!(listed.contains(&garbage));
    assert!(matches!(
        store.get(&garbage).await,
        Err(CoreError::Serde(_))
    ));
}

/// Consumer `put` treats a tombstoned key as absent: a conditional write
/// naming any revision (even the tombstone's own) is `NotFound` and commits
/// nothing; an unconditional write is a re-stamped recreation.
#[tokio::test]
async fn consumer_put_over_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "reborn");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    let tomb = store.get_raw(&key).await.unwrap().unwrap();
    let (after_tombstone, _) = head_commit_of(dir.path());

    assert!(matches!(
        store
            .put(rec(&key, b"y", Revision::initial(b"y")), Some(tomb.revision.clone()))
            .await,
        Err(CoreError::NotFound(_))
    ));
    assert_eq!(head_commit_of(dir.path()).0, after_tombstone);

    let recreated = committed(
        store
            .put(rec(&key, b"y", Revision::initial(b"y")), None)
            .await
            .unwrap(),
    );
    assert_eq!(recreated.counter, tomb.revision.counter + 1);
    let stored = store.get(&key).await.unwrap().unwrap();
    assert_eq!(stored.parent, Some(tomb.revision));
}

/// Replication `put_raw` never re-stamps: a create over a tombstone conflicts
/// with the tombstone as `current` and commits nothing; a write conditional on
/// the tombstone's revision commits the caller's revision verbatim.
#[tokio::test]
async fn put_raw_over_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).unwrap();
    let key = RecordKey::new("ns", "col", "replicated");
    committed(
        store
            .put(rec(&key, b"x", Revision::initial(b"x")), None)
            .await
            .unwrap(),
    );
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    let tomb = store.get_raw(&key).await.unwrap().unwrap();
    let (after_tombstone, _) = head_commit_of(dir.path());

    match store
        .put_raw(rec(&key, b"y", Revision::initial(b"y")), None)
        .await
        .unwrap()
    {
        PutResult::Conflict(c) => assert!(c.current.is_tombstone()),
        PutResult::Committed(rev) => panic!("create over a tombstone must conflict, got {rev:?}"),
    }
    assert_eq!(head_commit_of(dir.path()).0, after_tombstone);

    let incoming = Revision {
        counter: 7,
        hash: gonzalo_core::ContentHash::of(b"peer"),
    };
    assert_eq!(
        committed(
            store
                .put_raw(rec(&key, b"peer", incoming.clone()), Some(tomb.revision.clone()))
                .await
                .unwrap()
        ),
        incoming
    );
    let committed_rec: Record =
        serde_json::from_slice(&head_tree_bytes(dir.path(), "ns/col/replicated.json").unwrap())
            .unwrap();
    assert_eq!(committed_rec.revision, incoming);
    assert!(committed_rec.ancestors.contains(&tomb.revision));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-store-git --test tombstones`
Expected: FAIL.
- `delete_commits_a_tombstone_file_at_the_record_path` panics with `the tombstone is committed at the record path`.
- `consumer_reads_hide_a_tombstone_and_raw_reads_show_it`, `purge_removes_a_committed_tombstone`, `consumer_put_over_a_tombstone` and `put_raw_over_a_tombstone` panic on `unwrap()` of `None`, because the interim `delete_as` still removes the file.
- `noop_deletes_make_no_commit` and `list_keeps_an_unparseable_record_file_listed` already pass. They guard the no-empty-commit and unparseable-file behaviour through Step 3.

- [ ] **Step 3: Implement tombstone deletes and consumer filtering**

In the `use gonzalo_core::{ ... };` list, add `DeletePlan, now_ms, plan_delete` and **remove** `store::Conflict`.

Inside `impl GitStore`, add this method directly after `remove_and_commit`:

```rust
    /// Whether consumer `list` reports `key`. A tombstone is hidden. A file
    /// that vanished since the directory walk (a concurrent `purge`) is
    /// dropped. A file that doesn't parse as a `Record` stays listed, exactly
    /// as before tombstones, so `get` keeps surfacing the `Serde` error.
    fn is_listed(&self, key: &RecordKey) -> Result<bool> {
        match std::fs::read(self.path_for(key)) {
            Ok(bytes) => Ok(serde_json::from_slice::<Record>(&bytes)
                .map(|rec| !rec.is_tombstone())
                .unwrap_or(true)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(be(e)),
        }
    }
```

In `impl gonzalo_core::Store for GitStore`, replace `get`, `list` and `delete_as` with the following. There must be no `delete` method in the impl, because the trait provides it.

```rust
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        // Consumer read: a tombstoned key looks absent (spec §3.2).
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || Ok(store.read(&key)?.filter(|rec| !rec.is_tombstone()))).await
    }
```

```rust
    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Consumer listing excludes tombstoned keys, which means reading each
        // record file under the prefix (spec §8.4: a local read per key).
        let store = self.handle();
        let prefix = prefix.clone();
        run_blocking(move || {
            let mut keys = Vec::new();
            collect_keys(&store.root, &prefix, &mut keys)?;
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                if store.is_listed(&key)? {
                    out.push(key);
                }
            }
            Ok(out)
        })
        .await
    }
```

```rust
    // No `delete` here: the trait provides it as `delete_as(key, expected, None)`.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        let store = self.handle();
        let key = key.clone();
        run_blocking(move || {
            // Serialize the read→plan→write→commit critical section over the
            // shared index+HEAD, exactly as `put`; the lock releases when `_lock`
            // drops (all paths).
            let _lock = lock_repo(&store.root)?;
            let current = store.read(&key)?;
            // A delete commits a tombstone at the record's normal path, so git
            // history shows it as an ordinary modification (spec §3.3, §5.5).
            // A no-op (absent key, or already a tombstone) makes no commit.
            // `Some(author)` replaces `meta.author` on the tombstone.
            match plan_delete(current.as_ref(), expected, now_ms(), store.cap, author.as_ref()) {
                DeletePlan::Write(tombstone) => {
                    store.write_and_commit(&tombstone, &format!("delete {key}"))?;
                    Ok(DeleteResult::Deleted)
                }
                DeletePlan::Noop => Ok(DeleteResult::Deleted),
                DeletePlan::Conflict(conflict) => Ok(DeleteResult::Conflict(conflict)),
            }
        })
        .await
    }
```

(`put`, `put_raw`, `get_raw`, `list_raw` and `purge` stay as Task 4 left them.)

Check that no `delete` override remains:

Run: `rg -n 'async fn delete\(' crates/gonzalo-store-git/src/lib.rs`
Expected: no output.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-store-git --test tombstones`
Expected: PASS, `test result: ok. 12 passed; 0 failed`.

Run: `cargo test -p gonzalo-store-git`
Expected: PASS for every binary, including `conformance`, `pull`, `put_and_push` and the `diff.rs` unit tests.

- [ ] **Step 5: Format, lint the crate, commit**

```bash
cargo fmt --all
cargo clippy -p gonzalo-store-git --all-targets -- -D warnings
git add crates/gonzalo-store-git/src/lib.rs crates/gonzalo-store-git/tests/tombstones.rs
git commit -m "feat(store-git): delete commits tombstones; consumer reads hide them (#203)" -m "Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 6: Wire tombstone conformance into the git store

**Files:**
- Modify: `crates/gonzalo-store-git/tests/conformance.rs` (whole file, `:1-12`)

**Interfaces:**
- Consumes: `gonzalo_core::conformance::run_tombstone_conformance`, `gonzalo_core::DEFAULT_ANCESTOR_CAP`, and `GitStore::with_ancestor_cap` (Task 4).
- Produces: the test functions `git_store_passes_tombstone_conformance_default_cap` and `git_store_passes_tombstone_conformance_small_cap`.

- [ ] **Step 1: Write the conformance tests**

Replace `crates/gonzalo-store-git/tests/conformance.rs` in full:

```rust
use gonzalo_core::DEFAULT_ANCESTOR_CAP;
use gonzalo_core::conformance::{run_store_conformance, run_tombstone_conformance};
use gonzalo_store_git::GitStore;

/// A small cap makes `ancestors_capped_and_ordered` exercise truncation after a
/// handful of updates instead of 32+.
const SMALL_CAP: usize = 3;

/// A freshly initialized git store in a leaked TempDir (it must outlive the
/// factory invocation).
fn fresh_store() -> GitStore {
    let path = tempfile::tempdir().expect("tempdir").keep();
    GitStore::open(path).expect("open git store")
}

#[tokio::test]
async fn git_store_passes_conformance() {
    run_store_conformance(|| async { fresh_store() }).await;
}

#[tokio::test]
async fn git_store_passes_tombstone_conformance_default_cap() {
    run_tombstone_conformance(|| async { fresh_store() }, DEFAULT_ANCESTOR_CAP).await;
}

#[tokio::test]
async fn git_store_passes_tombstone_conformance_small_cap() {
    run_tombstone_conformance(
        || async {
            fresh_store()
                .with_ancestor_cap(SMALL_CAP)
                .expect("cap 3 is valid")
        },
        SMALL_CAP,
    )
    .await;
}
```

- [ ] **Step 2: Run the conformance tests**

Run: `cargo test -p gonzalo-store-git --test conformance`
Expected: PASS, `test result: ok. 3 passed; 0 failed`. Tasks 4–5 implemented the behaviour. If a §6.1 case fails, fix `src/lib.rs`, not the suite.

To confirm the wiring really runs the suite, temporarily pass `4` instead of `SMALL_CAP` as the second argument of the small-cap test (leave the builder at 3), then run
`cargo test -p gonzalo-store-git --test conformance git_store_passes_tombstone_conformance_small_cap`.
Expected: FAIL in `ancestors_capped_and_ordered`. Revert and re-run. Expected: PASS.

- [ ] **Step 3: Format, commit**

```bash
cargo fmt --all
git add crates/gonzalo-store-git/tests/conformance.rs
git commit -m "test(store-git): run tombstone conformance at default and small caps (#203)" -m "Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 7: Legacy-test re-run, full gate, push, PR

**Files:**
- None modified, unless the gate finds something. Fixes then go in the file the failure names, with their own commit.

**Interfaces:**
- Consumes: everything above.
- Produces: an open PR for slice 2 with green CI.

- [ ] **Step 1: Re-run the audited legacy tests by name**

Run each command on its own line:

```bash
cargo test -p gonzalo-store-fs --test list_ignores_stray_files --test concurrent_put_no_lost_update --test blob_store --test slice_gc
cargo test -p gonzalo-store-git --test pull --test put_and_push
cargo test -p gonzalo-integration-tests --all-features --test server_store_conformance
```

Expected: every binary PASSES. The last command runs `run_store_conformance` against the daemon backed by `FsStore`, which now writes tombstones. It is the first place slice 1's rewritten delete cases run over the wire. If it fails in a case that reads a tombstone through `ServerStore`'s interim `get_raw` (which delegates to consumer `get` until slice 4), stop and report it as a contract issue for slices 1 and 4. Do not add a fallback in `ServerStore`.

- [ ] **Step 2: Run the full verification gate**

Run these as four separate commands, one per line, never joined with `&&`:

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

```bash
cargo build --workspace --all-targets --all-features
```

```bash
cargo test --workspace --all-features
```

Expected: `fmt` prints nothing and exits 0. `clippy` ends with `Finished` and no `warning:` lines. `build` ends with `Finished`. `test` shows no `FAILED` and every `test result:` line reads `ok`. Check each command's exit status yourself before moving to the next.

- [ ] **Step 3: Push the branch**

```bash
git push -u origin feat/203-tombstones-fs-git
```

Expected: `branch 'feat/203-tombstones-fs-git' set up to track 'origin/feat/203-tombstones-fs-git'`.

- [ ] **Step 4: Open the PR**

```bash
gh pr create --base main --head feat/203-tombstones-fs-git --title "feat(store): fs and git stores write tombstones (#203 slice 2)" --body "$(cat <<'EOF'
## Summary

Slice 2 of replicated deletion (spec `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` §3.2, §3.3, §3.9).

- `FsStore` / `GitStore`: `put`, `put_raw`, `delete_as` and `purge` now carry out the shared core planners (`plan_put` / `plan_put_raw` / `plan_delete` / `plan_purge`) inside their existing OCC critical sections (fs per-key flock; git repo lock). `put` and `put_raw` share one locked write path. Neither store implements `delete`; the trait provides it via `delete_as(.., None)`.
- `put_raw` (replication write) never re-stamps: a create over a tombstone conflicts instead of recreating.
- `delete_as` stamps `Some(author)` onto the tombstone.
- `delete` writes a `Tombstone` record at the key's normal path. On fs it goes through the durable temp+fsync+rename publish. On git it is committed as an ordinary modification. No-op deletes write nothing and make no commit.
- `purge` is the only physical removal. fs unlinks the file and fsyncs the directory; git takes over `commit_removal`.
- Consumer `get`/`list` hide tombstones. `list` now reads each record to filter (spec §8.4); unparseable `*.json` files stay listed as before. `get_raw`/`list_raw` keep the unfiltered behaviour.
- `with_ancestor_cap(n) -> Result<Self>` on both stores (default `DEFAULT_ANCESTOR_CAP`, 0 rejected).
- Both stores run `run_tombstone_conformance` at the default cap and at cap 3, plus store-specific tests for on-disk / git-tree state.

Git pull is unchanged (slice 5). Not releasable on its own: no release is tagged until slices 1–5 merge.

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [x] `cargo build --workspace --all-targets --all-features`
- [x] `cargo test --workspace --all-features`

Part of #203

https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

Expected: `gh` prints the new PR URL.

- [ ] **Step 5: Wait for CI, then merge**

```bash
gh pr checks --watch
```

Expected: every check passes. Then merge the PR (the repository's usual squash merge, e.g. `gh pr merge --squash --delete-branch`). If a check fails, fix it on the branch, re-run the full gate from Step 2, and push again.

---

## Self-review

**Spec coverage**

| Spec item | Task |
|---|---|
| §3.2 `get` hides tombstones, `list` excludes them | 2 (fs), 5 (git) |
| §3.2 `put` over a tombstone (recreation, replication overwrite, NotFound) | 1 (fs), 4 (git), via `plan_put`, proven by conformance in 3 and 6 |
| §3.2 `delete` table | 2, 5 via `plan_delete`; conformance in 3 and 6 |
| §3.2 ancestor maintenance inside the critical section | 1, 4 (cap test), conformance `ancestors_capped_and_ordered` at cap 3 and 32 |
| §3.2 `get_raw` / `list_raw` / `purge` required | 1, 4 |
| Reconciled contract: `put_raw` via `plan_put_raw`, never re-stamps, create over tombstone conflicts | 1, 4 (cap tests), 2, 5 (`put_raw_over_a_tombstone`), conformance in 3 and 6 |
| Reconciled contract: consumer `put(Some(_))` over a tombstone is `NotFound` | 2, 5 (`consumer_put_over_a_tombstone`) |
| Reconciled contract: stores implement `delete_as` (author passed to `plan_delete`), never `delete` | 1, 4 (interim), 2, 5 (author assertions + `rg` checks), conformance `delete_as_stamps_author` |
| §3.3 fs: atomic tombstone write under flock; `purge` = today's conditional unlink | 1, 2 |
| §3.3 git: commit the tombstone file; `purge` = `commit_removal` | 4, 5 |
| §3.9 `with_ancestor_cap`, 0 rejected, default 32 | 1, 4 |
| §5.5 same path, filtered reads | 2, 5 (path/tree assertions) |
| §6.1 conformance on fs and git | 3, 6 |
| §8.4 `list` must read to filter | 2, 5 (documented in code comments) |

**Deliberately out of scope here:** git pull tombstone handling (§3.5, §6.3) is slice 5. s3 is slice 3. `ServerStore` raw routes are slice 4.

**Placeholder scan:** every code step has complete code, and no step says "similar to Task N". **Name consistency:** `lock_record` / `read_current` / `write_durable` / `remove_durable` / `put_locked` / `delete_locked` / `purge_locked` / `listed_live` (fs) and `handle` / `write_and_commit` / `remove_and_commit` / `is_listed` / `rel_path` (git) are used with the same signatures in every task. Contract names (`plan_put`, `PutPlan::{Write, Conflict, NotFound}`, `DeletePlan::{Write, Noop, Conflict}`, `PurgePlan::{Remove, Noop, Conflict}`, `now_ms`, `tombstone_hash`, `validate_ancestor_cap`, `DEFAULT_ANCESTOR_CAP`, `run_tombstone_conformance(factory, cap)`) match the overview verbatim.
