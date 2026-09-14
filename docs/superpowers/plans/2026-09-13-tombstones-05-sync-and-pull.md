# Tombstones Slice 5: Sync and Git Pull Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `sync` and non-fast-forward git `pull` replicate tombstones. They use raw reads and the in-record ancestor list to tell "behind" apart from "diverged" (spec §3.4, §3.5).

**Architecture:** `sync_pass` reads through `list_raw`/`get_raw`. It classifies each two-sided key as in-sync, A-ahead, B-ahead or diverged, using `Record::ancestors`. Ahead keys are overwritten conditionally. For diverged keys: two tombstones converge on the higher `(counter, hash)`, one tombstone is a `SyncConflict`, and two live records take today's merge path with folded ancestors. `merge_non_ff` in `gonzalo-store-git` applies the same kind checks before its body comparison. New pure helpers in `gonzalo-core/src/tombstone.rs` hold that shared logic, so sync and pull agree by construction:
- `reconciled_ancestors`
- `tombstone_winner`
- `reconciled_record`, which builds the merged record for both sync's `build_merged` and git's `merged_record`. It was added by pre-flight Ruling D11.

**Tech Stack:** Rust 2024 (MSRV 1.95), tokio, async-trait, serde_json, git2.

**Spec:** `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` (§3.4 sync decision table, §3.5 git pull, §6.2, §6.3). The overview and shared contract are in `docs/superpowers/plans/2026-09-13-tombstones-00-overview.md`. Read both before starting.

**Depends on:** slice 1 (core model, planners, `fold_ancestors`, `get_raw`/`list_raw`/`purge`, reference `MemStore`) and slice 2 (fs and git stores write tombstones and gain `cap: usize` + `with_ancestor_cap`). Both are assumed **merged to `main`**. Branch from an up-to-date `main`.
- *As executed:* slices 1–4 were merged at `734f8cc`, and this slice runs on `feat/203-tombstones-05-sync-pull`.
- `ServerStore`'s raw methods are real (slice 4), so sync against a daemon uses the replication routes.
- A daemon older than slice 4 makes sync fail with `DAEMON_PREDATES_REPLICATION`, with no consumer fallback.

## Global Constraints

- Verification gate before every push and PR, matching CI exactly. Run each as a bare command, one per line, never joined with `&&`:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo build --workspace --all-targets --all-features`
  - `cargo test --workspace --all-features`
- Always open a PR and merge it after CI is green. Never push to `main`.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`. PR bodies end with `https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.
- PR body says `Part of #203` (only slice 6 says `Closes #203`).
- Tombstone hash domain string, verbatim: `gonzalo:tombstone:v1`.
- Default ancestor cap: `32`. A cap of `0` is rejected at construction.
- `deleted_at` unit: milliseconds since the Unix epoch, `i64`.
- New `Record` fields are `#[serde(default, skip_serializing_if = ...)]`. Never add `deny_unknown_fields`.
- `get_raw`, `list_raw` and `purge` are required trait methods. **Never** give them a default implementation, and never fall back from a raw read to a consumer read. Sync and pull must use raw reads only.
- Workspace lints: `unsafe_code = "forbid"`, clippy `all = warn`, promoted to errors by `-D warnings`.
- No release is tagged until slices 1–5 have all merged (§7). This slice is the last one release-blocking.
- Contract names from the overview and the slice 1 plan are used exactly: `Record::is_tombstone`, `fold_ancestors`, `tombstone_hash`, `DEFAULT_ANCESTOR_CAP`, `MemStore::new`, `MemStore::with_ancestor_cap`, `Store::get_raw`, `Store::list_raw`, `Store::put_raw`, `Store::purge`, `Store::delete_as`, `plan_put_raw`.
- **Replication writes use `put_raw`, never `put`.** Consumer `put` over a tombstone recreates (`None`) or returns `NotFound` (`Some(_)`), so it can't replicate. `put_raw` never re-stamps. Its planner is `plan_put_raw`:

  | current | expected | plan |
  |---|---|---|
  | `None` | `None` | `Write` (folded) |
  | `None` | `Some(_)` | `NotFound` |
  | `Some(c)` | `Some(c.revision)` | `Write` verbatim, folded against `c` |
  | `Some(c)` | `None` or `Some(other)` | `Conflict { current: c }` (`c` may be a tombstone) |

- `delete` is a provided method calling `delete_as(key, expected, None)`. Store impls implement `delete_as`, not `delete`.

---

## Decisions this plan locks in

1. **`SyncReport` gains `fast_forwarded_to_a` / `fast_forwarded_to_b`.** An ancestor-based overwrite is neither a copy nor a merge. Reporting it as `merged` would claim a body merge that never ran. The CLI `SyncSummary` gains `fast_forwarded` (the sum) and prints it, so `gonzalo sync` doesn't show all zeros after it changed a store.
2. **Two diverged tombstones that converge are reported in `merged`.** A reconciled record was written to both sides, which is what `merged` means. Its doc comment is updated to say so.
3. **Ancestor cap in sync: fold losslessly, and each store truncates on write.** `sync` doesn't know either store's cap. `build_merged` and the tombstone winner fold with `cap = a.ancestors.len() + b.ancestors.len() + 2`, which never truncates. That size is already bounded by `cap_a + cap_b + 2`, because each input list was truncated by its own store. On write, `put_raw` → `plan_put_raw` folds `record.ancestors ∪ {current.revision} ∪ current.ancestors` and truncates to **the destination store's** cap (rows "`None` + `None`" and "`Some(c)` + `Some(c.revision)`"), so the store's cap always wins. Using `DEFAULT_ANCESTOR_CAP` in sync would drop history a store configured above 32 could keep. Using `usize::MAX` is avoided in case `fold_ancestors` pre-allocates by `cap`.
4. **Git pull does bypass `put_raw` / `plan_put_raw`.** `merge_non_ff` writes blobs straight into the index, so it has to truncate itself. `GitStore::pull` passes `self.cap` down to `git_pull` → `merge_non_ff` → `merged_record` / `tombstone_winner`.
5. **Every sync write goes through `put_raw`.** That covers copy, fast-forward, the tombstone winner and the merged record. `overwrite` treats `Conflict` (the key moved, possibly to a tombstone) and `Err(CoreError::NotFound)` (the key was purged after sync's read, `plan_put_raw` "`None` + `Some(_)`") as a lost race: the pass re-loops rather than aborting the sync.
6. **`copy` uses `put_raw(rec, None)`.** If the destination gains a record or a tombstone between sync's `get_raw` and the copy, `plan_put_raw` returns `Conflict` and nothing is written. That sets `raced` and the pass re-loops. Nothing is re-stamped, so a copy can't resurrect a record over a tombstone that arrived mid-pass. The window is closed by construction.
7. **Race re-loop semantics are unchanged.** `MAX_SYNC_PASSES = 16`, and the report is still the *last* pass's report.
8. **Git both-changed `(Some, None)` / `(None, Some)` (edit vs purge) keeps local, as today.** Spec §3.5 doesn't cover it, and `PullConflict` can't carry an absent side. The case is recorded under "Spec gaps" at the end and not changed here.
   - **Superseded by pre-flight Ruling Q1(b).** Spec §3.5 was amended: `(Some, None)` keeps local; `(None, Some)` takes the remote record, with no report entry, matching sync's `(None, rb)` copy row. Before, pull silently dropped a concurrent remote edit that sync would copy back. The test is `nonff_pull_takes_remote_edit_over_local_purge`.
   - **Also ruled at pre-flight.**
     - Q2(a): `merge_non_ff` checks out the merged tree before moving the branch, so remote-only deletions leave the worktree.
     - Q3(a): `git_pull` takes `lock_repo`.

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/gonzalo-core/src/tombstone.rs` | Modify (append) | `reconciled_ancestors`, `tombstone_winner`, and their unit tests |
| `crates/gonzalo-core/src/lib.rs` | Modify | Re-export the two helpers from the crate root |
| `crates/gonzalo-core/src/sync.rs` | Modify | Raw reads, §3.4 decision table, `SyncReport` fields, `copy`/`overwrite` race handling, `build_merged` ancestors; tests and doubles rebuilt on `crate::memstore::MemStore` |
| `crates/gonzalo-cli/src/lib.rs` | Modify `SyncSummary`, `sync_stores`, tests | Surface fast-forwards |
| `crates/gonzalo-cli/src/main.rs` | Modify `Commands::Sync` arm | Print `fast_forwarded:` |
| `crates/gonzalo-store-git/src/lib.rs` | Modify `pull`, `git_pull`, `merge_non_ff`, `merged_record`; add `stage_record` | §3.5 tombstone handling and ancestor folding |
| `crates/gonzalo-store-git/tests/pull.rs` | Modify (append tests + helpers) | §6.3 |
| `crates/gonzalo-core/src/ancestry.rs` | Modify a comment only (if slice 1/2 left it) | Remove the stale ADR 0018 "resurrect" remark |

Other callers checked (`rg -n "sync_with_ancestry|gonzalo_core::sync|SyncReport"`):
- `crates/gonzalo-cli/src/lib.rs:997-1008` `sync_stores` is the only non-core caller. Its test `sync_stores_copies_to_b` (`lib.rs:1280-1305`) still passes, because a one-sided copy is unchanged.
- `crates/gonzalo-cli/tests/cli.rs:110-173` asserts `stdout.contains("copied_to_b: 1")`. The added line doesn't affect it.
- `crates/gonzalo/src/lib.rs:6-10` re-exports `SyncReport`, and nothing constructs it by struct literal. The new fields are additive.
- No test asserts ADR 0018 resurrection through sync.

---

### Task 1: Shared reconciliation helpers in core

**Files:**
- Modify: `crates/gonzalo-core/src/tombstone.rs` (append after `plan_purge`)
- Modify: `crates/gonzalo-core/src/lib.rs` (the `tombstone` re-export slice 1 added)
- Test: `crates/gonzalo-core/src/tombstone.rs` (new `#[cfg(test)] mod reconcile_tests`)

**Interfaces:**
- Consumes (slice 1): `fold_ancestors(stored: &Revision, incoming: &[Revision], current: Option<&Record>, cap: usize) -> Vec<Revision>`, `tombstone_hash() -> ContentHash`, `DEFAULT_ANCESTOR_CAP`, `Record::is_tombstone(&self) -> bool`.
- Produces:
  - `pub fn reconciled_ancestors(stored: &Revision, a: &Record, b: &Record, cap: usize) -> Vec<Revision>`
  - `pub fn tombstone_winner(a: &Record, b: &Record, cap: usize) -> Record`
  - Both re-exported as `gonzalo_core::reconciled_ancestors` and `gonzalo_core::tombstone_winner`.

- [ ] **Step 1: Write the failing tests**

Append to the end of `crates/gonzalo-core/src/tombstone.rs`:

```rust
#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use crate::{Body, ContentHash, Identity, Meta, Record, RecordKey, RecordKind, Revision};
    use std::collections::BTreeMap;

    fn rev(counter: u64, tag: &str) -> Revision {
        Revision {
            counter,
            hash: ContentHash::of(tag.as_bytes()),
        }
    }

    fn record(kind: RecordKind, revision: Revision, ancestors: Vec<Revision>) -> Record {
        Record {
            key: RecordKey::new("ns", "col", "k"),
            kind,
            revision,
            parent: None,
            body: Body::Inline(Vec::new()),
            meta: Meta {
                author: Identity::new("t"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            ancestors,
            deleted_at: None,
        }
    }

    fn tomb(counter: u64, ancestors: Vec<Revision>) -> Record {
        let mut r = record(
            RecordKind::Tombstone,
            Revision {
                counter,
                hash: tombstone_hash(),
            },
            ancestors,
        );
        r.deleted_at = Some(1_000);
        r
    }

    #[test]
    fn reconciled_ancestors_unions_both_revisions_and_both_chains() {
        let base = rev(0, "base");
        let ra = rev(1, "a");
        let rb = rev(1, "b");
        let a = record(RecordKind::Topic, ra.clone(), vec![base.clone()]);
        let b = record(RecordKind::Topic, rb.clone(), vec![base.clone()]);
        let merged = rev(2, "merged");

        let got = reconciled_ancestors(&merged, &a, &b, DEFAULT_ANCESTOR_CAP);

        // Same result as folding the four inputs by hand: same order rule.
        let expected = fold_ancestors(
            &merged,
            &[ra.clone(), base.clone(), rb.clone(), base.clone()],
            None,
            DEFAULT_ANCESTOR_CAP,
        );
        assert_eq!(got, expected);
        assert_eq!(got.len(), 3, "base is deduplicated");
        assert!(got.contains(&ra) && got.contains(&rb) && got.contains(&base));
    }

    #[test]
    fn reconciled_ancestors_excludes_stored_and_truncates_to_cap() {
        let a = record(
            RecordKind::Topic,
            rev(3, "a3"),
            vec![rev(2, "a2"), rev(1, "a1")],
        );
        let b = record(RecordKind::Topic, rev(3, "b3"), vec![rev(2, "b2")]);

        // Stored revision == a's revision (a sync writing a's winner back onto a).
        let got = reconciled_ancestors(&a.revision, &a, &b, 2);

        assert_eq!(got.len(), 2);
        assert!(!got.contains(&a.revision), "own revision is never an ancestor");
        assert_eq!(got[0], b.revision, "newest remaining entry comes first");
    }

    #[test]
    fn tombstone_winner_takes_higher_counter_and_folds_both_chains() {
        let base = rev(0, "base");
        let r1 = rev(1, "r1");
        let low = tomb(1, vec![base.clone()]);
        let high = tomb(2, vec![r1.clone(), base.clone()]);

        let w = tombstone_winner(&low, &high, DEFAULT_ANCESTOR_CAP);

        assert!(w.is_tombstone());
        assert_eq!(w.revision, high.revision);
        assert_eq!(w.deleted_at, high.deleted_at);
        assert_eq!(w.ancestors.len(), 3);
        assert!(w.ancestors.contains(&low.revision));
        assert!(w.ancestors.contains(&r1));
        assert!(w.ancestors.contains(&base));
    }

    #[test]
    fn tombstone_winner_is_symmetric() {
        let base = rev(0, "base");
        let low = tomb(1, vec![base.clone()]);
        let high = tomb(2, vec![rev(1, "r1"), base]);
        assert_eq!(
            tombstone_winner(&low, &high, DEFAULT_ANCESTOR_CAP),
            tombstone_winner(&high, &low, DEFAULT_ANCESTOR_CAP)
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core --lib tombstone::reconcile_tests`
Expected: compile FAIL with `error[E0425]: cannot find function `reconciled_ancestors` in this scope` (and the same for `tombstone_winner`).

- [ ] **Step 3: Implement the helpers**

Append to `crates/gonzalo-core/src/tombstone.rs`, directly above the `#[cfg(test)] mod reconcile_tests` block. `Record` and `Revision` are already imported there for `fold_ancestors`. If the `use` line lacks either, add it.

```rust
/// Ancestors for a record that reconciles two diverged records `a` and `b`: a
/// sync or pull merge result, or a tombstone winner. Both revisions and both
/// ancestor lists are folded (spec §3.4), minus `stored`, sorted by
/// `(counter desc, hash desc)` and truncated to `cap`.
///
/// Sync passes a lossless cap (`a.ancestors.len() + b.ancestors.len() + 2`)
/// and lets each destination store's `plan_put_raw` truncate to its own cap on
/// write. Git pull writes the index directly and passes the store's cap.
pub fn reconciled_ancestors(stored: &Revision, a: &Record, b: &Record, cap: usize) -> Vec<Revision> {
    let mut incoming = Vec::with_capacity(a.ancestors.len() + b.ancestors.len() + 2);
    incoming.push(a.revision.clone());
    incoming.extend(a.ancestors.iter().cloned());
    incoming.push(b.revision.clone());
    incoming.extend(b.ancestors.iter().cloned());
    fold_ancestors(stored, &incoming, None, cap)
}

/// Resolve two diverged tombstones for one key (spec §3.4, §3.5): the one with
/// the higher `(counter, hash)` wins and carries ancestors reconciled from both.
///
/// Symmetric whenever `a.revision != b.revision`. Callers skip equal revisions
/// before reaching this, because independent deletes of the same revision are
/// already in sync. Both arguments must be tombstones.
pub fn tombstone_winner(a: &Record, b: &Record, cap: usize) -> Record {
    debug_assert!(
        a.is_tombstone() && b.is_tombstone(),
        "tombstone_winner needs two tombstones"
    );
    let a_wins = (a.revision.counter, &a.revision.hash) >= (b.revision.counter, &b.revision.hash);
    let mut winner = if a_wins { a.clone() } else { b.clone() };
    winner.ancestors = reconciled_ancestors(&winner.revision, a, b, cap);
    winner
}
```

In `crates/gonzalo-core/src/lib.rs`, find the tombstone re-export slice 1 added (`rg -n "tombstone" crates/gonzalo-core/src/lib.rs`) and add this line directly after it:

```rust
pub use tombstone::{reconciled_ancestors, tombstone_winner};
```

(If slice 1 used `pub use tombstone::*;`, this line is still valid: an explicit re-export of an item the glob also covers is not an error.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core --lib tombstone::reconcile_tests`
Expected: `test result: ok. 4 passed; 0 failed`

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-core/src/tombstone.rs crates/gonzalo-core/src/lib.rs
git commit -m "feat(core): tombstone_winner and reconciled_ancestors helpers (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 2: Tombstone-aware sync (§3.4)

**Files:**
- Modify: `crates/gonzalo-core/src/sync.rs:1-210` (module doc, imports, `SyncConflict`/`SyncReport` docs and fields, `sync_pass`, `copy`, `overwrite`, `build_merged`)
- Modify: `crates/gonzalo-core/src/sync.rs:212-700` (test module: doubles, one existing assertion, new tests)
- Modify: `crates/gonzalo-core/src/ancestry.rs:58-63` (comment only, if still present)

**Interfaces:**
- Consumes: Task 1's `reconciled_ancestors`, `tombstone_winner`. Slice 1's `crate::memstore::MemStore::{new, with_ancestor_cap}`, `Store::{get_raw, list_raw, put_raw, purge, delete_as}` (and the provided `Store::delete`), `Record::is_tombstone`.
- Produces:
  - `SyncReport.fast_forwarded_to_a: Vec<RecordKey>`: keys where A was behind B's chain and was overwritten with B's record (tombstones included).
  - `SyncReport.fast_forwarded_to_b: Vec<RecordKey>`: the same, for B.
  - `SyncReport.merged` now also holds keys where two diverged tombstones converged on the winner.
  - `SyncConflict` now also covers delete vs edit (exactly one side a tombstone).
  - `sync` / `sync_with_ancestry` signatures are unchanged.

- [ ] **Step 1: Rebuild the test doubles on the reference `MemStore` (refactor, no behaviour change)**

In `crates/gonzalo-core/src/sync.rs`, replace the whole top of the test module, from `mod tests {` down to (not including) `fn rec(` (currently `sync.rs:213-411`, or whatever interim version slice 1 left there), with:

```rust
mod tests {
    use super::*;
    use crate::memstore::MemStore;
    use crate::{CoreError, DeleteResult, PutResult, RecordKind, store::Conflict};
    use async_trait::async_trait;
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Mutex;

    /// A store that returns one spurious `Conflict` on the first conditional
    /// (`expected.is_some()`) write per key, like a concurrent writer racing
    /// the first overwrite, then behaves like the reference `MemStore`. Forces
    /// the sync re-loop to retry and still converge. It races both `put` and
    /// `put_raw`, so it behaves the same before and after sync moves to raw
    /// writes.
    struct FlakyOnceStore {
        inner: MemStore,
        tripped: Mutex<HashSet<RecordKey>>,
    }

    impl FlakyOnceStore {
        fn new() -> Self {
            Self {
                inner: MemStore::new(),
                tripped: Mutex::new(HashSet::new()),
            }
        }

        /// `Some(Conflict)` on the first conditional write for this key.
        async fn trip(
            &self,
            record: &Record,
            expected: &Option<Revision>,
        ) -> Result<Option<PutResult>> {
            let trip =
                expected.is_some() && self.tripped.lock().unwrap().insert(record.key.clone());
            if trip && let Some(current) = self.inner.get_raw(&record.key).await? {
                return Ok(Some(PutResult::Conflict(Box::new(Conflict {
                    key: record.key.clone(),
                    expected: expected.clone(),
                    current,
                }))));
            }
            Ok(None)
        }
    }

    #[async_trait]
    impl Store for FlakyOnceStore {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get(key).await
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            if let Some(raced) = self.trip(&record, &expected).await? {
                return Ok(raced);
            }
            self.inner.put(record, expected).await
        }
        async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            if let Some(raced) = self.trip(&record, &expected).await? {
                return Ok(raced);
            }
            self.inner.put_raw(record, expected).await
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

    /// A store whose every write *always* races (a concurrent writer that never
    /// stops), except a create into a raw-absent key, which commits. Used to
    /// prove the re-loop is bounded and terminates. Races both `put` and
    /// `put_raw`.
    struct AlwaysRacyStore {
        inner: MemStore,
    }

    impl AlwaysRacyStore {
        fn new() -> Self {
            Self {
                inner: MemStore::new(),
            }
        }

        /// `None` = let the write through (a create into a raw-absent key);
        /// `Some(Conflict)` otherwise.
        async fn race(
            &self,
            record: &Record,
            expected: &Option<Revision>,
        ) -> Result<Option<PutResult>> {
            match self.inner.get_raw(&record.key).await? {
                None if expected.is_none() => Ok(None),
                None => Err(CoreError::NotFound(record.key.clone())),
                Some(current) => Ok(Some(PutResult::Conflict(Box::new(Conflict {
                    key: record.key.clone(),
                    expected: expected.clone(),
                    current,
                })))),
            }
        }
    }

    #[async_trait]
    impl Store for AlwaysRacyStore {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get(key).await
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            if let Some(raced) = self.race(&record, &expected).await? {
                return Ok(raced);
            }
            self.inner.put(record, expected).await
        }
        async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            if let Some(raced) = self.race(&record, &expected).await? {
                return Ok(raced);
            }
            self.inner.put_raw(record, expected).await
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

    /// A destination that a third peer's tombstone reaches mid-pass. The first
    /// unconditional `put_raw` (sync's copy) for the pending tombstone's key
    /// first commits that tombstone, then runs the copy. This lands exactly in
    /// the window between sync's `get_raw` (which saw the key absent) and its
    /// write.
    struct TombstoneArrivesStore {
        inner: MemStore,
        pending: Mutex<Option<Record>>,
    }

    impl TombstoneArrivesStore {
        fn new(arriving: Record) -> Self {
            Self {
                inner: MemStore::new(),
                pending: Mutex::new(Some(arriving)),
            }
        }
    }

    #[async_trait]
    impl Store for TombstoneArrivesStore {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.inner.get(key).await
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            self.inner.put(record, expected).await
        }
        async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            let arriving = if expected.is_none() {
                let mut pending = self.pending.lock().unwrap();
                match pending.as_ref() {
                    Some(t) if t.key == record.key => pending.take(),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(t) = arriving {
                let _ = self.inner.put_raw(t, None).await?;
            }
            self.inner.put_raw(record, expected).await
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

```

`Identity` reaches the test module through `use super::*` (sync.rs imports it). The `MutexGuard` in `put_raw` is dropped at the end of the `let arriving = ...;` statement, before any `.await`, so the future stays `Send`.

Then, in the rest of the test module:
- replace every `MemStore::default()` with `MemStore::new()`
- replace `FlakyOnceStore::default()` with `FlakyOnceStore::new()`
- replace every `AlwaysRacyStore::default()` with `AlwaysRacyStore::new()`

Use `rg -n "::default\(\)" crates/gonzalo-core/src/sync.rs` to find them. Leave `crate::ancestry::tests::Mem::default()` alone.

The existing `fn rec(...)` helper stays as slice 1 left it. It must build `ancestors: Vec::new(), deleted_at: None`.

Run: `cargo test -p gonzalo-core --lib sync::tests`
Expected: `test result: ok. 10 passed; 0 failed`. The doubles race `put` and `put_raw` alike, so the existing tests pass whether sync still calls `put` or already calls `put_raw`. `TombstoneArrivesStore` is unused until Step 2; a `dead_code` warning here is expected and goes away then.

- [ ] **Step 2: Write the failing tests and update the one changed expectation**

**2a. Changed expectation.** In `re_loops_until_a_racing_store_converges`, the last pass is now a fast-forward. Pass 1 writes the merged record `M` to A (whose ancestors include B's revision) and loses the race on B. Pass 2 then sees B's revision in A's ancestors and overwrites B. Replace the final line `assert_eq!(report.merged, vec![key]);` with:

```rust
        // Pass 1 merged into A and lost the race on B. Pass 2 found B behind
        // A's chain and fast-forwarded it; the report is the last pass's.
        assert_eq!(report.fast_forwarded_to_b, vec![key]);
        assert!(report.merged.is_empty());
```

**2b. New helpers and tests.** Append inside `mod tests`, before its closing `}`:

```rust
    fn k(id: &str) -> RecordKey {
        RecordKey::new("ns", "col", id)
    }

    async fn commit(store: &dyn Store, r: Record, expected: Option<Revision>) -> Revision {
        let PutResult::Committed(rev) = store.put(r, expected).await.unwrap() else {
            panic!("put did not commit");
        };
        rev
    }

    /// An ordinary application update of a live record: next revision, parent =
    /// current, expected = current. The store folds the replaced revision into
    /// `ancestors`.
    async fn edit(store: &dyn Store, id: &str, kind: RecordKind, payload: &str) -> Revision {
        let cur = store
            .get(&k(id))
            .await
            .unwrap()
            .expect("edit needs a live record");
        let mut r = rec(id, kind, payload);
        r.revision = cur.revision.next(payload.as_bytes());
        r.parent = Some(cur.revision.clone());
        commit(store, r, Some(cur.revision)).await
    }

    async fn delete(store: &dyn Store, id: &str, expected: Revision) -> Record {
        assert!(matches!(
            store.delete(&k(id), Some(expected)).await.unwrap(),
            DeleteResult::Deleted
        ));
        let t = store.get_raw(&k(id)).await.unwrap().unwrap();
        assert!(t.is_tombstone());
        t
    }

    // ---- §6.2: tombstones ----

    #[tokio::test]
    async fn stale_peer_takes_the_tombstone() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let tomb = delete(&a, "d", r0).await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.fast_forwarded_to_b, vec![k("d")]);
        assert!(report.conflicts.is_empty() && report.merged.is_empty());
        assert!(report.copied_to_a.is_empty() && report.copied_to_b.is_empty());
        assert!(b.get(&k("d")).await.unwrap().is_none(), "hidden from consumers");
        let tb = b.get_raw(&k("d")).await.unwrap().unwrap();
        assert!(tb.is_tombstone());
        assert_eq!(tb.revision, tomb.revision);
        // Converged: a further sync does nothing.
        assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
    }

    #[tokio::test]
    async fn stale_peer_takes_the_tombstone_in_either_sync_direction() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let tomb = delete(&a, "d", r0).await;

        // Same state, arguments swapped: the deleted store is now side B.
        let report = sync(&b, &a).await.unwrap();

        assert_eq!(report.fast_forwarded_to_a, vec![k("d")]);
        assert!(report.conflicts.is_empty());
        assert!(b.get(&k("d")).await.unwrap().is_none());
        assert_eq!(
            b.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );
        assert!(a.get(&k("d")).await.unwrap().is_none(), "never copied back");
    }

    #[tokio::test]
    async fn recreation_after_delete_propagates_live() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let _ = delete(&a, "d", r0).await;
        let _ = sync(&a, &b).await.unwrap();

        // Recreate with a fresh counter-0 record; the store re-stamps it.
        let r2 = commit(&a, rec("d", RecordKind::Topic, "again\n"), None).await;
        assert_eq!(r2.counter, 2);

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.fast_forwarded_to_b, vec![k("d")]);
        assert!(report.conflicts.is_empty());
        let live = b.get(&k("d")).await.unwrap().expect("recreated record is live on b");
        assert_eq!(live.revision, r2);
        assert_eq!(live.body.bytes(), b"again\n");
    }

    #[tokio::test]
    async fn delete_vs_concurrent_edit_is_a_conflict_and_writes_nothing() {
        // Topic is AppendOnly: if this reached the body merge it would "merge".
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let tomb = delete(&a, "d", r0).await;
        let edited = edit(&b, "d", RecordKind::Topic, "v0\nedit\n").await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].key, k("d"));
        assert!(report.conflicts[0].a.is_tombstone());
        assert!(!report.conflicts[0].b.is_tombstone());
        assert!(report.merged.is_empty());
        assert!(report.fast_forwarded_to_a.is_empty() && report.fast_forwarded_to_b.is_empty());
        assert_eq!(
            a.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );
        assert_eq!(b.get(&k("d")).await.unwrap().unwrap().revision, edited);
    }

    #[tokio::test]
    async fn concurrent_tombstones_converge_on_the_higher_one() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        // A edits then deletes (tombstone counter 2); B deletes r0 (counter 1).
        let r1 = edit(&a, "d", RecordKind::Topic, "v0\nv1\n").await;
        let ta = delete(&a, "d", r1.clone()).await;
        let tb = delete(&b, "d", r0.clone()).await;
        assert_eq!(ta.revision.counter, 2);
        assert_eq!(tb.revision.counter, 1);

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.merged, vec![k("d")]);
        assert!(report.conflicts.is_empty());
        for store in [&a, &b] {
            let got = store.get_raw(&k("d")).await.unwrap().unwrap();
            assert!(got.is_tombstone());
            assert_eq!(got.revision, ta.revision);
            assert!(got.ancestors.contains(&tb.revision));
            assert!(got.ancestors.contains(&r1));
            assert!(got.ancestors.contains(&r0));
            assert!(!got.ancestors.contains(&ta.revision));
        }
        assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
    }

    // ---- §6.2: ancestry-driven ordering for ordinary records ----

    #[tokio::test]
    async fn opaque_fast_forward_no_longer_conflicts() {
        let a = MemStore::new();
        let b = MemStore::new();
        let _ = commit(&a, rec("c", RecordKind::Checkpoint, "c0"), None).await;
        let _ = sync(&a, &b).await.unwrap();

        // A moves ahead; B is simply behind.
        let c1 = edit(&a, "c", RecordKind::Checkpoint, "c1").await;
        let report = sync(&a, &b).await.unwrap();
        assert!(report.conflicts.is_empty(), "behind is not diverged");
        assert_eq!(report.fast_forwarded_to_b, vec![k("c")]);
        assert_eq!(b.get(&k("c")).await.unwrap().unwrap().revision, c1);

        // And the other way round.
        let c2 = edit(&b, "c", RecordKind::Checkpoint, "c2").await;
        let report = sync(&a, &b).await.unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(report.fast_forwarded_to_a, vec![k("c")]);
        assert_eq!(a.get(&k("c")).await.unwrap().unwrap().revision, c2);
    }

    #[tokio::test]
    async fn checkpoint_true_divergence_from_a_shared_parent_still_conflicts() {
        let a = MemStore::new();
        let b = MemStore::new();
        let _ = commit(&a, rec("c", RecordKind::Checkpoint, "c0"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let ca = edit(&a, "c", RecordKind::Checkpoint, "from_a").await;
        let cb = edit(&b, "c", RecordKind::Checkpoint, "from_b").await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.conflicts.len(), 1);
        assert!(report.fast_forwarded_to_a.is_empty() && report.fast_forwarded_to_b.is_empty());
        assert_eq!(a.get(&k("c")).await.unwrap().unwrap().revision, ca);
        assert_eq!(b.get(&k("c")).await.unwrap().unwrap().revision, cb);
    }

    #[tokio::test]
    async fn legacy_record_without_ancestors_takes_the_merge_path() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("l", RecordKind::Topic, "base\n"), None).await;
        // B holds a descendant of r0 written by a pre-0.7 binary: parent set,
        // ancestors empty. Put into an empty store, so nothing is folded in.
        let mut legacy = rec("l", RecordKind::Topic, "base\nmore\n");
        legacy.revision = r0.next(b"base\nmore\n");
        legacy.parent = Some(r0);
        let _ = commit(&b, legacy, None).await;
        assert!(b.get_raw(&k("l")).await.unwrap().unwrap().ancestors.is_empty());

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.merged, vec![k("l")], "no chain: merge, not fast-forward");
        assert!(report.fast_forwarded_to_a.is_empty() && report.fast_forwarded_to_b.is_empty());
        assert_eq!(
            a.get(&k("l")).await.unwrap().unwrap().body.bytes(),
            b"base\nmore\n"
        );
    }

    #[tokio::test]
    async fn structured_merge_folds_both_sides_ancestors() {
        let a = MemStore::new();
        let b = MemStore::new();
        let r0 = commit(&a, rec("t", RecordKind::Topic, "base\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        let ra = edit(&a, "t", RecordKind::Topic, "base\nfrom_a\n").await;
        let rb = edit(&b, "t", RecordKind::Topic, "base\nfrom_b\n").await;

        let report = sync(&a, &b).await.unwrap();

        assert_eq!(report.merged, vec![k("t")]);
        let ma = a.get_raw(&k("t")).await.unwrap().unwrap();
        let mb = b.get_raw(&k("t")).await.unwrap().unwrap();
        assert_eq!(ma.revision, mb.revision);
        assert_eq!(ma.ancestors, mb.ancestors, "byte-identical on both sides");
        assert!(ma.ancestors.contains(&ra));
        assert!(ma.ancestors.contains(&rb));
        assert!(ma.ancestors.contains(&r0));
        assert!(!ma.ancestors.contains(&ma.revision));
    }

    #[tokio::test]
    async fn truncated_chain_never_overwrites() {
        let a = MemStore::new().with_ancestor_cap(2);
        let b = MemStore::new();
        let c0 = commit(&a, rec("c", RecordKind::Checkpoint, "c0"), None).await;
        let _ = sync(&a, &b).await.unwrap();
        // Three edits on a cap-2 store push c0 out of A's chain.
        let _ = edit(&a, "c", RecordKind::Checkpoint, "c1").await;
        let _ = edit(&a, "c", RecordKind::Checkpoint, "c2").await;
        let _ = edit(&a, "c", RecordKind::Checkpoint, "c3").await;
        let ra = a.get_raw(&k("c")).await.unwrap().unwrap();
        assert_eq!(ra.ancestors.len(), 2);
        assert!(!ra.ancestors.contains(&c0));

        let report = sync(&a, &b).await.unwrap();

        // Fails safe: an unknown chain looks diverged (Opaque → conflict).
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.fast_forwarded_to_b.is_empty());
        assert_eq!(b.get(&k("c")).await.unwrap().unwrap().revision, c0);
    }

    #[tokio::test]
    async fn mixed_ancestor_caps_converge() {
        let a = MemStore::new().with_ancestor_cap(4);
        let b = MemStore::new().with_ancestor_cap(32);
        let _ = commit(&a, rec("t", RecordKind::Topic, "0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap();

        // B edits 10 times (chain of 10 fits in 32): B is ahead of A.
        for i in 1..=10 {
            let _ = edit(&b, "t", RecordKind::Topic, &format!("{i}\n")).await;
        }
        let report = sync(&a, &b).await.unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(report.fast_forwarded_to_a, vec![k("t")]);
        assert_eq!(a.get_raw(&k("t")).await.unwrap().unwrap().ancestors.len(), 4);

        // A edits twice: its 4-entry chain still holds B's revision.
        let _ = edit(&a, "t", RecordKind::Topic, "11\n").await;
        let r12 = edit(&a, "t", RecordKind::Topic, "12\n").await;
        let report = sync(&a, &b).await.unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(report.fast_forwarded_to_b, vec![k("t")]);

        let ra = a.get_raw(&k("t")).await.unwrap().unwrap();
        let rb = b.get_raw(&k("t")).await.unwrap().unwrap();
        assert_eq!(ra.revision, r12);
        assert_eq!(rb.revision, r12);
        assert_eq!(ra.ancestors.len(), 4, "A truncates to its own cap");
        assert_eq!(rb.ancestors.len(), 12, "B keeps r0..r11 under its larger cap");
        assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
    }

    // ---- §6.2: racing doubles, tombstone variants ----

    #[tokio::test]
    async fn tombstone_reaches_a_peer_that_races_the_first_overwrite() {
        let a = MemStore::new();
        let b = FlakyOnceStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = sync(&a, &b).await.unwrap(); // copy: unconditional, never trips
        let tomb = delete(&a, "d", r0).await;

        let report = sync(&a, &b).await.unwrap();

        assert!(report.conflicts.is_empty());
        assert_eq!(report.fast_forwarded_to_b, vec![k("d")]);
        let tb = b.get_raw(&k("d")).await.unwrap().unwrap();
        assert!(tb.is_tombstone());
        assert_eq!(tb.revision, tomb.revision);
    }

    #[tokio::test]
    async fn tombstone_sync_against_an_always_racing_peer_terminates() {
        let a = MemStore::new();
        let b = AlwaysRacyStore::new();
        let r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = commit(&b, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let _ = delete(&a, "d", r0.clone()).await;

        // Every overwrite of B races; sync must still return.
        let report = sync(&a, &b).await.unwrap();

        assert!(report.fast_forwarded_to_b.is_empty());
        assert!(report.conflicts.is_empty());
        assert_eq!(b.get(&k("d")).await.unwrap().unwrap().revision, r0);
    }

    // ---- write helpers treat store movement as a race ----

    #[tokio::test]
    async fn overwrite_treats_not_found_and_conflict_as_a_race() {
        let b = MemStore::new();
        // Absent (e.g. purged) key + Some(expected) → plan_put_raw NotFound.
        let stray = rec("x", RecordKind::Topic, "x\n");
        assert!(!overwrite(&b, &stray, &Revision::initial(b"gone")).await.unwrap());
        assert!(b.get_raw(&k("x")).await.unwrap().is_none());

        // Tombstoned key + Some(other) → plan_put_raw Conflict { current: t }.
        let r0 = commit(&b, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let tomb = delete(&b, "d", r0).await;
        let incoming = rec("d", RecordKind::Topic, "v1\n");
        assert!(!overwrite(&b, &incoming, &Revision::initial(b"stale")).await.unwrap());
        assert_eq!(
            b.get_raw(&k("d")).await.unwrap().unwrap().revision,
            tomb.revision
        );
    }

    #[tokio::test]
    async fn copy_over_a_tombstone_conflicts_and_writes_nothing() {
        let b = MemStore::new();
        let r0 = commit(&b, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let tomb = delete(&b, "d", r0).await;
        // As if the tombstone arrived after sync's raw read saw the key absent.
        assert!(!copy(&b, &rec("d", RecordKind::Topic, "v0\n")).await.unwrap());
        let still = b.get_raw(&k("d")).await.unwrap().unwrap();
        assert!(still.is_tombstone(), "put_raw never recreates");
        assert_eq!(still.revision, tomb.revision);
    }

    #[tokio::test]
    async fn tombstone_arriving_mid_copy_is_not_resurrected() {
        // A third peer C deleted r0. A still holds live r0. B is empty when
        // sync reads it, but C's tombstone reaches B between that raw read
        // and sync's copy of A's r0.
        let c = MemStore::new();
        let r0 = commit(&c, rec("d", RecordKind::Topic, "v0\n"), None).await;
        let tomb = delete(&c, "d", r0.clone()).await;

        let a = MemStore::new();
        let a_r0 = commit(&a, rec("d", RecordKind::Topic, "v0\n"), None).await;
        assert_eq!(a_r0, r0);
        let b = TombstoneArrivesStore::new(tomb.clone());

        let report = sync(&a, &b).await.unwrap();

        // Pass 1: the copy conflicts on the arrived tombstone and re-loops.
        // Pass 2: A's r0 is in the tombstone's chain, so A fast-forwards.
        assert!(report.conflicts.is_empty());
        assert!(report.copied_to_b.is_empty());
        assert_eq!(report.fast_forwarded_to_a, vec![k("d")]);
        for store in [&a as &dyn Store, &b as &dyn Store] {
            assert!(store.get(&k("d")).await.unwrap().is_none(), "not resurrected");
            let got = store.get_raw(&k("d")).await.unwrap().unwrap();
            assert!(got.is_tombstone());
            assert_eq!(got.revision, tomb.revision);
        }
        assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core --lib sync::tests`
Expected: compile FAIL with `error[E0609]: no field `fast_forwarded_to_b` on type `SyncReport`` (and `fast_forwarded_to_a`).

- [ ] **Step 4: Implement the §3.4 decision table**

Replace `crates/gonzalo-core/src/sync.rs` from line 1 through the end of `build_merged` (currently `sync.rs:1-210`, i.e. everything above `#[cfg(test)]`) with:

```rust
//! Reconcile two `Store`s. Any store can be a sync peer.
//!
//! Sync reads through the replication surface (`list_raw` / `get_raw`), so it
//! sees tombstones. For a key held by both sides it uses each record's bounded
//! `ancestors` list (spec §3.4) to tell "one side is behind" (overwrite it)
//! from "the sides diverged". Diverged tombstones converge on the higher
//! revision. A tombstone against a live edit is a conflict. Two diverged live
//! records take the class-aware merge: append-only kinds union, and
//! structured/opaque divergences surface as conflicts.
//! [`sync_with_ancestry`] 3-way-merges structured bodies against their real
//! common ancestor when an [`AncestryStore`](crate::AncestryStore) retains it;
//! [`sync`] uses an empty base (ADR 0016).

use crate::tombstone::{reconciled_ancestors, tombstone_winner};
use crate::{
    BlobStore, Body, CoreError, Identity, KeyPrefix, MergeOutcome, Meta, PutResult, Record,
    RecordKey, Result, Revision, Store, merge,
};
use std::collections::BTreeSet;

/// A divergence that could not be auto-merged and needs caller/CLI resolution:
/// an unmergeable body divergence, or a delete (one side a tombstone) against a
/// concurrent edit. Neither side is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncConflict {
    pub key: RecordKey,
    pub a: Box<Record>,
    pub b: Box<Record>,
}

/// What a sync run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "a SyncReport may contain unresolved conflicts that must be handled"]
pub struct SyncReport {
    /// Keys copied into store A (were only in B). Includes tombstones.
    pub copied_to_a: Vec<RecordKey>,
    /// Keys copied into store B (were only in A). Includes tombstones.
    pub copied_to_b: Vec<RecordKey>,
    /// Keys where A was behind B's revision chain and was overwritten with B's
    /// record. Includes tombstones.
    pub fast_forwarded_to_a: Vec<RecordKey>,
    /// Keys where B was behind A's revision chain and was overwritten with A's
    /// record. Includes tombstones.
    pub fast_forwarded_to_b: Vec<RecordKey>,
    /// Keys whose divergence was reconciled and written to both stores: an
    /// auto-merged live record, or two diverged tombstones converged on the
    /// winner.
    pub merged: Vec<RecordKey>,
    /// Divergences needing manual resolution.
    pub conflicts: Vec<SyncConflict>,
}

/// Upper bound on sync passes before giving up on a non-quiescent pair.
///
/// Each pass re-reads both stores, so a store that settles converges within
/// one extra pass. The cap only bites when writers never stop racing the merge
/// window (livelock guard): rather than spin forever, sync returns the last
/// pass's best-effort report.
const MAX_SYNC_PASSES: usize = 16;

/// Reconcile stores `a` and `b`. After a clean run (no `conflicts`), both
/// stores hold the same record (live or tombstone) for every key.
///
/// Stores need not be quiescent. A single pass can lose a write that lands in
/// the read→merge→write window (the OCC `put_raw` returns `Conflict`, or
/// `NotFound` when the key was purged meanwhile); sync re-runs the pass until one
/// completes without any such race (a fixpoint), bounded by
/// [`MAX_SYNC_PASSES`] so continuous concurrent writes can't livelock it.
pub async fn sync(a: &dyn Store, b: &dyn Store) -> Result<SyncReport> {
    sync_with_ancestry(a, b, None).await
}

/// As [`sync`], but 3-way-merges divergent `Structured` bodies against their
/// real common ancestor when one is available. `ancestry` is a content-addressed
/// store of past bodies keyed by revision hash (see
/// [`AncestryStore`](crate::AncestryStore)): when two records diverge from a
/// shared parent revision whose body it holds, that body is the merge base;
/// otherwise sync falls back to the empty base (ADR 0016).
pub async fn sync_with_ancestry(
    a: &dyn Store,
    b: &dyn Store,
    ancestry: Option<&dyn BlobStore>,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    for _ in 0..MAX_SYNC_PASSES {
        let (pass, raced) = sync_pass(a, b, ancestry).await?;
        report = pass;
        if !raced {
            break; // quiescent: this pass landed cleanly, stores have converged.
        }
    }
    Ok(report)
}

/// The merge base for a divergence: the body of `a`/`b`'s shared parent revision
/// when `ancestry` retains it, else an empty base (the base-agnostic fallback,
/// correct for `AppendOnly` and safe for the rest).
async fn ancestry_base(ancestry: Option<&dyn BlobStore>, rec_a: &Record, rec_b: &Record) -> Body {
    if let Some(anc) = ancestry
        && let (Some(pa), Some(pb)) = (&rec_a.parent, &rec_b.parent)
        && pa == pb
        && let Ok(Some(bytes)) = anc.get_blob(&pa.hash).await
    {
        return Body::Inline(bytes);
    }
    Body::Inline(Vec::new())
}

/// How two present records for the same key relate by revision chain (§3.4).
#[derive(Debug, PartialEq, Eq)]
enum Relation {
    /// Equal revisions.
    InSync,
    /// B's revision is in A's ancestors: B is behind.
    AAhead,
    /// A's revision is in B's ancestors: A is behind.
    BAhead,
    /// Neither chain contains the other's revision: diverged, or the chain is
    /// unknown (legacy empty list, or truncated past the cap). Fails safe.
    Diverged,
}

fn relate(a: &Record, b: &Record) -> Relation {
    if a.revision == b.revision {
        Relation::InSync
    } else if a.ancestors.contains(&b.revision) {
        Relation::AAhead
    } else if b.ancestors.contains(&a.revision) {
        Relation::BAhead
    } else {
        Relation::Diverged
    }
}

/// The cap sync folds with: large enough never to truncate. Each input list is
/// already bounded by its own store's cap, and each destination store's
/// `plan_put_raw` truncates to that store's cap on write, so the store's cap
/// wins.
fn lossless_cap(a: &Record, b: &Record) -> usize {
    a.ancestors.len() + b.ancestors.len() + 2
}

/// One reconciliation pass over the union of raw keys. Returns the pass's
/// report and whether any write lost a race (`true` ⇒ a store changed mid-pass,
/// so the caller should re-loop). A `SyncConflict` is a terminal divergence
/// (surfaced in the report), not a race, and does not trigger a re-loop.
async fn sync_pass(
    a: &dyn Store,
    b: &dyn Store,
    ancestry: Option<&dyn BlobStore>,
) -> Result<(SyncReport, bool)> {
    let mut report = SyncReport::default();
    let mut raced = false;

    // Raw reads only: consumer reads hide tombstones, and a tombstone that sync
    // cannot see is copied over by the peer's live record (resurrection).
    let mut keys: BTreeSet<RecordKey> = BTreeSet::new();
    keys.extend(a.list_raw(&KeyPrefix::default()).await?);
    keys.extend(b.list_raw(&KeyPrefix::default()).await?);

    for key in keys {
        let ra = a.get_raw(&key).await?;
        let rb = b.get_raw(&key).await?;
        match (ra, rb) {
            (Some(rec), None) => {
                if copy(b, &rec).await? {
                    report.copied_to_b.push(key);
                } else {
                    raced = true;
                }
            }
            (None, Some(rec)) => {
                if copy(a, &rec).await? {
                    report.copied_to_a.push(key);
                } else {
                    raced = true;
                }
            }
            (Some(rec_a), Some(rec_b)) => match relate(&rec_a, &rec_b) {
                Relation::InSync => {}
                Relation::AAhead => {
                    if overwrite(b, &rec_a, &rec_b.revision).await? {
                        report.fast_forwarded_to_b.push(key);
                    } else {
                        raced = true;
                    }
                }
                Relation::BAhead => {
                    if overwrite(a, &rec_b, &rec_a.revision).await? {
                        report.fast_forwarded_to_a.push(key);
                    } else {
                        raced = true;
                    }
                }
                Relation::Diverged => match (rec_a.is_tombstone(), rec_b.is_tombstone()) {
                    (true, true) => {
                        let winner = tombstone_winner(&rec_a, &rec_b, lossless_cap(&rec_a, &rec_b));
                        // Written to both sides, including the one that already
                        // holds the winning revision, so both carry the folded
                        // chain. `fold_ancestors` excludes the stored revision.
                        let la = overwrite(a, &winner, &rec_a.revision).await?;
                        let lb = overwrite(b, &winner, &rec_b.revision).await?;
                        if la && lb {
                            report.merged.push(key);
                        } else {
                            raced = true;
                        }
                    }
                    (true, false) | (false, true) => {
                        // Delete vs concurrent edit: no side wins without
                        // losing someone's intent (spec §5.4).
                        report.conflicts.push(SyncConflict {
                            key,
                            a: Box::new(rec_a),
                            b: Box::new(rec_b),
                        });
                    }
                    (false, false) => {
                        let base = ancestry_base(ancestry, &rec_a, &rec_b).await;
                        match merge(rec_a.kind.merge_class(), &base, &rec_a.body, &rec_b.body) {
                            MergeOutcome::Merged(body) => {
                                let merged = build_merged(&key, &rec_a, &rec_b, body);
                                let la = overwrite(a, &merged, &rec_a.revision).await?;
                                let lb = overwrite(b, &merged, &rec_b.revision).await?;
                                if la && lb {
                                    report.merged.push(key);
                                } else {
                                    // At least one side raced; re-loop to
                                    // reconcile the store that moved against
                                    // the now-merged peer.
                                    raced = true;
                                }
                            }
                            MergeOutcome::NeedsResolution => {
                                report.conflicts.push(SyncConflict {
                                    key,
                                    a: Box::new(rec_a),
                                    b: Box::new(rec_b),
                                });
                            }
                        }
                    }
                },
            },
            (None, None) => {}
        }
    }
    Ok((report, raced))
}

/// Create `rec` in `dst`, where the raw read found the key absent, through the
/// replication write. Returns `false` if `dst` gained a record or a tombstone
/// since that read (`put_raw` → `Conflict`, nothing written), signalling the
/// caller to re-loop. `put_raw` never re-stamps, so a copy can't recreate over
/// a tombstone that arrived mid-pass.
async fn copy(dst: &dyn Store, rec: &Record) -> Result<bool> {
    Ok(matches!(
        dst.put_raw(rec.clone(), None).await?,
        PutResult::Committed(_)
    ))
}

/// Conditionally overwrite `dst` with `rec` (stored verbatim), expecting
/// revision `expected`, through the replication write. Returns `false` if a
/// concurrent mutation raced the write window: `Conflict` (the key moved, maybe
/// to a tombstone), or `NotFound` (the key was purged after sync's read).
async fn overwrite(dst: &dyn Store, rec: &Record, expected: &Revision) -> Result<bool> {
    match dst.put_raw(rec.clone(), Some(expected.clone())).await {
        Ok(PutResult::Committed(_)) => Ok(true),
        Ok(PutResult::Conflict(_)) => Ok(false),
        Err(CoreError::NotFound(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

fn build_merged(key: &RecordKey, a: &Record, b: &Record, body: Body) -> Record {
    let revision = Revision {
        counter: a.revision.counter.max(b.revision.counter) + 1,
        hash: crate::ContentHash::of(body.bytes()),
    };
    let ancestors = reconciled_ancestors(&revision, a, b, lossless_cap(a, b));
    let mut labels = a.meta.labels.clone();
    labels.extend(b.meta.labels.clone());
    let mut links = a.links.clone();
    for l in &b.links {
        if !links.contains(l) {
            links.push(l.clone());
        }
    }
    Record {
        key: key.clone(),
        kind: a.kind,
        revision,
        parent: Some(if a.revision.counter >= b.revision.counter {
            a.revision.clone()
        } else {
            b.revision.clone()
        }),
        body,
        meta: Meta {
            author: Identity::new("gonzalo-sync"),
            origin_system: "sync".into(),
            created: a.meta.created.min(b.meta.created),
            updated: a.meta.updated.max(b.meta.updated),
            labels,
        },
        links,
        ancestors,
        deleted_at: None,
    }
}
```

If `crates/gonzalo-core/src/ancestry.rs` still carries the comment `// sync from a peer may resurrect the record (ADR 0018).` in `delete` (currently `ancestry.rs:59-61`), replace those three comment lines with:

```rust
        // Delete writes a tombstone through the inner store and leaves ancestry
        // blobs untouched: retained bodies stay available for a later
        // divergence's 3-way merge (ADR 0016).
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core --lib sync::tests`
Expected: `test result: ok. 26 passed; 0 failed` (10 existing plus 16 new; the exact count may differ by any tests slice 1 added here, but the result must be 0 failed).

Run: `cargo test -p gonzalo-core --all-features`
Expected: every test binary reports `0 failed`.

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-core/src/sync.rs crates/gonzalo-core/src/ancestry.rs
git commit -m "feat(core): tombstone-aware sync with ancestor fast-forward (#203)

sync reads through list_raw/get_raw and applies the spec 3.4 decision table:
fast-forward when one side's revision is in the other's ancestors, converge
diverged tombstones on the higher revision, surface delete-vs-edit as a
SyncConflict, and fold both chains into merged records. SyncReport gains
fast_forwarded_to_a / fast_forwarded_to_b.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 3: CLI sync summary reports fast-forwards

**Files:**
- Modify: `crates/gonzalo-cli/src/lib.rs:988-1008` (`SyncSummary`, `sync_stores`)
- Modify: `crates/gonzalo-cli/src/main.rs:361-367` (`Commands::Sync` arm)
- Test: `crates/gonzalo-cli/src/lib.rs` test module (after `sync_stores_copies_to_b`, currently line 1305)

**Interfaces:**
- Consumes: Task 2's `SyncReport.fast_forwarded_to_a`, `SyncReport.fast_forwarded_to_b`.
- Produces: `SyncSummary.fast_forwarded: usize` (sum of both directions); stdout line `fast_forwarded: N`.

- [ ] **Step 1: Write the failing test**

Insert after `sync_stores_copies_to_b` in the `crates/gonzalo-cli/src/lib.rs` test module:

```rust
    #[tokio::test]
    async fn sync_stores_fast_forwards_a_peer_that_is_behind() {
        use gonzalo_core::{Body, KeyPrefix, PutResult, Store};

        let store_a = TempDir::new().unwrap();
        let store_b = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        write_file(src.path(), "note.md", "synced content");
        migrate(
            store_a.path(),
            src.path(),
            "testns",
            "testcol",
            RecordKind::Topic,
        )
        .await
        .unwrap();
        let _ = sync_stores(store_a.path(), store_b.path()).await.unwrap();

        // A edits the record; B is now simply behind.
        let fs_a = FsStore::new(store_a.path());
        let key = fs_a.list(&KeyPrefix::default()).await.unwrap().remove(0);
        let cur = fs_a.get(&key).await.unwrap().unwrap();
        let mut next = cur.clone();
        next.body = Body::Inline(b"synced content\nmore\n".to_vec());
        next.revision = cur.revision.next(next.body.bytes());
        next.parent = Some(cur.revision.clone());
        next.ancestors = Vec::new();
        assert!(matches!(
            fs_a.put(next, Some(cur.revision)).await.unwrap(),
            PutResult::Committed(_)
        ));

        let summary = sync_stores(store_a.path(), store_b.path()).await.unwrap();
        assert_eq!(summary.fast_forwarded, 1);
        assert_eq!(summary.merged, 0);
        assert_eq!(summary.conflicts, 0);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p gonzalo-cli --lib sync_stores_fast_forwards_a_peer_that_is_behind`
Expected: compile FAIL with `error[E0609]: no field `fast_forwarded` on type `SyncSummary``.

- [ ] **Step 3: Implement**

Replace `crates/gonzalo-cli/src/lib.rs:988-1008` (`SyncSummary` and `sync_stores`) with:

```rust
/// Summary returned by [`sync_stores`].
pub struct SyncSummary {
    pub copied_to_a: usize,
    pub copied_to_b: usize,
    /// Keys where one store was behind the other's revision chain and was
    /// overwritten (both directions, tombstones included).
    pub fast_forwarded: usize,
    pub merged: usize,
    pub conflicts: usize,
}

/// Sync two filesystem stores via [`gonzalo_core::sync`].
pub async fn sync_stores(a: &Path, b: &Path) -> Result<SyncSummary> {
    let store_a = FsStore::new(a);
    let store_b = FsStore::new(b);
    let report = gonzalo_core::sync(&store_a, &store_b).await?;
    Ok(SyncSummary {
        copied_to_a: report.copied_to_a.len(),
        copied_to_b: report.copied_to_b.len(),
        fast_forwarded: report.fast_forwarded_to_a.len() + report.fast_forwarded_to_b.len(),
        merged: report.merged.len(),
        conflicts: report.conflicts.len(),
    })
}
```

(If slice 2 changed `FsStore::new(a)` to a fallible or builder form, keep slice 2's construction lines and change only the struct and the `Ok(SyncSummary { .. })` literal.)

Replace the `Commands::Sync` arm in `crates/gonzalo-cli/src/main.rs` (currently `main.rs:361-367`) with the block below. Don't realign the existing labels: `crates/gonzalo-cli/tests/cli.rs:147` asserts `stdout.contains("copied_to_b: 1")`, with exactly one space.

```rust
        Commands::Sync { a, b } => {
            let summary = sync_stores(&a, &b).await?;
            println!("copied_to_a: {}", summary.copied_to_a);
            println!("copied_to_b: {}", summary.copied_to_b);
            println!("fast_forwarded: {}", summary.fast_forwarded);
            println!("merged:      {}", summary.merged);
            println!("conflicts:   {}", summary.conflicts);
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-cli --lib sync_stores`
Expected: `test result: ok. 2 passed; 0 failed`

Run: `cargo test -p gonzalo-cli --test cli sync_`
Expected: `test result: ok. 2 passed; 0 failed`

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-cli/src/lib.rs crates/gonzalo-cli/src/main.rs
git commit -m "feat(cli): report fast-forwarded keys from gonzalo sync (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 4: Tombstone-aware non-fast-forward git pull (§3.5)

**Files:**
- Modify: `crates/gonzalo-store-git/src/lib.rs:12-15` (imports)
- Modify: `crates/gonzalo-store-git/src/lib.rs:150-155` (`GitStore::pull`)
- Modify: `crates/gonzalo-store-git/src/lib.rs:191-220` (`git_pull` signature and call)
- Modify: `crates/gonzalo-store-git/src/lib.rs:226-341` (`merge_non_ff`)
- Modify: `crates/gonzalo-store-git/src/lib.rs:430-465` (`merged_record`), and add `stage_record`
- Test: `crates/gonzalo-store-git/tests/pull.rs` (append)

Line numbers are from before slice 2. Slice 2 adds `cap` and tombstone-aware `put`/`delete`/`purge` to the same file, so locate each function by name.

**Interfaces:**
- Consumes: Task 1's `gonzalo_core::{reconciled_ancestors, tombstone_winner}`. Slice 2's `GitStore { root: PathBuf, cap: usize }`, `GitStore::{delete_as, get_raw, list_raw, purge}` (tombstone-writing; tests call the provided `Store::delete`), `Record::is_tombstone`. `merge_non_ff` still writes the git index directly rather than calling `put_raw`, which is why it applies `cap` itself.
- Produces: no public API change. `PullReport` is unchanged. `merged` also holds keys where two diverged tombstones converged, and `conflicts` also holds delete-vs-edit keys (`local` is kept).

- [ ] **Step 1: Write the failing tests**

In `crates/gonzalo-store-git/tests/pull.rs`, change the `use gonzalo_core::{...}` block (`pull.rs:7-9`) to:

```rust
use gonzalo_core::{
    Body, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
    Revision, Store,
};
```

The `record()` helper (`pull.rs:12-34`) already builds `ancestors: Vec::new(), deleted_at: None` since slice 1. Append at the end of the file:

```rust
/// Delete `id` expecting `expected`, and return the tombstone it wrote.
async fn tombstone(store: &GitStore, id: &str, expected: Revision) -> Record {
    assert!(matches!(
        store.delete(&key(id), Some(expected)).await.unwrap(),
        DeleteResult::Deleted
    ));
    let t = store.get_raw(&key(id)).await.unwrap().unwrap();
    assert!(t.is_tombstone());
    t
}

/// Commit an unrelated new record `n`. Used to make a side diverge without
/// touching `m`.
async fn add_n(store: &GitStore) {
    commit(
        store,
        record(
            "n",
            RecordKind::MemoryTier,
            r#"{"added":true}"#,
            Revision::initial(br#"{"added":true}"#),
            None,
        ),
        None,
    )
    .await;
}

#[tokio::test]
async fn pull_fast_forwards_tombstone() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let t = tombstone(&remote, "m", base_rev).await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(report.fast_forwarded);
    assert!(local.get(&key("m")).await.unwrap().is_none());
    assert_eq!(
        local.get_raw(&key("m")).await.unwrap().unwrap().revision,
        t.revision
    );
    assert!(!local.list(&KeyPrefix::default()).await.unwrap().contains(&key("m")));
}

#[tokio::test]
async fn nonff_pull_applies_remote_tombstone() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let t = tombstone(&remote, "m", base_rev).await;
    add_n(&local).await; // diverge without touching `m`

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(!report.fast_forwarded);
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    assert!(local.get(&key("m")).await.unwrap().is_none());
    assert_eq!(
        local.get_raw(&key("m")).await.unwrap().unwrap().revision,
        t.revision
    );
    assert!(local.get(&key("n")).await.unwrap().is_some());
}

#[tokio::test]
async fn nonff_pull_keeps_local_tombstone() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let t = tombstone(&local, "m", base_rev).await;
    add_n(&remote).await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(!report.fast_forwarded);
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    assert!(local.get(&key("m")).await.unwrap().is_none());
    assert_eq!(
        local.get_raw(&key("m")).await.unwrap().unwrap().revision,
        t.revision
    );
    assert!(local.get(&key("n")).await.unwrap().is_some());
}

#[tokio::test]
async fn nonff_pull_surfaces_delete_vs_edit_conflict() {
    // Topic is AppendOnly, and local is the live side, so without the
    // tombstone check `merge(local.kind.merge_class(), ..)` would "merge"
    // away the remote delete.
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let _ = tombstone(&remote, "m", base_rev.clone()).await;
    commit(
        &local,
        record(
            "m",
            RecordKind::Topic,
            "base\nlocal\n",
            base_rev.next(b"base\nlocal\n"),
            Some(base_rev.clone()),
        ),
        Some(base_rev),
    )
    .await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].key, key("m"));
    assert!(report.conflicts[0].remote.is_tombstone());
    assert!(!report.conflicts[0].local.is_tombstone());
    assert!(report.merged.is_empty());
    // Local is kept.
    assert_eq!(
        local.get(&key("m")).await.unwrap().unwrap().body.bytes(),
        b"base\nlocal\n"
    );
}

#[tokio::test]
async fn nonff_pull_converges_concurrent_tombstones() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    // Remote edits then deletes (tombstone counter 2); local deletes base (1).
    let r1 = base_rev.next(b"base\nr1\n");
    commit(
        &remote,
        record(
            "m",
            RecordKind::Topic,
            "base\nr1\n",
            r1.clone(),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    let tr = tombstone(&remote, "m", r1.clone()).await;
    let tl = tombstone(&local, "m", base_rev.clone()).await;
    assert_eq!(tr.revision.counter, 2);
    assert_eq!(tl.revision.counter, 1);

    let report = local.pull("origin", &branch).await.unwrap();

    assert_eq!(report.merged, vec![key("m")]);
    assert!(report.conflicts.is_empty());
    let got = local.get_raw(&key("m")).await.unwrap().unwrap();
    assert!(got.is_tombstone());
    assert_eq!(got.revision, tr.revision);
    assert!(got.ancestors.contains(&tl.revision));
    assert!(got.ancestors.contains(&r1));
    assert!(got.ancestors.contains(&base_rev));
    assert!(!got.ancestors.contains(&tr.revision));
}

#[tokio::test]
async fn nonff_pull_identical_tombstones_are_a_noop() {
    // Both sides delete the same revision: equal revisions, but the files
    // differ (deleted_at), so git sees a both-sided change.
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let tr = tombstone(&remote, "m", base_rev.clone()).await;
    let tl = tombstone(&local, "m", base_rev).await;
    assert_eq!(tr.revision, tl.revision);

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    assert_eq!(
        local.get_raw(&key("m")).await.unwrap().unwrap().revision,
        tl.revision
    );
}

#[tokio::test]
async fn nonff_pull_applies_remote_purge() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::Topic, "base\n").await;
    let t = tombstone(&remote, "m", base_rev).await;
    assert!(matches!(
        remote.purge(&key("m"), t.revision).await.unwrap(),
        DeleteResult::Deleted
    ));
    add_n(&local).await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert!(!report.fast_forwarded);
    assert!(report.merged.is_empty() && report.conflicts.is_empty());
    assert!(local.get_raw(&key("m")).await.unwrap().is_none());
    assert!(!local.list_raw(&KeyPrefix::default()).await.unwrap().contains(&key("m")));
    assert!(local.get(&key("n")).await.unwrap().is_some());
}

#[tokio::test]
async fn nonff_pull_merged_record_folds_both_ancestries() {
    let (_r, _l, remote, local, _p, branch, base_rev) =
        cloned_base(RecordKind::MemoryTier, r#"{"name":"a","content":"x"}"#).await;
    let remote_rev = base_rev.next(b"remote");
    let local_rev = base_rev.next(b"local");
    commit(
        &remote,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"name":"a","content":"y"}"#,
            remote_rev.clone(),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;
    commit(
        &local,
        record(
            "m",
            RecordKind::MemoryTier,
            r#"{"name":"b","content":"x"}"#,
            local_rev.clone(),
            Some(base_rev.clone()),
        ),
        Some(base_rev.clone()),
    )
    .await;

    let report = local.pull("origin", &branch).await.unwrap();

    assert_eq!(report.merged, vec![key("m")]);
    let merged = local.get_raw(&key("m")).await.unwrap().unwrap();
    assert!(merged.ancestors.contains(&local_rev));
    assert!(merged.ancestors.contains(&remote_rev));
    assert!(merged.ancestors.contains(&base_rev));
    assert!(!merged.ancestors.contains(&merged.revision));
}
```

- [ ] **Step 2: Run the tests to verify the right ones fail**

Run: `cargo test -p gonzalo-store-git --test pull`
Expected: FAIL on exactly these tests:
- `nonff_pull_surfaces_delete_vs_edit_conflict`: `assertion `left == right` failed` (`conflicts.len()` is 0, because the old path AppendOnly-merged it)
- `nonff_pull_converges_concurrent_tombstones`: `report.merged` is `[]` (equal empty bodies take the `_ => {}` arm)
- `nonff_pull_merged_record_folds_both_ancestries`: the `contains(&local_rev)` assertion fails (merged record has empty `ancestors`)

The other new tests (`pull_fast_forwards_tombstone`, `nonff_pull_applies_remote_tombstone`, `nonff_pull_keeps_local_tombstone`, `nonff_pull_identical_tombstones_are_a_noop`, `nonff_pull_applies_remote_purge`) already PASS on slice 2. They pin §3.5's "unchanged" rows and must stay green. All 5 pre-existing tests pass.

- [ ] **Step 3: Implement**

**3a. Imports.** In the `use gonzalo_core::{...}` block at the top of `crates/gonzalo-store-git/src/lib.rs`, add `reconciled_ancestors` and `tombstone_winner`. Keep whatever slice 2 added (e.g. planner names). The result must include at least:

```rust
use gonzalo_core::{
    Body, ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, MergeOutcome, Meta, PutResult,
    Record, RecordKey, Result, Revision, decode_segment, merge, reconciled_ancestors,
    record_components, store::Conflict, tombstone_winner,
};
```

**3b. Thread the store's cap into pull.** Replace `GitStore::pull` with:

```rust
    /// Pull `branch` from `remote` (typically "origin"). A fast-forward advances
    /// the branch; a divergence is reconciled by a content-aware 3-way merge
    /// (gonzalo `merge()` per record, ADR 0017), with tombstones decided by
    /// kind and revision first (spec §3.5), and unresolved records kept local
    /// and reported in the [`PullReport`].
    pub async fn pull(&self, remote: &str, branch: &str) -> Result<PullReport> {
        let root = self.root.clone();
        let remote = remote.to_string();
        let branch = branch.to_string();
        let cap = self.cap;
        run_blocking(move || git_pull(&root, &remote, &branch, cap)).await
    }
```

In `git_pull`, change the signature and the last line:

```rust
fn git_pull(root: &Path, remote: &str, branch: &str, cap: usize) -> Result<PullReport> {
```

```rust
    merge_non_ff(&repo, remote, branch, fetch_commit.id(), cap)
```

(the fast-forward and up-to-date branches of `git_pull` are unchanged.)

**3c. `merge_non_ff`.** Replace the whole function (doc comment through closing brace) with:

```rust
/// Reconcile a diverged local branch with `remote_oid` by a content-aware 3-way
/// merge, recorded in a two-parent merge commit (ADR 0017).
///
/// A path changed only on the remote takes the remote side verbatim. That
/// includes a remote tombstone (a modified file) and a remote purge (a git
/// deletion). A record changed on both sides is decided by kind before any body
/// merge (spec §3.5): equal revisions are a no-op; two tombstones converge on
/// the higher `(counter, hash)`; exactly one tombstone is a `PullConflict` that
/// keeps local; two live records take gonzalo's class-aware `merge()`. Records
/// written into the index bypass `put_raw`, so ancestors are truncated here to
/// this store's `cap`.
fn merge_non_ff(
    repo: &git2::Repository,
    remote: &str,
    branch: &str,
    remote_oid: git2::Oid,
    cap: usize,
) -> Result<PullReport> {
    let local_oid = repo
        .head()
        .map_err(be)?
        .target()
        .ok_or_else(|| CoreError::Backend("local HEAD is unborn".into()))?;
    let local_commit = repo.find_commit(local_oid).map_err(be)?;
    let remote_commit = repo.find_commit(remote_oid).map_err(be)?;
    let local_tree = local_commit.tree().map_err(be)?;
    let remote_tree = remote_commit.tree().map_err(be)?;
    // The merge base is the true common ancestor (git retains history); an
    // unrelated history has no base, so treat every overlap as add/add.
    let base_tree = match repo.merge_base(local_oid, remote_oid) {
        Ok(base_oid) => Some(repo.find_commit(base_oid).map_err(be)?.tree().map_err(be)?),
        Err(_) => None,
    };

    // Start the merged index from local, then fold in the remote-side changes.
    let mut index = repo.index().map_err(be)?;
    index.read_tree(&local_tree).map_err(be)?;

    let local_changed = changed_paths_set(repo, base_tree.as_ref(), &local_tree)?;
    let remote_diff = repo
        .diff_tree_to_tree(base_tree.as_ref(), Some(&remote_tree), None)
        .map_err(be)?;

    let mut report = PullReport::default();
    for delta in remote_diff.deltas() {
        let Some(path) = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(Path::to_path_buf)
        else {
            continue;
        };

        if !local_changed.contains(&path) {
            // Changed only on the remote: apply the remote side verbatim. A
            // remote tombstone is a modified file; a remote purge is a deletion.
            if delta.status() == git2::Delta::Deleted {
                index.remove_path(&path).map_err(be)?;
            } else if let Some(bytes) = tree_blob(repo, &remote_tree, &path)? {
                index
                    .add_frombuffer(&blob_entry(&path), &bytes)
                    .map_err(be)?;
            }
            continue;
        }

        // Changed on both sides.
        let Some(key) = key_from_path(&path) else {
            continue; // non-record file (should not occur in a record store)
        };
        let local_rec = record_at(repo, Some(&local_tree), &path)?;
        let remote_rec = record_at(repo, Some(&remote_tree), &path)?;
        match (local_rec, remote_rec) {
            // Same revision, e.g. two independent deletes of one revision whose
            // files differ only in `deleted_at`: already in sync, keep local.
            (Some(local), Some(remote)) if local.revision == remote.revision => {}
            // Two diverged tombstones: the higher (counter, hash) wins, carrying
            // both chains. Checked by kind, because two tombstones always have
            // equal (empty) bodies and the body guard below would skip them.
            (Some(local), Some(remote)) if local.is_tombstone() && remote.is_tombstone() => {
                let winner = tombstone_winner(&local, &remote, cap);
                stage_record(&mut index, &path, &winner)?;
                report.merged.push(key);
            }
            // Delete vs edit: keep local (already staged) and surface both,
            // the same policy as an unmergeable body.
            (Some(local), Some(remote)) if local.is_tombstone() || remote.is_tombstone() => {
                report.conflicts.push(PullConflict {
                    key,
                    local: Box::new(local),
                    remote: Box::new(remote),
                });
            }
            (Some(local), Some(remote)) if local.body != remote.body => {
                let base_body = record_at(repo, base_tree.as_ref(), &path)?
                    .map(|r| r.body)
                    .unwrap_or(Body::Inline(Vec::new()));
                match merge(
                    local.kind.merge_class(),
                    &base_body,
                    &local.body,
                    &remote.body,
                ) {
                    MergeOutcome::Merged(body) => {
                        let merged = merged_record(&key, &local, &remote, body, cap);
                        stage_record(&mut index, &path, &merged)?;
                        report.merged.push(key);
                    }
                    MergeOutcome::NeedsResolution => {
                        // Keep local (already staged); surface both sides.
                        report.conflicts.push(PullConflict {
                            key,
                            local: Box::new(local),
                            remote: Box::new(remote),
                        });
                    }
                }
            }
            // One-sided presence (modify vs delete or purge), or equal bodies:
            // keep the local side, which is already staged from `local_tree`.
            _ => {}
        }
    }

    // Commit the reconciled tree with both parents, then advance the branch.
    let tree_oid = index.write_tree_to(repo).map_err(be)?;
    let tree = repo.find_tree(tree_oid).map_err(be)?;
    let sig = git2::Signature::now("gonzalo", "gonzalo@localhost").map_err(be)?;
    let refname = format!("refs/heads/{branch}");
    repo.commit(
        Some(&refname),
        &sig,
        &sig,
        &format!("merge {remote}/{branch}"),
        &tree,
        &[&local_commit, &remote_commit],
    )
    .map_err(be)?;
    repo.set_head(&refname).map_err(be)?;
    repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))
        .map_err(be)?;

    Ok(report)
}

/// Serialize `record` as the store does on `put` and stage it at `path`.
fn stage_record(index: &mut git2::Index, path: &Path, record: &Record) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(record).map_err(|e| CoreError::Serde(e.to_string()))?;
    index
        .add_frombuffer(&blob_entry(path), &bytes)
        .map_err(be)
}
```

**3d. `merged_record`.** Replace the whole function (doc comment through closing brace) with:

```rust
/// The merged record from an auto-resolved divergence: a fresh revision over the
/// merged `body`, mirroring `sync`'s merged-record construction, with both
/// sides' revisions and ancestors folded and truncated to the store's `cap`.
fn merged_record(
    key: &RecordKey,
    local: &Record,
    remote: &Record,
    body: Body,
    cap: usize,
) -> Record {
    let revision = Revision {
        counter: local.revision.counter.max(remote.revision.counter) + 1,
        hash: ContentHash::of(body.bytes()),
    };
    let ancestors = reconciled_ancestors(&revision, local, remote, cap);
    let mut labels = local.meta.labels.clone();
    labels.extend(remote.meta.labels.clone());
    let mut links = local.links.clone();
    for l in &remote.links {
        if !links.contains(l) {
            links.push(l.clone());
        }
    }
    let parent = if local.revision.counter >= remote.revision.counter {
        local.revision.clone()
    } else {
        remote.revision.clone()
    };
    Record {
        key: key.clone(),
        kind: local.kind,
        revision,
        parent: Some(parent),
        body,
        meta: Meta {
            author: Identity::new("gonzalo-merge"),
            origin_system: "git-pull".into(),
            created: local.meta.created.min(remote.meta.created),
            updated: local.meta.updated.max(remote.meta.updated),
            labels,
        },
        links,
        ancestors,
        deleted_at: None,
    }
}
```

If any import in the `use gonzalo_core::{...}` block is now unused (clippy `unused_imports`), remove only that name.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-store-git --test pull`
Expected: `test result: ok. 13 passed; 0 failed`

Run: `cargo test -p gonzalo-store-git --all-features`
Expected: every test binary (`conformance`, `pull`, `put_and_push`, unit tests) reports `0 failed`.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-store-git/src/lib.rs crates/gonzalo-store-git/tests/pull.rs
git commit -m "feat(git): tombstone-aware non-fast-forward pull (#203)

Both-sided changes are decided by kind before the body merge: equal
revisions are a no-op, diverged tombstones converge on the higher revision
with folded ancestors, delete-vs-edit is a PullConflict that keeps local.
Merged records fold both chains, truncated to the store's ancestor cap.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 5: Full gate, push, open the PR

**Files:** none changed.

**Interfaces:**
- Consumes: Tasks 1–4 committed on branch `feat/203-tombstones-05-sync-pull`.
- Produces: an open PR against `main`, with CI green before merge.

- [ ] **Step 1: Confirm branch**

Run: `git rev-parse --abbrev-ref HEAD`
Expected: `feat/203-tombstones-05-sync-pull`. If this prints `main`, stop: create the branch with `git switch -c feat/203-tombstones-05-sync-pull` before continuing. The commits move with it.

- [ ] **Step 2: Format check**

Run: `cargo fmt --all -- --check`
Expected: no output, exit 0. On failure run `cargo fmt --all`, then commit with `git commit -am "style: cargo fmt" -m "Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"` and re-run the check.

- [ ] **Step 3: Lint**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: `Finished` with no `warning:` or `error:` lines.

- [ ] **Step 4: Build**

Run: `cargo build --workspace --all-targets --all-features`
Expected: `Finished`.

- [ ] **Step 5: Test**

Run: `cargo test --workspace --all-features`
Expected: every `test result:` line reports `0 failed`.

- [ ] **Step 6: Push**

Run: `git push -u origin feat/203-tombstones-05-sync-pull`
Expected: `branch 'feat/203-tombstones-05-sync-pull' set up to track 'origin/feat/203-tombstones-05-sync-pull'`.

- [ ] **Step 7: Open the PR**

```bash
gh pr create --base main --head feat/203-tombstones-05-sync-pull \
  --title "feat: tombstone-aware sync and git pull (#203, slice 5)" \
  --body "$(cat <<'EOF'
Part of #203

Slice 5 of the replicated-deletion plan (docs/superpowers/plans/2026-09-13-tombstones-05-sync-and-pull.md).

## Sync (spec §3.4)
- `sync_pass` reads through `list_raw` / `get_raw`, so tombstones replicate.
- A key held by both sides uses the in-record `ancestors` list: equal revision skips; a revision in the other side's chain fast-forwards (conditional overwrite); diverged tombstones converge on the higher `(counter, hash)`; one tombstone against a live edit is a `SyncConflict`; two live records take the existing merge path with both chains folded.
- `SyncReport` gains `fast_forwarded_to_a` / `fast_forwarded_to_b`; `gonzalo sync` prints `fast_forwarded:`.
- Behaviour change: an `Opaque` kind (Checkpoint) that is simply behind now fast-forwards instead of conflicting. Legacy records with no ancestors still take the merge path.
- Every sync write (copy, fast-forward, tombstone winner, merge) goes through `put_raw`, which never re-stamps. A write that hits `Conflict` or `NotFound` (the key moved or was purged mid-sync) is a race that re-loops, not a hard error. A tombstone that arrives between sync's read and its copy is not resurrected (tested).
- Ancestor cap: sync folds losslessly and each store's `plan_put_raw` truncates to its own cap on write.

## Git pull (spec §3.5)
- Both-sided changes are decided by kind before the body merge: equal revisions no-op, diverged tombstones converge, delete-vs-edit is a `PullConflict` keeping local.
- Merged records fold both chains, truncated to the store's cap.
- Remote-only tombstones and purges are applied verbatim (pinned by tests).

## Verification
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo build --workspace --all-targets --all-features`
- `cargo test --workspace --all-features`

https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

Expected: the PR URL is printed.

- [ ] **Step 8: Wait for CI and merge**

Run: `gh pr checks --watch`
Expected: all checks `pass`. Then run `gh pr merge --squash --delete-branch`.
If a check fails, fix it on the branch, re-run Steps 2–5, then push.

---

## Contract assumptions

- `Store::put_raw` / `plan_put_raw` (slice 1) are the replication write, and every sync write goes through them. The copy window, where a tombstone arrives between sync's raw read and its copy, is closed by construction: `put_raw(rec, None)` over a tombstone returns `Conflict`, and `tombstone_arriving_mid_copy_is_not_resurrected` pins that down.
- `GitStore` carries `cap: usize` (slice 2, per the overview's per-store builder contract).

## Spec gaps found while planning

- ~~§3.5 doesn't cover both-sided `(Some, None)` / `(None, Some)`: edit vs purge, or purge vs edit. Current code keeps local silently, including when local purged and the remote edited. `PullConflict` can't represent an absent side. Left unchanged.~~ **Resolved** by Ruling Q1(b): spec §3.5 now keeps local for `(Some, None)` and takes the remote side for `(None, Some)`.
- §3.5 said nothing about checkout order. A non-fast-forward pull that moved HEAD before its forced checkout left remotely purged files behind as untracked. **Resolved** by Ruling Q2(a); see the §3.5 amendment.
- A crash between `GitStore::delete_as` writing a tombstone and committing it leaves an uncommitted file that pull's forced checkout overwrites. `put` has the same gap, tracked in gonzalo#283. Pull now takes `lock_repo` (Ruling Q3(a)), which closes the in-process race but not the crash window.
- §3.5 doesn't say that pull writes bypass `put_raw`, so the git merge path has to apply the store's cap itself (Decision 4).
- §3.4 doesn't give a cap for `build_merged` or the tombstone winner (Decision 3).
- §3.4 doesn't mention that a sync write can now hit `NotFound` (key purged mid-sync) or `Conflict` on a tombstone (Decision 5). It also predates `put_raw`: it describes overwrites as `put` with `expected`.
- §3.4 says legacy data "behaves exactly as before", but a racing sync whose first pass wrote the merged record now finishes with a fast-forward on the second pass. `re_loops_until_a_racing_store_converges` changes its assertion from `merged` to `fast_forwarded_to_b`.
- Tombstones from different chains with the same counter have identical revisions (fixed hash), so sync and pull treat them as already in sync and never union their ancestors. Both sides are deleted, so this is harmless, but it isn't stated.

## Self-review

- **§3.4 coverage:** equal skip (`Relation::InSync`), both ahead directions (`AAhead`/`BAhead`, expected = behind side's revision), both tombstones (`tombstone_winner` written to both), exactly one tombstone (`SyncConflict`), both live (merge + `reconciled_ancestors`), one-sided copy unchanged and tombstones included (raw reads). Covered in Task 2.
- **§3.5 coverage:** remote-only verbatim (unchanged code, pinned by `nonff_pull_applies_remote_tombstone` and `nonff_pull_applies_remote_purge`), both tombstones equal and different, one tombstone conflict, `merged_record` folds. The body guard is preceded by revision and kind arms. Covered in Task 4.
- **§6.2 coverage:** stale peer, reversed direction, recreation, delete vs edit, concurrent tombstones, Opaque fast-forward (plus true divergence still conflicts), legacy merge path, merge folds ancestors, truncated chain, mixed caps, Flaky and AlwaysRacy tombstone variants. All in Task 2 Step 2.
- **§6.3 coverage:** all six named tests, plus `nonff_pull_identical_tombstones_are_a_noop` and `nonff_pull_merged_record_folds_both_ancestries`. Task 4 Step 1.
- **Name consistency:** `fast_forwarded_to_a`/`fast_forwarded_to_b` (Task 2) are consumed in Task 3. `reconciled_ancestors`/`tombstone_winner` (Task 1) are consumed in Tasks 2 and 4. `GitStore.cap` comes from slice 2 per the overview's per-store builder contract.
