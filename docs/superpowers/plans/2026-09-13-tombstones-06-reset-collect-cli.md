# Tombstones Slice 6 — Reset, Collect and CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `gonzalo_core::reset` and `gonzalo_core::collect` (spec §3.7, §3.8) and the CLI commands `gonzalo delete`, `gonzalo reset` and `gonzalo collect` (spec §3.10), plus `--ancestor-cap` (spec §3.9), closing gonzalo#203.

**Architecture:** Two new free-function modules in `gonzalo-core`, `reset.rs` and `collect.rs`. They follow the file-per-concern style of `sync.rs` and `gc.rs`: a `#[must_use]` report struct, one `pub async fn` over `&dyn Store`, and unit tests in the same file. Both use only `Store` trait methods from slice 1, so they work on every substrate and don't depend on slice 5 (sync). The CLI gains thin wrappers in `crates/gonzalo-cli/src/lib.rs` (open an `FsStore` with a validated ancestor cap, call core, return the report) and three new clap subcommands in `main.rs`. `main` changes to return `ExitCode` so a conflict exits with a dedicated code, `3`, distinct from errors (`1`) and usage errors (`2`).

**Tech Stack:** Rust 2024 (MSRV 1.95), tokio, async-trait, clap 4 derive, anyhow, serde_json. No new dependencies. The CLI duration parser is hand-written because the workspace has no `humantime`.

**Spec:** `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` (§3.7 reset, §3.8 collect, §3.9 cap, §3.10 CLI, §6.4, §6.6). Shared contract: `docs/superpowers/plans/2026-09-13-tombstones-00-overview.md`.

## Global Constraints

- Verification gate before every push and PR, matching CI exactly:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo build --workspace --all-targets --all-features`
  - `cargo test --workspace --all-features`
- Run the gate as bare commands, one per line, not joined with `&&`.
- Always open a PR and merge it after CI is green. Never push to `main`.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`. PR bodies end with `https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.
- **This slice's PR body says `Closes #203`** (the only slice that does).
- Default ancestor cap: `32` (`gonzalo_core::DEFAULT_ANCESTOR_CAP`). A cap of `0` is rejected at construction.
- `deleted_at` unit: milliseconds since the Unix epoch, `i64`.
- Never fall back from a raw read to a consumer read. `collect` uses `list_raw`/`get_raw` only; `reset` uses `list`/`get`/`delete` only.
- `reset` refuses a prefix with no namespace. `collect` has **no default horizon**.
- Workspace lints: `unsafe_code = "forbid"`, clippy `all = warn`, promoted to errors by `-D warnings`. (`clippy::items_after_test_module` is in that set, so new non-test items must go **above** an existing `#[cfg(test)] mod tests`.)
- Use shared-contract names exactly (reconciled contract):
  - `Record::is_tombstone`
  - `tombstone_of(current: &Record, now_ms: i64, cap: usize, author: Option<&Identity>) -> Record`
  - `plan_delete(current, expected, now_ms, cap, author: Option<&Identity>)`
  - `gonzalo_core::{now_ms, DEFAULT_ANCESTOR_CAP}` (re-exported at the crate root)
  - `crate::memstore::MemStore::{new, with_clock, raw_snapshot}`
  - Required `Store` methods: `get`, `put`, `list`, `delete_as(&self, key, expected, author: Option<Identity>)`, `get_raw`, `list_raw`, `put_raw(&self, record, expected) -> Result<PutResult>` (replication write, never re-stamps), `purge`. `delete(key, expected)` is a **provided** method calling `delete_as(key, expected, None)`, so test doubles implement `delete_as`, not `delete`.
  - Consumer `put` over a tombstone with `expected = Some(_)` is `NotFound`. Only `put(_, None)` recreates.
  - `FsStore::with_ancestor_cap(self, usize) -> gonzalo_core::Result<Self>`, whose error text is exactly `ancestor cap must be at least 1`.
- CLI exit codes: `0` success, `1` error (anyhow), `2` usage error (clap), `3` conflict (`delete` conflict, or `reset` with any conflicts). Each new command's `--help` states its codes.
- CLI deletes are attributed to `Identity::new("gonzalo-cli")`, the identity `migrate` already stamps (`crates/gonzalo-cli/src/lib.rs:122`) and the `ticket sync --author` default.

## Prerequisites (assumed merged on `main`)

- Slice 1: core model, planners, trait methods, `MemStore`.
- Slice 2: `FsStore` writes tombstones, filters consumer reads, implements `purge`, and has `with_ancestor_cap`.
- Slice 5 (sync) is **not** required.

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `crates/gonzalo-core/src/reset.rs` | Create | `ResetReport`, `reset`, and its tests (including the `EditBeforeDelete` race double) |
| `crates/gonzalo-core/src/collect.rs` | Create | `CollectReport`, `collect`, and its tests (including the `RecreateBeforePurge` race double) |
| `crates/gonzalo-core/src/lib.rs` | Modify (after `pub use sync::…`, currently lines 36–37) | `pub mod reset; pub mod collect;` plus re-exports |
| `crates/gonzalo-cli/src/lib.rs` | Modify (new section above `// ─── sync_stores`, currently line 986; `sync_stores` at 996–1007; new test module at end of file) | `open_store`, `DeleteOutcome`, `delete`, `reset`, `collect`, `Horizon`, `parse_horizon`, `parse_duration`, `parse_revision`, `sync_stores_with_cap` |
| `crates/gonzalo-cli/src/main.rs` | Modify (imports 5–9; `Commands` 32–143; `main` 212–214, 306, 361–367, 428) | `Delete`, `Reset` and `Collect` subcommands; `--ancestor-cap` on those and on `Sync`; `ExitCode` |
| `crates/gonzalo-cli/tests/cli.rs` | Modify (append) | §6.6 integration tests |

The guide (`docs/guide/src/`) has no CLI command pages today (`SUMMARY.md` lists only principles, MCP, changelog and ADRs), and nothing in it is generated from or checked against the clap definitions. Guide prose for these commands is left to slice 7.

---

### Task 1: Core `reset`

**Files:**
- Create: `crates/gonzalo-core/src/reset.rs`
- Modify: `crates/gonzalo-core/src/lib.rs` (after the `pub use sync::…` line)

**Interfaces:**
- Consumes (slice 1): `Store::{list, get, delete}` (`delete` is the provided method over the required `delete_as`), `Store::put_raw` (the race double must implement it), `DeleteResult::{Deleted, Conflict}`, `KeyPrefix::matches`, `crate::memstore::MemStore::{new, with_clock, raw_snapshot}`, `Record::{ancestors, deleted_at, is_tombstone}`.
- Produces:
  ```rust
  #[derive(Clone, Debug, Default, PartialEq, Eq)]
  pub struct ResetReport { pub deleted: Vec<RecordKey>, pub conflicts: Vec<RecordKey> }
  pub async fn reset(store: &dyn Store, prefix: &KeyPrefix) -> Result<ResetReport>;
  // re-exported: gonzalo_core::{ResetReport, reset}
  ```
  Error for a missing namespace: `CoreError::Backend("reset requires a namespace".into())`, with that exact text.

- [ ] **Step 1: Create the branch and confirm the prerequisites are merged**

```bash
git switch main
git pull --ff-only
git switch -c issue-203-reset-collect-cli
rg -n "pub fn with_clock|pub fn raw_snapshot" crates/gonzalo-core/src/memstore.rs
rg -n "pub fn tombstone_of|pub const DEFAULT_ANCESTOR_CAP|pub fn now_ms" crates/gonzalo-core/src/tombstone.rs
rg -n "fn with_ancestor_cap|async fn purge" crates/gonzalo-store-fs/src/lib.rs
```

Expected: every `rg` prints at least one match. If any prints nothing, stop: slice 1 or 2 has not merged.

- [ ] **Step 2: Write the failing tests (module file with tests only, and registration)**

Create `crates/gonzalo-core/src/reset.rs`:

```rust
//! Namespace / collection reset (spec §3.7, gonzalo#203).
//!
//! Reset tombstones every live record under a prefix by issuing ordinary
//! OCC-guarded `delete` calls, so it replicates like any other delete and
//! needs only `write` on the namespace. It is **not atomic**: no substrate
//! offers multi-key transactions. It is **idempotent** instead. A second run
//! tombstones whatever the first run missed (a key that lost a race) and
//! skips everything already gone.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memstore::MemStore;
    use crate::{
        Body, CoreError, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey,
        RecordKind, Result, Revision, Store,
    };
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    const CLOCK: i64 = 1_000;

    fn rec(ns: &str, col: &str, id: &str, payload: &str) -> Record {
        let body = Body::Inline(payload.as_bytes().to_vec());
        Record {
            key: RecordKey::new(ns, col, id),
            kind: RecordKind::Topic,
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
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

    async fn seed(store: &dyn Store, record: Record) {
        assert!(matches!(
            store.put(record, None).await.unwrap(),
            PutResult::Committed(_)
        ));
    }

    fn ns(namespace: &str) -> KeyPrefix {
        KeyPrefix {
            namespace: Some(namespace.into()),
            collection: None,
        }
    }

    #[tokio::test]
    async fn refuses_a_prefix_without_a_namespace() {
        let store = MemStore::new().with_clock(CLOCK);
        seed(&store, rec("ns", "col", "a", "x")).await;

        let err = reset(
            &store,
            &KeyPrefix {
                namespace: None,
                collection: Some("col".into()),
            },
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&err, CoreError::Backend(m) if m == "reset requires a namespace"),
            "got {err:?}"
        );
        // Nothing was touched.
        assert!(
            store
                .raw_snapshot()
                .values()
                .all(|r| !r.is_tombstone())
        );
    }

    #[tokio::test]
    async fn tombstones_every_live_record_in_the_namespace_only() {
        let store = MemStore::new().with_clock(CLOCK);
        seed(&store, rec("ns", "a", "1", "x")).await;
        seed(&store, rec("ns", "b", "2", "y")).await;
        seed(&store, rec("other", "a", "3", "z")).await;

        let report = reset(&store, &ns("ns")).await.unwrap();

        assert_eq!(
            report.deleted,
            vec![RecordKey::new("ns", "a", "1"), RecordKey::new("ns", "b", "2")]
        );
        assert!(report.conflicts.is_empty());
        for key in &report.deleted {
            assert_eq!(store.get(key).await.unwrap(), None, "{key} hidden");
            let raw = store.get_raw(key).await.unwrap().expect("tombstone kept");
            assert!(raw.is_tombstone());
            assert_eq!(raw.deleted_at, Some(CLOCK));
        }
        let other = RecordKey::new("other", "a", "3");
        assert!(store.get(&other).await.unwrap().is_some(), "sibling namespace untouched");
    }

    #[tokio::test]
    async fn collection_scope_leaves_sibling_collections_alone() {
        let store = MemStore::new().with_clock(CLOCK);
        seed(&store, rec("ns", "col", "1", "x")).await;
        seed(&store, rec("ns", "sibling", "2", "y")).await;

        let report = reset(
            &store,
            &KeyPrefix {
                namespace: Some("ns".into()),
                collection: Some("col".into()),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.deleted, vec![RecordKey::new("ns", "col", "1")]);
        assert!(report.conflicts.is_empty());
        assert!(
            store
                .get(&RecordKey::new("ns", "sibling", "2"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn already_deleted_keys_are_skipped_and_their_chain_does_not_advance() {
        let store = MemStore::new().with_clock(CLOCK);
        let gone = RecordKey::new("ns", "col", "gone");
        seed(&store, rec("ns", "col", "gone", "x")).await;
        seed(&store, rec("ns", "col", "live", "y")).await;
        assert_eq!(
            store.delete(&gone, None).await.unwrap(),
            DeleteResult::Deleted
        );
        let before = store.get_raw(&gone).await.unwrap().unwrap().revision;

        let report = reset(&store, &ns("ns")).await.unwrap();

        assert_eq!(report.deleted, vec![RecordKey::new("ns", "col", "live")]);
        assert_eq!(
            store.get_raw(&gone).await.unwrap().unwrap().revision,
            before
        );
    }

    /// Wraps a `MemStore` and, on the first `delete` of `target`, commits a
    /// concurrent edit to that key before delegating. This is the "someone
    /// edited the key between reset's `get` and its `delete`" race.
    struct EditBeforeDelete {
        inner: MemStore,
        target: RecordKey,
        fired: AtomicBool,
    }

    #[async_trait]
    impl Store for EditBeforeDelete {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get(key).await
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            self.inner.put(record, expected).await
        }
        async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.inner.list(prefix).await
        }
        // `delete` is a provided method over `delete_as`, so the race hooks here.
        async fn delete_as(
            &self,
            key: &RecordKey,
            expected: Option<Revision>,
            author: Option<Identity>,
        ) -> Result<DeleteResult> {
            if key == &self.target && !self.fired.swap(true, Ordering::SeqCst) {
                let current = self.inner.get(key).await?.expect("target is live");
                let mut edited = current.clone();
                edited.body = Body::Inline(b"edited concurrently".to_vec());
                edited.revision = current.revision.next(edited.body.bytes());
                edited.parent = Some(current.revision.clone());
                assert!(matches!(
                    self.inner.put(edited, Some(current.revision)).await?,
                    PutResult::Committed(_)
                ));
            }
            self.inner.delete_as(key, expected, author).await
        }
        async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            self.inner.put_raw(record, expected).await
        }
        async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get_raw(key).await
        }
        async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.inner.list_raw(prefix).await
        }
        async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
            self.inner.purge(key, expected).await
        }
    }

    #[tokio::test]
    async fn concurrent_edit_is_a_conflict_and_rerunning_is_idempotent() {
        let raced = RecordKey::new("ns", "col", "b");
        let store = EditBeforeDelete {
            inner: MemStore::new().with_clock(CLOCK),
            target: raced.clone(),
            fired: AtomicBool::new(false),
        };
        for id in ["a", "b", "c"] {
            seed(&store, rec("ns", "col", id, id)).await;
        }

        // Run 1: the edit wins the race on `b`; reset reports it, doesn't retry.
        let first = reset(&store, &ns("ns")).await.unwrap();
        assert_eq!(
            first.deleted,
            vec![RecordKey::new("ns", "col", "a"), RecordKey::new("ns", "col", "c")]
        );
        assert_eq!(first.conflicts, vec![raced.clone()]);
        let survivor = store.get(&raced).await.unwrap().expect("edit survives");
        assert_eq!(survivor.body.bytes(), b"edited concurrently");

        // Run 2: tombstones exactly what run 1 missed, and conflicts on nothing.
        let second = reset(&store, &ns("ns")).await.unwrap();
        assert_eq!(second.deleted, vec![raced.clone()]);
        assert!(second.conflicts.is_empty());
        assert_eq!(store.get(&raced).await.unwrap(), None);

        // Run 3: nothing left to do.
        let third = reset(&store, &ns("ns")).await.unwrap();
        assert_eq!(third, ResetReport::default());
    }
}
```

In `crates/gonzalo-core/src/lib.rs`, directly after the line `pub use sync::{…};` (line 37 today; slice 5 may have changed its item list, so anchor on `pub use sync::`), add:

```rust

pub mod reset;
pub use reset::{ResetReport, reset};
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core --lib reset::`
Expected: compile FAIL with `error[E0425]: cannot find function \`reset\` in this scope` and `cannot find struct, variant or union type \`ResetReport\``, and `unresolved import \`reset::ResetReport\`` from `lib.rs`.

- [ ] **Step 4: Write the implementation**

In `crates/gonzalo-core/src/reset.rs`, insert between the module doc comment and `#[cfg(test)]`:

```rust

use crate::{CoreError, DeleteResult, KeyPrefix, RecordKey, Result, Store};

/// What a reset run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "a ResetReport may list conflicted keys that were not deleted"]
pub struct ResetReport {
    /// Keys tombstoned by this run, in `list` order.
    pub deleted: Vec<RecordKey>,
    /// Keys edited between this run's `get` and its `delete`. Left live, not
    /// retried. Run reset again to delete them.
    pub conflicts: Vec<RecordKey>,
}

/// Tombstone every live record under `prefix`. `prefix.namespace` is required.
/// Resetting every namespace in a store is not a reset, so a caller who wants
/// that must loop over namespaces explicitly.
///
/// For each live key (consumer `list`), reads its current revision and issues
/// `delete(key, Some(revision))`. A key that disappears between `list` and
/// `get` is skipped. A `Conflict` goes into [`ResetReport::conflicts`].
pub async fn reset(store: &dyn Store, prefix: &KeyPrefix) -> Result<ResetReport> {
    if prefix.namespace.is_none() {
        return Err(CoreError::Backend("reset requires a namespace".into()));
    }
    let mut report = ResetReport::default();
    for key in store.list(prefix).await? {
        // Belt-and-braces: reset is destructive, so never act on a key a
        // misbehaving store returned from outside the prefix.
        if !prefix.matches(&key) {
            continue;
        }
        let Some(current) = store.get(&key).await? else {
            continue;
        };
        match store.delete(&key, Some(current.revision)).await? {
            DeleteResult::Deleted => report.deleted.push(key),
            DeleteResult::Conflict(_) => report.conflicts.push(key),
        }
    }
    Ok(report)
}
```

Then trim the test module's `use crate::{…}` so it doesn't re-import items `super::*` already brings in (clippy would not fail on this, but rustc warns on unused imports if any become redundant). Replace it with:

```rust
    use crate::{Body, Identity, Meta, PutResult, Record, RecordKind, Revision};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core --lib reset::`
Expected: `test result: ok. 5 passed; 0 failed`.

Run: `cargo clippy -p gonzalo-core --all-targets --all-features -- -D warnings`
Expected: no warnings, `Finished`.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/gonzalo-core/src/reset.rs crates/gonzalo-core/src/lib.rs
git commit -m "feat(core): namespace/collection reset over tombstoning deletes (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 2: Core `collect`

**Files:**
- Create: `crates/gonzalo-core/src/collect.rs`
- Modify: `crates/gonzalo-core/src/lib.rs` (after the `pub use reset::…` line from Task 1)

**Interfaces:**
- Consumes (slice 1): `Store::{list_raw, get_raw, purge, put, put_raw, delete, get}` (the race double implements `delete_as` and `put_raw`), `Record::is_tombstone`, `crate::tombstone::{tombstone_of, DEFAULT_ANCESTOR_CAP}` with `tombstone_of(current, now_ms, cap, author: Option<&Identity>)`, `MemStore::{new, with_clock, raw_snapshot}`.
- Produces:
  ```rust
  #[derive(Clone, Debug, Default, PartialEq, Eq)]
  pub struct CollectReport { pub purged: Vec<RecordKey>, pub unstamped: usize, pub conflicts: Vec<RecordKey> }
  pub async fn collect(store: &dyn Store, prefix: &KeyPrefix, horizon: std::time::Duration, now_ms: i64) -> Result<CollectReport>;
  // re-exported: gonzalo_core::{CollectReport, collect}
  ```

- [ ] **Step 1: Write the failing tests (module file with tests only, and registration)**

Create `crates/gonzalo-core/src/collect.rs`:

```rust
//! Tombstone collection (spec §3.8, gonzalo#203).
//!
//! A tombstone is what stops a deleted record from resurrecting on the next
//! sync, so purging one is only safe once every peer has synced past the
//! delete. Only the operator knows how long that takes. Collection is
//! therefore explicit, never automatic, and takes the horizon as a required
//! argument with no default. Not to be confused with [`gc_blobs`](crate::gc_blobs),
//! which sweeps orphaned code-graph slices.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memstore::MemStore;
    use crate::tombstone::{DEFAULT_ANCESTOR_CAP, tombstone_of};
    use crate::{
        Body, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
        Result, Revision, Store,
    };
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    const DAY_MS: i64 = 86_400_000;
    const NOW: i64 = 100 * DAY_MS;
    const THIRTY_DAYS: Duration = Duration::from_secs(30 * 86_400);

    fn rec(ns: &str, col: &str, id: &str, payload: &str) -> Record {
        let body = Body::Inline(payload.as_bytes().to_vec());
        Record {
            key: RecordKey::new(ns, col, id),
            kind: RecordKind::Topic,
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
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

    async fn seed(store: &dyn Store, record: Record) {
        assert!(matches!(
            store.put(record, None).await.unwrap(),
            PutResult::Committed(_)
        ));
    }

    /// Store a tombstone for `ns/col/id` directly, with an arbitrary
    /// `deleted_at`, the way sync would replicate one into an empty store.
    async fn seed_tombstone(store: &dyn Store, ns: &str, col: &str, id: &str, deleted_at: Option<i64>) {
        let live = rec(ns, col, id, "was live");
        let mut tomb = tombstone_of(&live, 0, DEFAULT_ANCESTOR_CAP, None);
        tomb.deleted_at = deleted_at;
        // `put_raw` is the replication write: stores the tombstone unchanged.
        assert!(matches!(
            store.put_raw(tomb, None).await.unwrap(),
            PutResult::Committed(_)
        ));
    }

    #[tokio::test]
    async fn purges_only_stamped_tombstones_at_least_horizon_old() {
        let store = MemStore::new();
        seed_tombstone(&store, "ns", "col", "old", Some(NOW - 31 * DAY_MS)).await;
        seed_tombstone(&store, "ns", "col", "exact", Some(NOW - 30 * DAY_MS)).await;
        seed_tombstone(&store, "ns", "col", "young", Some(NOW - DAY_MS)).await;
        seed_tombstone(&store, "ns", "col", "future", Some(NOW + DAY_MS)).await;
        seed_tombstone(&store, "ns", "col", "unstamped", None).await;
        seed(&store, rec("ns", "col", "live", "content")).await;
        let live_key = RecordKey::new("ns", "col", "live");
        let live_before = store.raw_snapshot()[&live_key].clone();

        let report = collect(&store, &KeyPrefix::default(), THIRTY_DAYS, NOW)
            .await
            .unwrap();

        assert_eq!(
            report.purged,
            vec![
                RecordKey::new("ns", "col", "exact"),
                RecordKey::new("ns", "col", "old"),
            ]
        );
        assert_eq!(report.unstamped, 1);
        assert!(report.conflicts.is_empty());

        let after = store.raw_snapshot();
        assert!(!after.contains_key(&RecordKey::new("ns", "col", "old")));
        assert!(!after.contains_key(&RecordKey::new("ns", "col", "exact")));
        for kept in ["young", "future", "unstamped"] {
            let key = RecordKey::new("ns", "col", kept);
            assert!(after[&key].is_tombstone(), "{kept} must be kept");
        }
        assert_eq!(after[&live_key], live_before, "live record untouched");
    }

    #[tokio::test]
    async fn respects_the_prefix() {
        let store = MemStore::new();
        let old = Some(NOW - 60 * DAY_MS);
        seed_tombstone(&store, "ns", "col", "in", old).await;
        seed_tombstone(&store, "ns", "sibling", "out1", old).await;
        seed_tombstone(&store, "other", "col", "out2", old).await;

        let report = collect(
            &store,
            &KeyPrefix {
                namespace: Some("ns".into()),
                collection: Some("col".into()),
            },
            THIRTY_DAYS,
            NOW,
        )
        .await
        .unwrap();

        assert_eq!(report.purged, vec![RecordKey::new("ns", "col", "in")]);
        let after = store.raw_snapshot();
        assert!(after.contains_key(&RecordKey::new("ns", "sibling", "out1")));
        assert!(after.contains_key(&RecordKey::new("other", "col", "out2")));
    }

    #[tokio::test]
    async fn purges_a_tombstone_written_by_delete() {
        let store = MemStore::new().with_clock(NOW - 40 * DAY_MS);
        let key = RecordKey::new("ns", "col", "k");
        seed(&store, rec("ns", "col", "k", "x")).await;
        assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);

        let report = collect(&store, &KeyPrefix::default(), THIRTY_DAYS, NOW)
            .await
            .unwrap();

        assert_eq!(report.purged, vec![key.clone()]);
        assert_eq!(store.get_raw(&key).await.unwrap(), None);
        assert!(store.list_raw(&KeyPrefix::default()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_horizon_too_large_for_i64_millis_purges_nothing() {
        let store = MemStore::new();
        seed_tombstone(&store, "ns", "col", "ancient", Some(i64::MIN / 2)).await;

        let report = collect(&store, &KeyPrefix::default(), Duration::MAX, NOW)
            .await
            .unwrap();

        assert!(report.purged.is_empty());
        assert_eq!(store.raw_snapshot().len(), 1);
    }

    /// Wraps a `MemStore` and, on the first `purge` of `target`, recreates the
    /// key (a consumer `put(_, None)` over the tombstone) before delegating.
    /// This is the "key recreated during collection" race.
    struct RecreateBeforePurge {
        inner: MemStore,
        target: RecordKey,
        fired: AtomicBool,
    }

    #[async_trait]
    impl Store for RecreateBeforePurge {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get(key).await
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            self.inner.put(record, expected).await
        }
        async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.inner.list(prefix).await
        }
        async fn delete_as(
            &self,
            key: &RecordKey,
            expected: Option<Revision>,
            author: Option<Identity>,
        ) -> Result<DeleteResult> {
            self.inner.delete_as(key, expected, author).await
        }
        async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            self.inner.put_raw(record, expected).await
        }
        async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get_raw(key).await
        }
        async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.inner.list_raw(prefix).await
        }
        async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
            if key == &self.target && !self.fired.swap(true, Ordering::SeqCst) {
                let fresh = rec(&key.namespace, &key.collection, &key.id, "recreated");
                assert!(matches!(
                    self.inner.put(fresh, None).await?,
                    PutResult::Committed(_)
                ));
            }
            self.inner.purge(key, expected).await
        }
    }

    #[tokio::test]
    async fn recreation_during_collection_is_a_conflict_and_the_live_record_survives() {
        let key = RecordKey::new("ns", "col", "k");
        let store = RecreateBeforePurge {
            inner: MemStore::new(),
            target: key.clone(),
            fired: AtomicBool::new(false),
        };
        seed_tombstone(&store, "ns", "col", "k", Some(NOW - 60 * DAY_MS)).await;
        let tomb_rev = store.get_raw(&key).await.unwrap().unwrap().revision;

        let report = collect(&store, &KeyPrefix::default(), THIRTY_DAYS, NOW)
            .await
            .unwrap();

        assert!(report.purged.is_empty());
        assert_eq!(report.conflicts, vec![key.clone()]);
        let live = store.get(&key).await.unwrap().expect("recreated record survives");
        assert_eq!(live.body.bytes(), b"recreated");
        assert_eq!(live.revision.counter, tomb_rev.counter + 1);
    }
}
```

In `crates/gonzalo-core/src/lib.rs`, directly after `pub use reset::{ResetReport, reset};`, add:

```rust

pub mod collect;
pub use collect::{CollectReport, collect};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core --lib collect::`
Expected: compile FAIL with `error[E0425]: cannot find function \`collect\` in this scope` and `unresolved import \`collect::CollectReport\``.

- [ ] **Step 3: Write the implementation**

In `crates/gonzalo-core/src/collect.rs`, insert between the module doc comment and `#[cfg(test)]`:

```rust

use crate::{DeleteResult, KeyPrefix, RecordKey, Result, Store};
use std::time::Duration;

/// What a collection run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "a CollectReport may list conflicts (keys recreated during collection)"]
pub struct CollectReport {
    /// Tombstones physically removed, in `list_raw` order.
    pub purged: Vec<RecordKey>,
    /// Tombstones kept because they have no `deleted_at`.
    pub unstamped: usize,
    /// Purge lost an OCC race (the key was recreated during collection).
    pub conflicts: Vec<RecordKey>,
}

/// Purge tombstones under `prefix` whose `deleted_at` is at least `horizon` old.
///
/// Live records are never touched. A tombstone with no `deleted_at` is kept
/// and counted in [`CollectReport::unstamped`]. A future-dated `deleted_at`
/// (clock skew) gives a negative age and is kept, so skew can delay collection
/// but can't trigger it early. A horizon too large to express in `i64`
/// milliseconds purges nothing. `now_ms` is a parameter so tests control time;
/// the CLI passes the system clock.
pub async fn collect(
    store: &dyn Store,
    prefix: &KeyPrefix,
    horizon: Duration,
    now_ms: i64,
) -> Result<CollectReport> {
    let horizon_ms = i64::try_from(horizon.as_millis()).unwrap_or(i64::MAX);
    let mut report = CollectReport::default();
    for key in store.list_raw(prefix).await? {
        if !prefix.matches(&key) {
            continue;
        }
        let Some(record) = store.get_raw(&key).await? else {
            continue;
        };
        if !record.is_tombstone() {
            continue;
        }
        let Some(deleted_at) = record.deleted_at else {
            report.unstamped += 1;
            continue;
        };
        if now_ms.saturating_sub(deleted_at) < horizon_ms {
            continue;
        }
        match store.purge(&key, record.revision).await? {
            DeleteResult::Deleted => report.purged.push(key),
            DeleteResult::Conflict(_) => report.conflicts.push(key),
        }
    }
    Ok(report)
}
```

Replace the test module's `use crate::{…};` and `use std::time::Duration;` lines (both already in scope through `super::*`) with:

```rust
    use crate::{Body, Identity, Meta, PutResult, Record, RecordKind, Revision};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core --lib collect::`
Expected: `test result: ok. 5 passed; 0 failed`.

Run: `cargo test -p gonzalo-core --lib reset::`
Expected: `test result: ok. 5 passed; 0 failed` (still green).

Run: `cargo clippy -p gonzalo-core --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/gonzalo-core/src/collect.rs crates/gonzalo-core/src/lib.rs
git commit -m "feat(core): explicit tombstone collection with an operator horizon (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 3: CLI library — store opening, delete/reset/collect wrappers and argument parsers

**Files:**
- Modify: `crates/gonzalo-cli/src/lib.rs`: new section inserted directly above `// ─── sync_stores ───…` (line 986 today); `sync_stores` (lines 996–1007); new test module appended at the very end of the file, after the existing `mod tests`.

**Interfaces:**
- Consumes: `FsStore::new(root).with_ancestor_cap(cap) -> gonzalo_core::Result<FsStore>` (slice 2); `gonzalo_core::{reset, collect, ResetReport, CollectReport, DeleteResult, DEFAULT_ANCESTOR_CAP}` (Tasks 1–2, slice 1).
- Produces (all `pub` in `gonzalo_cli`):
  ```rust
  pub const CLI_AUTHOR: &str = "gonzalo-cli";
  pub fn open_store(root: &Path, ancestor_cap: usize) -> Result<FsStore>;
  pub enum DeleteOutcome { Deleted, Conflict { current: Revision } }
  pub async fn delete(root: &Path, ancestor_cap: usize, namespace: &str, collection: &str, id: &str, expected: Option<Revision>) -> Result<DeleteOutcome>;
  pub async fn reset(root: &Path, ancestor_cap: usize, namespace: String, collection: Option<String>) -> Result<gonzalo_core::ResetReport>;
  pub async fn collect(root: &Path, ancestor_cap: usize, namespace: Option<String>, collection: Option<String>, horizon: Duration, now_ms: i64) -> Result<gonzalo_core::CollectReport>;
  pub struct Horizon { pub raw: String, pub duration: Duration }
  pub fn parse_duration(raw: &str) -> std::result::Result<Duration, String>;
  pub fn parse_horizon(raw: &str) -> std::result::Result<Horizon, String>;
  pub fn parse_revision(raw: &str) -> std::result::Result<Revision, String>;
  pub async fn sync_stores_with_cap(a: &Path, b: &Path, ancestor_cap: usize) -> Result<SyncSummary>;
  ```
  (`Result` here is `anyhow::Result`, already imported at `lib.rs:4`.)

- [ ] **Step 1: Write the failing unit tests**

Append at the very end of `crates/gonzalo-cli/src/lib.rs`, after the closing `}` of `mod tests`:

```rust

#[cfg(test)]
mod tombstone_cli_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_duration_accepts_each_unit() {
        assert_eq!(parse_duration("30d"), Ok(Duration::from_secs(30 * 86_400)));
        assert_eq!(parse_duration("12h"), Ok(Duration::from_secs(12 * 3_600)));
        assert_eq!(parse_duration("90m"), Ok(Duration::from_secs(90 * 60)));
        assert_eq!(parse_duration("45s"), Ok(Duration::from_secs(45)));
        assert_eq!(parse_duration(" 7d "), Ok(Duration::from_secs(7 * 86_400)));
    }

    #[test]
    fn parse_duration_rejects_malformed_input() {
        for (raw, needle) in [
            ("", "missing unit"),
            ("30", "missing unit"),
            ("d", "expected a number"),
            ("-5d", "expected a number"),
            ("30x", "unknown unit"),
            ("30dd", "unknown unit"),
            ("1d2h", "unknown unit"),
            ("0d", "greater than zero"),
            ("99999999999999999999d", "too large"),
            ("18446744073709551615d", "too large"),
        ] {
            let err = parse_duration(raw).expect_err(raw);
            assert!(err.contains(needle), "{raw:?}: {err:?} should mention {needle:?}");
        }
    }

    #[test]
    fn parse_horizon_keeps_the_operator_spelling() {
        let h = parse_horizon(" 30d").unwrap();
        assert_eq!(h.raw, "30d");
        assert_eq!(h.duration, Duration::from_secs(2_592_000));
    }

    #[test]
    fn parse_revision_reads_the_json_that_get_prints() {
        let rev = parse_revision(r#"{"counter":3,"hash":"abc"}"#).unwrap();
        assert_eq!(
            rev,
            Revision {
                counter: 3,
                hash: ContentHash("abc".into())
            }
        );
        let err = parse_revision("3").unwrap_err();
        assert!(err.contains("expected a revision as JSON"), "{err}");
    }

    #[test]
    fn open_store_rejects_a_zero_ancestor_cap() {
        let root = TempDir::new().unwrap();
        let err = open_store(root.path(), 0).err().expect("cap 0 rejected");
        assert!(
            format!("{err:#}").contains("ancestor cap must be at least 1"),
            "{err:#}"
        );
        assert!(open_store(root.path(), 1).is_ok());
    }

    #[tokio::test]
    async fn delete_conflicts_on_a_stale_revision_and_attributes_the_tombstone() {
        let root = TempDir::new().unwrap();
        let store = FsStore::new(root.path());
        let body = Body::Inline(b"x".to_vec());
        let record = Record {
            key: RecordKey::new("ns", "col", "k"),
            kind: RecordKind::Topic,
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
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
        };
        let live = record.revision.clone();
        assert!(matches!(
            store.put(record, None).await.unwrap(),
            PutResult::Committed(_)
        ));
        let stale = Revision {
            counter: 99,
            hash: ContentHash("deadbeef".into()),
        };

        let outcome = delete(root.path(), 32, "ns", "col", "k", Some(stale))
            .await
            .unwrap();
        assert!(matches!(outcome, DeleteOutcome::Conflict { ref current } if *current == live));

        let outcome = delete(root.path(), 32, "ns", "col", "k", Some(live))
            .await
            .unwrap();
        assert!(matches!(outcome, DeleteOutcome::Deleted));
        assert_eq!(get(root.path(), "ns", "col", "k").await.unwrap(), None);

        // The seeded record was authored by "t"; the tombstone names the CLI.
        let tomb = store
            .get_raw(&RecordKey::new("ns", "col", "k"))
            .await
            .unwrap()
            .expect("tombstone stored");
        assert!(tomb.is_tombstone());
        assert_eq!(tomb.meta.author, Identity::new(CLI_AUTHOR));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-cli --lib tombstone_cli_tests`
Expected: compile FAIL with `cannot find function \`parse_duration\``, `parse_horizon`, `parse_revision`, `open_store`, `delete`, and `cannot find type \`DeleteOutcome\``.

- [ ] **Step 3: Write the implementation**

In `crates/gonzalo-cli/src/lib.rs`, insert directly **above** the line `// ─── sync_stores ─────…` (line 986 today):

```rust
// ─── delete / reset / collect (tombstones, gonzalo#203) ─────────────────────

/// Open the fs store at `root` with an explicit ancestor cap (spec §3.9).
/// A cap of 0 is rejected here, before any record is touched.
pub fn open_store(root: &Path, ancestor_cap: usize) -> Result<FsStore> {
    FsStore::new(root)
        .with_ancestor_cap(ancestor_cap)
        .with_context(|| format!("opening store at {}", root.display()))
}

/// Outcome of [`delete`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// A tombstone was written, or the record was already absent or deleted.
    Deleted,
    /// `expected` did not match. Nothing was written. `current` is the live
    /// record's revision.
    Conflict { current: Revision },
}

/// The identity the CLI stamps on records it writes. Matches `migrate`
/// (`Identity::new("gonzalo-cli")` above) and the `ticket sync --author`
/// default.
pub const CLI_AUTHOR: &str = "gonzalo-cli";

/// Delete one record by writing a tombstone attributed to [`CLI_AUTHOR`]
/// (spec §3.1–§3.2). Deleting an absent or already-deleted key is `Deleted`.
pub async fn delete(
    root: &Path,
    ancestor_cap: usize,
    namespace: &str,
    collection: &str,
    id: &str,
    expected: Option<Revision>,
) -> Result<DeleteOutcome> {
    let store = open_store(root, ancestor_cap)?;
    let key = RecordKey::new(namespace, collection, id);
    let author = Some(Identity::new(CLI_AUTHOR));
    Ok(match store.delete_as(&key, expected, author).await? {
        gonzalo_core::DeleteResult::Deleted => DeleteOutcome::Deleted,
        gonzalo_core::DeleteResult::Conflict(conflict) => DeleteOutcome::Conflict {
            current: conflict.current.revision,
        },
    })
}

/// Tombstone every live record in `namespace` (optionally one `collection`)
/// via [`gonzalo_core::reset`].
pub async fn reset(
    root: &Path,
    ancestor_cap: usize,
    namespace: String,
    collection: Option<String>,
) -> Result<gonzalo_core::ResetReport> {
    let store = open_store(root, ancestor_cap)?;
    let prefix = KeyPrefix {
        namespace: Some(namespace),
        collection,
    };
    Ok(gonzalo_core::reset(&store, &prefix).await?)
}

/// Purge tombstones at least `horizon` old via [`gonzalo_core::collect`].
/// `namespace == None` collects across the whole store.
pub async fn collect(
    root: &Path,
    ancestor_cap: usize,
    namespace: Option<String>,
    collection: Option<String>,
    horizon: Duration,
    now_ms: i64,
) -> Result<gonzalo_core::CollectReport> {
    let store = open_store(root, ancestor_cap)?;
    let prefix = KeyPrefix {
        namespace,
        collection,
    };
    Ok(gonzalo_core::collect(&store, &prefix, horizon, now_ms).await?)
}

/// A collection horizon as the operator typed it, plus its parsed value, so
/// command output can echo the horizon it used (spec §3.10, §8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Horizon {
    pub raw: String,
    pub duration: Duration,
}

/// Parse a duration of one positive whole number followed by exactly one unit:
/// `d`, `h`, `m` or `s` (e.g. `30d`, `12h`). Hand-rolled on purpose. The
/// workspace has no `humantime`, and a horizon never needs compound or
/// fractional forms.
pub fn parse_duration(raw: &str) -> std::result::Result<Duration, String> {
    let s = raw.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{raw:?}: missing unit (use d, h, m or s, e.g. 30d)"))?;
    let (digits, unit) = s.split_at(split);
    if digits.is_empty() {
        return Err(format!("{raw:?}: expected a number before the unit, e.g. 30d"));
    }
    let per_unit: u64 = match unit {
        "d" => 86_400,
        "h" => 3_600,
        "m" => 60,
        "s" => 1,
        _ => return Err(format!("{raw:?}: unknown unit {unit:?} (use d, h, m or s)")),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{raw:?}: duration is too large"))?;
    if n == 0 {
        return Err(format!("{raw:?}: horizon must be greater than zero"));
    }
    let secs = n
        .checked_mul(per_unit)
        .ok_or_else(|| format!("{raw:?}: duration is too large"))?;
    Ok(Duration::from_secs(secs))
}

/// clap value parser for `--older-than`.
pub fn parse_horizon(raw: &str) -> std::result::Result<Horizon, String> {
    Ok(Horizon {
        raw: raw.trim().to_string(),
        duration: parse_duration(raw)?,
    })
}

/// clap value parser for `--expected`: a revision as JSON, exactly as
/// `gonzalo get` prints it, e.g. `{"counter":1,"hash":"…"}`.
pub fn parse_revision(raw: &str) -> std::result::Result<Revision, String> {
    serde_json::from_str(raw).map_err(|e| {
        format!(r#"expected a revision as JSON, e.g. {{"counter":1,"hash":"…"}} ({e})"#)
    })
}

```

Then replace the current `sync_stores` (lines 996–1000 today):

```rust
/// Sync two filesystem stores via [`gonzalo_core::sync`].
pub async fn sync_stores(a: &Path, b: &Path) -> Result<SyncSummary> {
    let store_a = FsStore::new(a);
    let store_b = FsStore::new(b);
```

with:

```rust
/// Sync two filesystem stores via [`gonzalo_core::sync`], at the default
/// ancestor cap.
pub async fn sync_stores(a: &Path, b: &Path) -> Result<SyncSummary> {
    sync_stores_with_cap(a, b, gonzalo_core::DEFAULT_ANCESTOR_CAP).await
}

/// [`sync_stores`] with an explicit ancestor cap for both stores (spec §3.9).
pub async fn sync_stores_with_cap(a: &Path, b: &Path, ancestor_cap: usize) -> Result<SyncSummary> {
    let store_a = open_store(a, ancestor_cap)?;
    let store_b = open_store(b, ancestor_cap)?;
```

Leave the rest of the function body (the `sync` call and the `SyncSummary` construction) exactly as it is. Slice 5 may have changed it.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-cli --lib tombstone_cli_tests`
Expected: `test result: ok. 6 passed; 0 failed`.

Run: `cargo test -p gonzalo-cli --lib sync_stores`
Expected: `test result: ok.` (the existing `sync_stores_copies_to_b` still passes).

Run: `cargo clippy -p gonzalo-cli --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/gonzalo-cli/src/lib.rs
git commit -m "feat(cli): delete/reset/collect wrappers, horizon and revision parsers (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 4: CLI subcommands, exit codes and integration tests

**Files:**
- Modify: `crates/gonzalo-cli/src/main.rs`: imports (lines 3–12), `Commands` enum (the `Sync` variant at 129–137, with new variants after it), `main` (212–214, the watch `return Ok(());` at 306, the `Sync` arm at 361–367, the final `Ok(())` at 428)
- Modify: `crates/gonzalo-cli/tests/cli.rs` (imports at 5–7; append tests)

**Interfaces:**
- Consumes (Task 3): `gonzalo_cli::{DeleteOutcome, Horizon, collect, delete, parse_horizon, parse_revision, reset, sync_stores_with_cap}`; `gonzalo_core::{DEFAULT_ANCESTOR_CAP, RecordKey, Revision, now_ms, record_components}`.
- Produces: the user-visible CLI contract.

| Command | stdout | stderr | Exit |
|---|---|---|---|
| `gonzalo delete --namespace N --collection C --id I [--expected REV] [--root R] [--ancestor-cap K]` | `deleted: N/C/I` | — | 0 |
| same, `expected` stale | `conflict: N/C/I` then `current:  {"counter":…,"hash":"…"}` | — | 3 |
| `gonzalo reset --namespace N [--collection C] [--root R] [--ancestor-cap K]` | `X deleted, M conflicts` | one `conflict: <key>` line per conflict | 0 if M == 0, else 3 |
| `gonzalo collect --older-than D [--namespace N [--collection C]] [--root R] [--ancestor-cap K]` | `horizon:   D (Ss)`, `purged:    P`, `unstamped: U`, `conflicts: Q` | one `conflict: <key>` line per conflict | 0 |
| missing `--namespace` on reset, missing `--older-than`, `--collection` without `--namespace` on collect, malformed `--expected`/`--older-than` | — | clap usage error | 2 |
| I/O or store error (including `--ancestor-cap 0`) | — | `Error: …` | 1 |

- [ ] **Step 1: Write the failing integration tests**

In `crates/gonzalo-cli/tests/cli.rs`, replace lines 5–7:

```rust
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;
```

with:

```rust
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;
```

Append at the end of the file:

```rust
// ── delete / reset / collect over tombstones (gonzalo#203, spec §3.10) ──────

/// Run `gonzalo <args> --root <root>`.
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .arg("--root")
        .arg(root)
        .output()
        .expect("run gonzalo")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `gonzalo list` output as sorted lines (the fs store lists in OS directory
/// order, which is not stable across platforms).
fn listed_keys(root: &Path) -> Vec<String> {
    let out = run(root, &["list"]);
    assert!(out.status.success(), "{out:?}");
    let mut keys: Vec<String> = stdout(&out).lines().map(str::to_owned).collect();
    keys.sort();
    keys
}

/// Import one small file per id into `namespace/collection` (id = file name).
fn seed(root: &Path, namespace: &str, collection: &str, ids: &[&str]) {
    let src = TempDir::new().unwrap();
    for id in ids {
        std::fs::write(src.path().join(id), format!("body of {id}")).unwrap();
    }
    let out = Command::new(bin())
        .args(["migrate", "--namespace", namespace, "--collection", collection, "--root"])
        .arg(root)
        .arg(src.path())
        .output()
        .expect("run gonzalo migrate");
    assert!(out.status.success(), "seeding failed: {out:?}");
}

/// The on-disk JSON file of a record in the fs store.
fn record_file(root: &Path, namespace: &str, collection: &str, id: &str) -> PathBuf {
    let key = gonzalo_core::RecordKey::new(namespace, collection, id);
    let (ns, col, file) = gonzalo_core::record_components(&key);
    root.join(ns).join(col).join(file)
}

/// Rewrite a stored record's JSON in place.
fn edit_record_file(path: &Path, edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>)) {
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    edit(value.as_object_mut().expect("record JSON is an object"));
    std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

#[test]
fn delete_hides_the_record_and_exits_zero() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);

    let out = run(
        root.path(),
        &["delete", "--namespace", "ns", "--collection", "col", "--id", "note.md"],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(stdout(&out), "deleted: ns/col/note.md\n");

    let get = run(root.path(), &["get", "ns", "col", "note.md"]);
    assert!(!get.status.success(), "a deleted record reads as absent");
    assert!(
        record_file(root.path(), "ns", "col", "note.md").is_file(),
        "the tombstone stays on disk"
    );
}

#[test]
fn delete_of_an_absent_record_exits_zero() {
    let root = TempDir::new().unwrap();
    let out = run(
        root.path(),
        &["delete", "--namespace", "ns", "--collection", "col", "--id", "nope"],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(stdout(&out), "deleted: ns/col/nope\n");
}

#[test]
fn delete_with_a_stale_expected_revision_exits_one_and_keeps_the_record() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);

    let out = run(
        root.path(),
        &[
            "delete", "--namespace", "ns", "--collection", "col", "--id", "note.md",
            "--expected", r#"{"counter":99,"hash":"deadbeef"}"#,
        ],
    );
    assert_eq!(out.status.code(), Some(3), "a conflict exits 3: {out:?}");
    let text = stdout(&out);
    assert!(text.starts_with("conflict: ns/col/note.md\n"), "{text:?}");
    assert!(text.contains("current:  {\"counter\":0,"), "{text:?}");

    let get = run(root.path(), &["get", "ns", "col", "note.md"]);
    assert!(get.status.success(), "a conflicted delete writes nothing");
}

#[test]
fn delete_with_the_revision_get_printed_succeeds() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);
    let get = run(root.path(), &["get", "ns", "col", "note.md"]);
    let record: serde_json::Value = serde_json::from_slice(&get.stdout).unwrap();
    let revision = record["revision"].to_string();

    let out = run(
        root.path(),
        &[
            "delete", "--namespace", "ns", "--collection", "col", "--id", "note.md",
            "--expected", &revision,
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn delete_rejects_a_malformed_expected_revision_at_parse_time() {
    let root = TempDir::new().unwrap();
    let out = run(
        root.path(),
        &[
            "delete", "--namespace", "ns", "--collection", "col", "--id", "x",
            "--expected", "3",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(stderr(&out).contains("expected a revision as JSON"), "{out:?}");
}

#[test]
fn help_documents_each_commands_exit_codes() {
    for (command, line) in [
        ("delete", "Exit codes: 0 deleted, 1 error, 2 usage error, 3 conflict"),
        (
            "reset",
            "Exit codes: 0 no conflicts, 1 error, 2 usage error, 3 one or more conflicts",
        ),
        (
            "collect",
            "Exit codes: 0 success (conflicts are reported, not failures), 1 error, 2 usage error",
        ),
    ] {
        let out = Command::new(bin())
            .args([command, "--help"])
            .output()
            .expect("run gonzalo <command> --help");
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        assert!(
            stdout(&out).contains(line),
            "{command} --help must state {line:?}, got {:?}",
            stdout(&out)
        );
    }
}

#[test]
fn a_zero_ancestor_cap_is_an_error_and_touches_nothing() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);
    let out = run(
        root.path(),
        &[
            "delete", "--namespace", "ns", "--collection", "col", "--id", "note.md",
            "--ancestor-cap", "0",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr(&out).contains("ancestor cap must be at least 1"), "{out:?}");
    assert!(run(root.path(), &["get", "ns", "col", "note.md"]).status.success());
}

#[test]
fn reset_prints_its_summary_scopes_to_the_prefix_and_is_idempotent() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["a.md", "b.md"]);
    seed(root.path(), "ns", "other", &["c.md"]);
    seed(root.path(), "keep", "col", &["d.md"]);

    let out = run(root.path(), &["reset", "--namespace", "ns", "--collection", "col"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(stdout(&out), "2 deleted, 0 conflicts\n");

    // `FsStore::list` walks directories in OS order, so compare sorted lines.
    assert_eq!(listed_keys(root.path()), ["keep/col/d.md", "ns/other/c.md"]);

    let again = run(root.path(), &["reset", "--namespace", "ns", "--collection", "col"]);
    assert_eq!(again.status.code(), Some(0), "{again:?}");
    assert_eq!(stdout(&again), "0 deleted, 0 conflicts\n");

    let whole = run(root.path(), &["reset", "--namespace", "ns"]);
    assert_eq!(stdout(&whole), "1 deleted, 0 conflicts\n");
    assert_eq!(listed_keys(root.path()), ["keep/col/d.md"]);
}

#[test]
fn reset_without_namespace_is_a_usage_error() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["a.md"]);
    let out = run(root.path(), &["reset", "--collection", "col"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(stderr(&out).contains("--namespace"), "{out:?}");
    assert!(run(root.path(), &["get", "ns", "col", "a.md"]).status.success());
}

#[test]
fn collect_without_older_than_is_a_usage_error() {
    let root = TempDir::new().unwrap();
    let out = run(root.path(), &["collect", "--namespace", "ns"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(stderr(&out).contains("--older-than"), "{out:?}");
}

#[test]
fn collect_rejects_a_bad_horizon_and_collection_without_namespace() {
    let root = TempDir::new().unwrap();
    let bad = run(root.path(), &["collect", "--older-than", "30x"]);
    assert_eq!(bad.status.code(), Some(2), "{bad:?}");
    assert!(stderr(&bad).contains("unknown unit"), "{bad:?}");

    let orphan = run(root.path(), &["collect", "--older-than", "1d", "--collection", "col"]);
    assert_eq!(orphan.status.code(), Some(2), "{orphan:?}");
    assert!(stderr(&orphan).contains("--namespace"), "{orphan:?}");
}

#[test]
fn collect_purges_old_tombstones_and_reports_what_it_kept() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["old.md", "young.md", "unstamped.md", "live.md"]);
    for id in ["old.md", "young.md", "unstamped.md"] {
        let out = run(
            root.path(),
            &["delete", "--namespace", "ns", "--collection", "col", "--id", id],
        );
        assert!(out.status.success(), "{out:?}");
    }
    // Age one tombstone to the epoch and strip the stamp from another.
    edit_record_file(&record_file(root.path(), "ns", "col", "old.md"), |r| {
        r.insert("deleted_at".into(), serde_json::json!(0));
    });
    edit_record_file(&record_file(root.path(), "ns", "col", "unstamped.md"), |r| {
        r.remove("deleted_at");
    });

    let out = run(root.path(), &["collect", "--older-than", "30d"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        stdout(&out),
        "horizon:   30d (2592000s)\npurged:    1\nunstamped: 1\nconflicts: 0\n"
    );

    assert!(!record_file(root.path(), "ns", "col", "old.md").exists(), "purged");
    assert!(record_file(root.path(), "ns", "col", "young.md").is_file(), "too young");
    assert!(record_file(root.path(), "ns", "col", "unstamped.md").is_file(), "unstamped");
    assert!(run(root.path(), &["get", "ns", "col", "live.md"]).status.success(), "live untouched");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-cli --test cli`
Expected: the pre-existing 5 tests pass. The new tests FAIL: clap reports `error: unrecognized subcommand 'delete'` (and `'reset'`, `'collect'`) with exit code 2. For example `delete_hides_the_record_and_exits_zero` panics with `assertion \`left == right\` failed … left: Some(2) right: Some(0)`. Result line: `test result: FAILED. 5 passed; 12 failed`.

- [ ] **Step 3: Implement the subcommands**

In `crates/gonzalo-cli/src/main.rs`, replace lines 3–12:

```rust
use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use gonzalo_cli::{
    IndexFilter, WatchConfig, gc, get, index_with_gc_filtered, list, migrate, resolve_parse_worker,
    status, sync_stores, ticket_move, ticket_sync, watch,
};
use gonzalo_core::RecordKind;
use gonzalo_store_fs::expand_tilde;
use std::path::PathBuf;
use std::time::Duration;
```

with:

```rust
use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use gonzalo_cli::{
    DeleteOutcome, Horizon, IndexFilter, WatchConfig, collect, delete, gc, get,
    index_with_gc_filtered, list, migrate, parse_horizon, parse_revision, reset,
    resolve_parse_worker, status, sync_stores_with_cap, ticket_move, ticket_sync, watch,
};
use gonzalo_core::{DEFAULT_ANCESTOR_CAP, RecordKey, RecordKind, Revision};
use gonzalo_store_fs::expand_tilde;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Exit code for a write conflict (`delete` with a stale `--expected`, or
/// `reset` leaving any record live). Distinct from 1 (error, via anyhow) and
/// 2 (usage error, via clap), so automation can tell "retry" from "broken".
const EXIT_CONFLICT: u8 = 3;
```

Replace the `Sync` variant (lines 129–137):

```rust
    /// Sync two filesystem stores.
    Sync {
        /// Root directory of store A.
        #[arg(value_parser = store_root)]
        a: PathBuf,
        /// Root directory of store B.
        #[arg(value_parser = store_root)]
        b: PathBuf,
    },
```

with:

```rust
    /// Sync two filesystem stores.
    Sync {
        /// Root directory of store A.
        #[arg(value_parser = store_root)]
        a: PathBuf,
        /// Root directory of store B.
        #[arg(value_parser = store_root)]
        b: PathBuf,
        /// Most recent revisions a record remembers in `ancestors` (at least 1).
        #[arg(long, default_value_t = DEFAULT_ANCESTOR_CAP)]
        ancestor_cap: usize,
    },
    /// Delete one record by writing a tombstone, which replicates on sync.
    /// Deleting an absent or already-deleted record succeeds. Exits 3 when
    /// `--expected` doesn't match the current revision.
    #[command(after_help = "Exit codes: 0 deleted, 1 error, 2 usage error, 3 conflict")]
    Delete {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
        /// Namespace of the record.
        #[arg(long)]
        namespace: String,
        /// Collection of the record.
        #[arg(long)]
        collection: String,
        /// ID of the record.
        #[arg(long)]
        id: String,
        /// Only delete if the current revision is this one, as JSON exactly as
        /// `gonzalo get` prints it: '{"counter":1,"hash":"…"}'.
        #[arg(long, value_parser = parse_revision)]
        expected: Option<Revision>,
        /// Most recent revisions a record remembers in `ancestors` (at least 1).
        #[arg(long, default_value_t = DEFAULT_ANCESTOR_CAP)]
        ancestor_cap: usize,
    },
    /// Tombstone every live record in a namespace, or in one collection of it.
    /// Not atomic but idempotent: re-run to finish after conflicts. Exits 3 if
    /// any record was edited concurrently and left live.
    #[command(
        after_help = "Exit codes: 0 no conflicts, 1 error, 2 usage error, 3 one or more conflicts"
    )]
    Reset {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
        /// Namespace to reset (required).
        #[arg(long)]
        namespace: String,
        /// Limit the reset to this collection.
        #[arg(long)]
        collection: Option<String>,
        /// Most recent revisions a record remembers in `ancestors` (at least 1).
        #[arg(long, default_value_t = DEFAULT_ANCESTOR_CAP)]
        ancestor_cap: usize,
    },
    /// Physically purge tombstones older than a horizon. A peer that hasn't
    /// synced since a purged delete will bring that record back, so choose a
    /// horizon longer than any peer's longest gap between syncs. (Unrelated
    /// to `gc`, which sweeps code-graph slices.)
    #[command(
        after_help = "Exit codes: 0 success (conflicts are reported, not failures), 1 error, 2 usage error"
    )]
    Collect {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
        /// Minimum tombstone age to purge: a number and one unit of d, h, m or
        /// s, e.g. 30d. Required; there is no default.
        #[arg(long, value_parser = parse_horizon)]
        older_than: Horizon,
        /// Limit collection to this namespace (default: the whole store).
        #[arg(long)]
        namespace: Option<String>,
        /// Limit collection to this collection of `--namespace`.
        #[arg(long, requires = "namespace")]
        collection: Option<String>,
        /// Most recent revisions a record remembers in `ancestors` (at least 1).
        #[arg(long, default_value_t = DEFAULT_ANCESTOR_CAP)]
        ancestor_cap: usize,
    },
```

Replace lines 212–214:

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
```

with:

```rust
#[tokio::main]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
```

In the `Index` arm, replace (line 305–306):

```rust
                watch(&root, &src, &repo, &view, config, gc).await?;
                return Ok(());
```

with:

```rust
                watch(&root, &src, &repo, &view, config, gc).await?;
                return Ok(ExitCode::SUCCESS);
```

Replace the `Sync` arm (lines 361–367):

```rust
        Commands::Sync { a, b } => {
            let summary = sync_stores(&a, &b).await?;
```

with:

```rust
        Commands::Sync { a, b, ancestor_cap } => {
            let summary = sync_stores_with_cap(&a, &b, ancestor_cap).await?;
```

Leave the four `println!` lines of that arm unchanged. Directly after that arm's closing `}`, and before `Commands::Ticket { command } => match command {`, insert:

```rust

        Commands::Delete {
            root,
            namespace,
            collection,
            id,
            expected,
            ancestor_cap,
        } => {
            let key = RecordKey::new(&namespace, &collection, &id);
            match delete(&root, ancestor_cap, &namespace, &collection, &id, expected).await? {
                DeleteOutcome::Deleted => println!("deleted: {key}"),
                DeleteOutcome::Conflict { current } => {
                    println!("conflict: {key}");
                    println!("current:  {}", serde_json::to_string(&current)?);
                    return Ok(ExitCode::from(EXIT_CONFLICT));
                }
            }
        }

        Commands::Reset {
            root,
            namespace,
            collection,
            ancestor_cap,
        } => {
            let report = reset(&root, ancestor_cap, namespace, collection).await?;
            for key in &report.conflicts {
                eprintln!("conflict: {key}");
            }
            println!(
                "{} deleted, {} conflicts",
                report.deleted.len(),
                report.conflicts.len()
            );
            if !report.conflicts.is_empty() {
                return Ok(ExitCode::from(EXIT_CONFLICT));
            }
        }

        Commands::Collect {
            root,
            older_than,
            namespace,
            collection,
            ancestor_cap,
        } => {
            let report = collect(
                &root,
                ancestor_cap,
                namespace,
                collection,
                older_than.duration,
                gonzalo_core::now_ms(),
            )
            .await?;
            println!(
                "horizon:   {} ({}s)",
                older_than.raw,
                older_than.duration.as_secs()
            );
            println!("purged:    {}", report.purged.len());
            println!("unstamped: {}", report.unstamped);
            println!("conflicts: {}", report.conflicts.len());
            for key in &report.conflicts {
                eprintln!("conflict: {key}");
            }
        }
```

Replace the final lines of `main` (427–429):

```rust
    }

    Ok(())
}
```

with:

```rust
    }

    Ok(ExitCode::SUCCESS)
}
```

Note: `gonzalo_core::now_ms` is re-exported from the crate root per the shared contract. If the compiler reports it isn't, use `gonzalo_core::tombstone::now_ms()` instead and list that under CONTRACT GAPS in the PR description.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-cli --test cli`
Expected: `test result: ok. 17 passed; 0 failed`.

Run: `cargo test -p gonzalo-cli`
Expected: every test binary reports `test result: ok.`

Run: `cargo clippy -p gonzalo-cli --all-targets --all-features -- -D warnings`
Expected: no warnings.

Manual smoke check of help text:

Run: `cargo run -q -p gonzalo-cli --bin gonzalo -- collect --help`
Expected: lists `--older-than <OLDER_THAN>`, `--namespace`, `--collection`, `--ancestor-cap <ANCESTOR_CAP>` with `[default: 32]`.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/gonzalo-cli/src/main.rs crates/gonzalo-cli/tests/cli.rs
git commit -m "feat(cli): gonzalo delete, reset and collect with exit codes (#203)

Adds --ancestor-cap to the new commands and to sync. main now returns
ExitCode: a conflict exits 3 (1 stays error, 2 usage error), and each
command's --help states its exit codes. CLI deletes are attributed to
gonzalo-cli.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 5: Full gate, push, PR

**Files:** none changed (verification and delivery only).

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces: an open PR, green in CI, whose body says `Closes #203`.

- [ ] **Step 1: Run the full gate, one bare command at a time**

```bash
cargo fmt --all -- --check
```
Expected: no output, exit 0.

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```
Expected: `Finished`, no `warning:` or `error:` lines.

```bash
cargo build --workspace --all-targets --all-features
```
Expected: `Finished`.

```bash
cargo test --workspace --all-features
```
Expected: every `test result:` line reads `ok.`, with 0 failed, including `gonzalo-core` (`reset::tests` 5, `collect::tests` 5) and `gonzalo-cli` (`tombstone_cli_tests` 6, `tests/cli.rs` 17).

If any command fails, fix it, commit the fix with the session trailer, and rerun **all four** commands from the top.

- [ ] **Step 2: Push the branch**

```bash
git push -u origin issue-203-reset-collect-cli
```
Expected: `branch 'issue-203-reset-collect-cli' set up to track 'origin/issue-203-reset-collect-cli'`.

- [ ] **Step 3: Open the PR**

```bash
gh pr create --base main --head issue-203-reset-collect-cli \
  --title "feat: namespace reset, tombstone collection and CLI delete/reset/collect (#203)" \
  --body "$(cat <<'EOF'
Closes #203

Slice 6 of the replicated-deletion plan (`docs/superpowers/plans/2026-09-13-tombstones-06-reset-collect-cli.md`, spec §3.7–§3.10).

## What

- `gonzalo_core::reset(store, prefix) -> ResetReport`: tombstones every live record under a namespace (optionally a collection) through OCC `delete`. Refuses a prefix without a namespace. Not atomic but idempotent: a concurrent edit is reported in `conflicts`, and a re-run finishes the job.
- `gonzalo_core::collect(store, prefix, horizon, now_ms) -> CollectReport`: purges tombstones at least `horizon` old through raw reads and OCC `purge`. Keeps unstamped (counted), too-young and future-dated tombstones. A recreation during collection is reported in `conflicts`, and the live record survives.
- CLI:
  - `gonzalo delete --namespace --collection --id [--expected '<revision JSON as printed by get>']` (exit 0 deleted, 3 conflict; the tombstone is attributed to `gonzalo-cli`)
  - `gonzalo reset --namespace [--collection]` (prints `N deleted, M conflicts`; exit 3 if M > 0)
  - `gonzalo collect --older-than 30d [--namespace [--collection]]` (prints horizon / purged / unstamped / conflicts; exit 0)
  - `--ancestor-cap <n>` on those three commands and on `sync`.
- `main` returns `ExitCode`. Exit codes are 0 success, 1 error, 2 usage error (clap) and 3 conflict, and each new command's `--help` states them.
- `--older-than` uses a small hand-rolled `Nd|Nh|Nm|Ns` parser. No new dependency.

## Tests

- core: 5 reset tests (namespace required, scoping, skip already-deleted, concurrent-edit race + idempotent re-runs) and 5 collect tests (eligibility matrix, prefix, delete→collect, overflow horizon, recreation race).
- cli: 6 unit tests (parsers, zero cap, delete conflict) and 12 integration tests for exit codes, help text, summary lines and clap rejections (§6.6).

## Verification

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace --all-targets --all-features`, `cargo test --workspace --all-features`: all green locally.

https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```
Expected: `gh` prints the PR URL.

- [ ] **Step 4: Wait for CI and merge**

```bash
gh pr checks --watch
```
Expected: every check `pass`. Then merge (squash, per repo convention):

```bash
gh pr merge --squash --delete-branch
```
Expected: `✓ Squashed and merged pull request` and issue #203 closed automatically.

---

## Self-Review

**Spec coverage**

| Spec item | Where |
|---|---|
| §3.7 signature, namespace required, list→get→delete(Some), conflicts not retried, vanished key skipped, idempotent | Task 1 (implementation + 5 tests) |
| §3.8 signature, list_raw/get_raw, tombstones only, unstamped counted, young/future skipped, purge(revision), conflict on recreation | Task 2 (implementation + 5 tests) |
| §3.9 `--ancestor-cap` on the CLI, 0 rejected | Task 3 `open_store`, Task 4 flags + `a_zero_ancestor_cap_is_an_error_and_touches_nothing` |
| §3.10 delete/reset/collect behaviour, output, exit codes; reset namespace a clap error; collect with no namespace runs store-wide | Task 4 |
| §3.8 "no default horizon; CLI requires `--older-than`" | Task 4 (`older_than: Horizon`, no default) + `collect_without_older_than_is_a_usage_error` |
| §6.4 all five bullets | Task 1: `refuses_a_prefix_without_a_namespace`, `concurrent_edit_is_a_conflict_and_rerunning_is_idempotent`, `collection_scope_leaves_sibling_collections_alone`. Task 2: `purges_only_stamped_tombstones_at_least_horizon_old`, `recreation_during_collection_is_a_conflict_and_the_live_record_survives` |
| §6.6 all three bullets | Task 4 integration tests |
| `Closes #203` | Task 5 |

**Known gap, deliberate:** `reset`'s exit-1-on-conflict path isn't covered by an integration test. A concurrent edit can't be staged deterministically from outside the process. The branch is three lines, mirrors `delete`'s tested `ExitCode::from(EXIT_CONFLICT)` (exit 3) path, and the conflict reporting itself is covered by the core race test.

**Type consistency:** `ResetReport`/`CollectReport` field names match the spec. `DeleteOutcome::Conflict { current: Revision }` is used identically in Task 3 (definition, unit test) and Task 4 (`main`). `Horizon { raw, duration }` matches `parse_horizon` and `main`. `open_store(root, ancestor_cap)` has the same argument order everywhere.
