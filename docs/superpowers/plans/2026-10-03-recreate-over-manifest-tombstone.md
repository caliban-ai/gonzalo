# Guarding a Manifest Tombstone Against Recreation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop an ordinary `put` from silently destroying a deleted manifest's restore window, and give operators an explicit way to discard that window when they mean to.

**Architecture:** One new arm in `plan_put` refuses a create over a tombstone that carries `deleted_kind`, keyed on the **stored** tombstone rather than the incoming record. `PutPlan::Rejected` already exists and every substrate already maps it to `CoreError::Invalid` → HTTP 400, so the guard needs no new plan variant, substrate change, route or proto change. A new `gonzalo purge` subcommand is the deliberate escape hatch, and it refuses anything that is not a tombstone.

**Tech Stack:** Rust (edition 2024), `serde`, `async-trait`, `tokio`, `clap`, `anyhow` (CLI).

**Spec:** `docs/superpowers/specs/2026-10-03-recreate-over-manifest-tombstone-design.md`

## Global Constraints

- The guard reads the **stored** tombstone's `deleted_kind`, never the incoming record's. It must fire for both `GraphManifest` and `VectorManifest`, and for no other kind.
- The guard lives in `plan_put` only. **`plan_put_raw` must not change** — replication's create-over-tombstone Conflict and `undelete`'s matching-revision write both depend on it.
- No new `PutPlan` variant, no substrate change, no new daemon route, no `.proto` change, no OpenAPI change.
- `PutPlan::Rejected` carries `&'static str`, so the refusal message is a constant and cannot name the key. Follow the `CONSUMER_TOMBSTONE_REJECTED` precedent.
- `Store::purge` and `plan_purge` keep their current contracts — `collect` depends on them. The tombstone check for `purge` lives at the **operator surface** (the CLI wrapper), not in core.
- `gonzalo purge` refuses a live record, and treats an absent key as an error (exit 1), deliberately unlike `gonzalo delete`, which exits 0 for an absent key. No confirmation prompt and no `--yes` flag.
- Refusals exit 1 via anyhow with the reason on stderr. `EXIT_CONFLICT` (3) is not reused.
- The gate is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace --all-targets --all-features`, `cargo test --workspace --all-features`.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.

## Review Focus

Five conditions the spec implies that no task's happy path reaches. Each one's test is added to the task that owns the code.

1. **`undelete` must still work.** It is the primary recovery path and rides `plan_put_raw`. A guard implemented in the wrong planner — or applied too broadly — would break the very capability this ticket protects, and every other test here would still pass. → Task 1.
2. **`collect` past the horizon, then create, must succeed.** The guard must not make a manifest key permanently unwritable. Once the tombstone is gone there is no window to protect, so the key must accept a create again. → Task 3.
3. **`reset` then `gonzalo index` must fail cleanly.** This is the ergonomic regression the design accepts; it has to be a clean refusal with guidance and nothing partially written, not a panic or a half-committed manifest. → Task 4, which owns `cli/src/lib.rs`; see the note in its Files block.
4. **Replication must be untouched.** A `put_raw` create over a manifest tombstone must still Conflict exactly as before, or sync starts resurrecting deleted manifests. → Task 2.
5. **A manifest tombstone whose retained body does not parse must still refuse cleanly.** The guard reads `deleted_kind` and must never parse the body, so a corrupt window still produces the refusal rather than a serde error. → Task 1.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/gonzalo-core/src/tombstone.rs` | **Modify.** The refusal constant, the new `plan_put` arm, and their unit tests. |
| `crates/gonzalo-core/src/lib.rs` | **Modify.** Re-export the new constant beside `CONSUMER_TOMBSTONE_REJECTED`. |
| `crates/gonzalo-core/src/conformance.rs` | **Modify.** The case every substrate runs. |
| `crates/gonzalo-vector/tests/durability.rs` | **Modify.** End-to-end: a deleted index survives an upsert attempt; collect-then-create works. |
| `crates/gonzalo-cli/src/lib.rs` | **Modify.** The `purge` wrapper with its live-record refusal. |
| `crates/gonzalo-cli/src/main.rs` | **Modify.** The `gonzalo purge` subcommand. |
| `crates/gonzalo-cli/tests/cli.rs` | **Modify.** `purge` tests and the `index`-after-delete refusal. |
| `docs/adr/0030-manifest-tombstone-recreate-guard.md` | **Create.** The decision record. |

**Verified API shapes** — re-verified against `da91ba0` after rebasing past #329 and #331.
Use them verbatim. The `crates/gonzalo-cli/src/lib.rs` line numbers moved when #331
inserted `IndexOptions` earlier in that file; `tombstone.rs` and every substrate
reference were unaffected.

- `PutPlan::{Write(Record), Conflict(Box<Conflict>), NotFound, Rejected(&'static str)}` (`tombstone.rs:135-149`).
- `pub const CONSUMER_TOMBSTONE_REJECTED: &str = "consumer put cannot write a tombstone; use delete_as";` (`tombstone.rs:153-154`) — the style to match.
- Every substrate maps `Rejected(reason)` to `CoreError::Invalid(reason.to_string())`: `fs` at `lib.rs:437`, `s3` at `lib.rs:650` and `lib.rs:1457`, `git` at `lib.rs:125`, `memstore` at `memstore.rs:83`.
- `plan_put`'s tombstone arm is at `tombstone.rs:193-219`; the recreation body clears `deleted_at`, `deleted_blob`, `deleted_kind` and re-stamps `meta.created`.
- `plan_purge(current: Option<&Record>, expected: &Revision) -> PurgePlan` (`tombstone.rs:328-338`) — **does not check the record's kind.**
- `Store::purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult>`.
- `tomb(counter)` test helper = `tombstone_of(&live(counter - 1, ..), 42, 32, None)` (`tombstone.rs:740-743`), so its `deleted_kind` is `None`.
- `open_store(root: &Path, ancestor_cap: usize) -> anyhow::Result<FsStore>` (`cli/src/lib.rs:1111-1115`).
- CLI wrapper shape: `pub async fn undelete(root: &Path, ancestor_cap: usize, namespace: &str, collection: &str, id: &str) -> Result<Revision>` (`cli/src/lib.rs:1158-1169`), using `open_store` and `CLI_AUTHOR`.
- `Commands::Delete`'s `root` arg: `#[arg(long, default_value = ".", value_parser = store_root)] root: PathBuf` (`main.rs:163-164`).
- `conformance.rs` registration list is in `run_tombstone_conformance` (`conformance.rs:255-268`); `recreate_continues_chain` is registered at `:264` and defined at `:545`. `consumer_put_of_a_tombstone_is_rejected` is the model for asserting a refusal.
- `cli.rs` test helpers: `run(root, args)` appends `--root <root>` itself; also `stdout`, `stderr`, `seed(root, ns, col, ids)`, `record_file`, `edit_record_file`.
- `blob_gc.rs` helpers (for reference): `fresh_store()`, `meta()`, `committed(store, record)` — two args, `immediate()`.
- `durability.rs` uses `RecordVectorIndex::open(store, key, space, dim)`, `upsert_many`, `keys(&KeyPrefix)`, `query(&vec, k, &prefix)`, and `const DIM: usize = 16`.

---

### Task 1: The guard in `plan_put`

**Files:**
- Modify: `crates/gonzalo-core/src/tombstone.rs` (the constant, `plan_put`'s tombstone arm, `mod tests`), `crates/gonzalo-core/src/lib.rs` (re-export)

**Interfaces:**
- Produces: `pub const MANIFEST_TOMBSTONE_RECREATE_REJECTED: &str`, and `plan_put` returning `PutPlan::Rejected(MANIFEST_TOMBSTONE_RECREATE_REJECTED)` for a create over a tombstone whose `deleted_kind.is_some()`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/gonzalo-core/src/tombstone.rs`. Read the neighbouring `plan_put` tests first — they use `tomb(counter)` and `live(counter, body, ancestors)`; match that idiom and add only the imports the module lacks.

```rust
    /// A tombstone of a manifest kind: the shape that carries a restore window.
    fn manifest_tomb(counter: u64, kind: RecordKind) -> Record {
        let mut t = tomb(counter);
        t.deleted_kind = Some(kind);
        t
    }

    // The bug. A create over a manifest tombstone would overwrite it and
    // discard the retained body, unpinning blobs that may be unregenerable.
    #[test]
    fn create_over_a_vector_manifest_tombstone_is_rejected() {
        let t = manifest_tomb(3, RecordKind::VectorManifest);
        assert_eq!(
            plan_put(Some(&t), live(0, b"again", vec![]), None, 32),
            PutPlan::Rejected(MANIFEST_TOMBSTONE_RECREATE_REJECTED)
        );
    }

    #[test]
    fn create_over_a_graph_manifest_tombstone_is_rejected() {
        let t = manifest_tomb(3, RecordKind::GraphManifest);
        assert_eq!(
            plan_put(Some(&t), live(0, b"again", vec![]), None, 32),
            PutPlan::Rejected(MANIFEST_TOMBSTONE_RECREATE_REJECTED)
        );
    }

    // The guard is surgical: where no restore window exists, ADR 0021's
    // recreation rule still applies unchanged.
    #[test]
    fn create_over_a_non_manifest_tombstone_still_recreates() {
        let t = tomb(3);
        assert_eq!(t.deleted_kind, None, "tomb() must stay a non-manifest tombstone");
        let PutPlan::Write(stored) = plan_put(Some(&t), live(0, b"again", vec![]), None, 32) else {
            panic!("a non-manifest tombstone must still be recreatable");
        };
        assert!(!stored.is_tombstone());
        assert_eq!(stored.revision.counter, t.revision.counter + 1);
    }

    // The new arm must not swallow the stale-revision case.
    #[test]
    fn put_with_expected_over_a_manifest_tombstone_is_still_not_found() {
        let t = manifest_tomb(3, RecordKind::VectorManifest);
        let expected = t.revision.clone();
        assert_eq!(
            plan_put(Some(&t), live(0, b"x", vec![]), Some(expected), 32),
            PutPlan::NotFound
        );
    }

    // Review Focus 5: the guard reads `deleted_kind` and must never parse the
    // body, so a corrupt window still produces a clean refusal.
    #[test]
    fn a_manifest_tombstone_with_an_unparseable_body_still_refuses_cleanly() {
        let mut t = manifest_tomb(3, RecordKind::VectorManifest);
        t.body = Body::Inline(b"not a manifest at all".to_vec());
        assert_eq!(
            plan_put(Some(&t), live(0, b"again", vec![]), None, 32),
            PutPlan::Rejected(MANIFEST_TOMBSTONE_RECREATE_REJECTED)
        );
    }

    // Review Focus 1: `undelete` is the recovery path this ticket protects, and
    // it rides `plan_put_raw`. A guard in the wrong planner would break it while
    // every other test here still passed.
    #[test]
    fn put_raw_over_a_manifest_tombstone_is_unaffected_by_the_guard() {
        let t = manifest_tomb(3, RecordKind::VectorManifest);
        let mut incoming = live(0, b"restored", vec![]);
        incoming.revision = Revision {
            counter: t.revision.counter + 1,
            hash: ContentHash::of(b"restored"),
        };
        incoming.parent = Some(t.revision.clone());

        // The restore shape: put_raw with the tombstone's own revision.
        let PutPlan::Write(stored) =
            plan_put_raw(Some(&t), incoming, Some(t.revision.clone()), 32)
        else {
            panic!("put_raw must still write a restore over a manifest tombstone");
        };
        assert!(!stored.is_tombstone());
        assert_eq!(stored.ancestors.first(), Some(&t.revision));
    }
```

Check `plan_put_raw`'s exact parameter order before writing the last test — match how the module's existing `plan_put_raw` tests call it.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core tombstone`
Expected: FAIL to compile — `MANIFEST_TOMBSTONE_RECREATE_REJECTED` does not exist. Once the constant exists but the arm does not, the two rejection tests fail with `Write(..)` instead of `Rejected(..)`; confirm you see that intermediate state, because it is the one that proves the arm is doing the work.

- [ ] **Step 3: Add the constant**

In `crates/gonzalo-core/src/tombstone.rs`, immediately after `CONSUMER_TOMBSTONE_REJECTED`:

```rust
/// The reason a consumer `put` that would create over a *manifest* tombstone is
/// rejected (see [`PutPlan::Rejected`]). A manifest tombstone retains the
/// deleted body, which is the restore window ADR 0029 promises; overwriting it
/// would unpin blobs that may be unregenerable (ADR 0030).
pub const MANIFEST_TOMBSTONE_RECREATE_REJECTED: &str =
    "a deleted manifest is at this key; restore it with `gonzalo undelete`, \
     or discard the tombstone with `gonzalo purge`";
```

Re-export it from `crates/gonzalo-core/src/lib.rs` beside `CONSUMER_TOMBSTONE_REJECTED` — find that name in the `pub use tombstone::{…}` list and add the new one in the same position relative to its neighbours.

- [ ] **Step 4: Add the guard arm**

In `plan_put`'s tombstone arm (`tombstone.rs:193`), add a new first branch to the inner `match expected`, leaving the existing `None` and `Some(_)` branches exactly as they are:

```rust
        Some(t) if t.is_tombstone() => match expected {
            // A manifest tombstone is a restore window: its retained body is
            // the only record of which blob was which shard, and GC marks
            // through it (ADR 0029). A create would overwrite the tombstone and
            // discard that body, so refuse and make the caller choose between
            // restoring and discarding (ADR 0030). Keyed on the STORED
            // tombstone, never the incoming record: an incoming `deleted_kind`
            // is a client smuggling a field, handled by the recreation arm.
            None if t.deleted_kind.is_some() => {
                PutPlan::Rejected(MANIFEST_TOMBSTONE_RECREATE_REJECTED)
            }
            None => { /* … the existing recreation arm, unchanged … */ }
            Some(_) => PutPlan::NotFound,
        },
```

Do not touch `plan_put_raw`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core`, then `cargo test --workspace --all-features`
Expected: PASS. `recreating_over_a_tombstone_clears_deleted_kind` (`tombstone.rs:812`) must still pass **unchanged** — its stored tombstone comes from `tomb()` and so has `deleted_kind: None`. If it fails, the guard is reading the incoming record instead of the stored one; fix the guard, not the test.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "$(printf 'fix(core): refuse a create over a manifest tombstone (#333)\n\nplan_put now rejects a consumer create over a tombstone carrying deleted_kind,\nso an ordinary put can no longer discard the retained body and unpin blobs that\nmay be unregenerable. put_raw is untouched, so replication and undelete still\nwork exactly as before.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 2: Hold every substrate to the refusal

**Files:**
- Modify: `crates/gonzalo-core/src/conformance.rs`

**Interfaces:**
- Consumes: the guard and `MANIFEST_TOMBSTONE_RECREATE_REJECTED` (Task 1).

**Why conformance:** `plan_put` is reached through a `PutPlanner` function pointer on `fs`, `git` and `memstore`, and inline at two S3 call sites. "Every substrate refuses identically, and none writes anyway" is a claim only this suite can make.

- [ ] **Step 1: Write the case and register it**

Add to `crates/gonzalo-core/src/conformance.rs`. Read `consumer_put_of_a_tombstone_is_rejected` first — it is the existing model for asserting a refusal through a real store, and your case should match how it inspects the error.

```rust
/// A consumer `put` that would create over a manifest tombstone is refused, and
/// nothing is written. The tombstone retains the deleted manifest's body, which
/// is the restore window GC marks through (ADR 0029); a substrate that wrote
/// anyway — or mapped the refusal to a different error — would silently unpin
/// blobs that may be unregenerable (ADR 0030).
async fn create_over_a_manifest_tombstone_is_refused<S: Store>(store: &S) {
    let key = tomb_key("manifest-recreate");
    let mut m = VectorManifest::new("space-a", 3, 256);
    m.entries.insert(0, ContentHash::of(b"shard-zero"));
    let body = m.to_body();

    let mut record = sample(key.clone(), b"unused");
    record.kind = RecordKind::VectorManifest;
    record.revision = Revision::initial(body.bytes());
    record.body = body.clone();
    committed(store, record, None).await;
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );

    // A create at the key, as any ordinary writer would issue it.
    let err = store
        .put(sample(key.clone(), b"replacement"), None)
        .await
        .expect_err("a create over a manifest tombstone must be refused");
    assert!(
        matches!(err, CoreError::Invalid(_)),
        "every substrate maps Rejected to Invalid, got {err:?}"
    );

    // The window survives: the tombstone is still there, with its body.
    let t = store.get_raw(&key).await.unwrap().expect("tombstone survives");
    assert!(t.is_tombstone());
    assert_eq!(t.body, body, "the retained body was not discarded");
    assert_eq!(t.deleted_kind, Some(RecordKind::VectorManifest));
}

/// Review Focus 4: replication is untouched. A `put_raw` create over a manifest
/// tombstone must still Conflict, or sync starts resurrecting deleted manifests.
async fn put_raw_create_over_a_manifest_tombstone_still_conflicts<S: Store>(store: &S) {
    let key = tomb_key("manifest-raw-create");
    let mut record = sample(key.clone(), b"unused");
    record.kind = RecordKind::VectorManifest;
    let body = VectorManifest::new("space-a", 3, 256).to_body();
    record.revision = Revision::initial(body.bytes());
    record.body = body;
    committed(store, record, None).await;
    store.delete(&key, None).await.unwrap();

    match store.put_raw(sample(key.clone(), b"from-peer"), None).await.unwrap() {
        PutResult::Conflict(c) => assert!(c.current.is_tombstone()),
        PutResult::Committed(rev) => {
            panic!("put_raw must not create over a tombstone, committed {rev:?}")
        }
    }
}
```

Register both inside `run_tombstone_conformance` (`conformance.rs:255-268`), in that function's existing call style — put them after `recreate_continues_chain` at `:264`, so the recreation rule and its new exception read together.

- [ ] **Step 2: Run it against one substrate to verify it fails**

Run: `cargo test -p gonzalo-store-fs conformance`
Expected: `create_over_a_manifest_tombstone_is_refused` FAILS before Task 1 is in (the put succeeds), and passes after. `put_raw_create_over_a_manifest_tombstone_still_conflicts` is a non-regression guard and should pass immediately — **say so plainly rather than inventing a failure for it.**

- [ ] **Step 3: Run the suites and commit**

Run: `cargo test -p gonzalo-store-fs`, `cargo test -p gonzalo-core --all-features`, then the full gate. The S3 conformance path needs a live endpoint and is skipped locally — say so in your report rather than claiming it ran.

```bash
git add crates/gonzalo-core/src/conformance.rs
git commit -m "$(printf 'test(core): conformance for refusing a create over a manifest tombstone (#333)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 3: End-to-end — the hazard is closed, and the key is not bricked

**Files:**
- Modify: `crates/gonzalo-vector/tests/durability.rs`

**Interfaces:**
- Consumes: the guard (Task 1).

This task proves the fix where the bug actually bit, and proves the guard did not create a worse problem by making a manifest key permanently unwritable.

- [ ] **Step 1: Write the failing tests**

Add to `crates/gonzalo-vector/tests/durability.rs`, reusing that file's `DIM` and its existing idiom for building an index. Extend its `use gonzalo_core::{…}` list with what these need (`CoreError`, `DeleteResult`, `Store`, `BlobStore`, `SweepPolicy`, `gc_blobs_with`, `collect`, `KeyPrefix`, `now_ms`), plus `std::time::{Duration, SystemTime}`.

```rust
/// The hazard #333 exists for: reopening a deleted index and writing to it used
/// to overwrite the tombstone and orphan the shards. Now the commit refuses and
/// the shards stay pinned (ADR 0030).
#[tokio::test]
async fn an_upsert_on_a_deleted_index_is_refused_and_the_shards_stay_pinned() {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    let key = VectorManifest::key("ns", "guarded");

    let mut probe = vec![0.0; DIM];
    probe[1] = 1.0;

    let idx = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "guarded-space", DIM)
        .await
        .unwrap();
    idx.upsert_many(vec![(RecordKey::new("ns", "coll", "a"), probe.clone())])
        .await
        .unwrap();
    drop(idx);

    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );

    // Reopen: this still succeeds, because `open` reads through `store.get`,
    // which hides tombstones. The refusal lands on the first commit instead.
    let reopened =
        RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "guarded-space", DIM)
            .await
            .unwrap();
    let err = reopened
        .upsert_many(vec![(RecordKey::new("ns", "coll", "b"), probe.clone())])
        .await
        .expect_err("the commit must be refused while a tombstone holds the key");
    assert!(matches!(err, CoreError::Invalid(_)), "got {err:?}");

    // The window survived, so the shards are still pinned.
    let swept = gc_blobs_with(
        &store,
        SweepPolicy {
            min_age: Duration::ZERO,
            now: SystemTime::now(),
        },
    )
    .await
    .unwrap();
    assert!(swept.freed.is_empty(), "the refusal kept the shards pinned");

    // And the record is still restorable.
    gonzalo_core::undelete(&store, &key, now_ms(), None).await.unwrap();
    let restored = RecordVectorIndex::open(FsStore::new(dir.path()), key, "guarded-space", DIM)
        .await
        .unwrap();
    assert_eq!(restored.keys(&KeyPrefix::default()).await.unwrap().len(), 1);
}

/// Review Focus 2: the guard must not brick the key. Once `collect` has removed
/// the tombstone there is no window left to protect, so a create must work
/// again — otherwise a deleted index's key becomes permanently unwritable.
#[tokio::test]
async fn after_collect_removes_the_tombstone_a_fresh_index_can_be_created() {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    let key = VectorManifest::key("ns", "recycled");

    let mut probe = vec![0.0; DIM];
    probe[2] = 1.0;

    let idx = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "recycled-space", DIM)
        .await
        .unwrap();
    idx.upsert_many(vec![(RecordKey::new("ns", "coll", "a"), probe.clone())])
        .await
        .unwrap();
    drop(idx);
    store.delete(&key, None).await.unwrap();

    // Past the horizon: collect removes the tombstone, and with it the window.
    let collected = collect(&store, &KeyPrefix::default(), Duration::ZERO, now_ms() + 1)
        .await
        .unwrap();
    assert_eq!(collected.purged.len(), 1);

    // A fresh index at the same key now commits normally.
    let fresh = RecordVectorIndex::open(FsStore::new(dir.path()), key, "recycled-space", DIM)
        .await
        .unwrap();
    fresh
        .upsert_many(vec![(RecordKey::new("ns", "coll", "b"), probe)])
        .await
        .expect("with no tombstone there is no window to protect");
    assert_eq!(fresh.keys(&KeyPrefix::default()).await.unwrap().len(), 1);
}
```

`collect`'s prefix is `KeyPrefix::default()` here, which matches every key in the store; that is fine in a single-index test. Check `VectorManifest::key`'s namespace before assuming a narrower prefix would match.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p gonzalo-vector --test durability`
Expected: before Task 1, `an_upsert_on_a_deleted_index_is_refused_and_the_shards_stay_pinned` fails because the upsert **succeeds** — that failure is the bug, so read the output and confirm it says the error was expected but the commit went through. After Task 1 both pass.

- [ ] **Step 3: Run and commit**

Run: `cargo test -p gonzalo-vector`, then the full gate.

```bash
git add crates/gonzalo-vector/tests/durability.rs
git commit -m "$(printf 'test(vector): a deleted index refuses writes and stays restorable (#333)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 4: `gonzalo purge`

**Files:**
- Modify: `crates/gonzalo-cli/src/lib.rs` (the `purge` wrapper, **and** the Review Focus 3 test in its `mod tests`), `crates/gonzalo-cli/src/main.rs`, `crates/gonzalo-cli/tests/cli.rs`

**Interfaces:**
- Consumes: `Store::purge`, `Store::get_raw`; the guard's refusal message, which names this command.
- Produces: `pub async fn purge(root: &Path, ancestor_cap: usize, namespace: &str, collection: &str, id: &str) -> Result<Revision>` returning the purged tombstone's revision; the `gonzalo purge` subcommand.

**The live-record refusal is the point of this task.** `plan_purge` (`tombstone.rs:328-338`) does **not** check the record's kind — it removes whatever matches `expected`, live or tombstone, and `Store::purge`'s own doc describes it as the mechanism tombstone collection uses. Exposing it to operators without a check would hand them a way to physically delete a live record leaving no tombstone, which a peer that has not synced would then resurrect. Core keeps its contract; this wrapper adds the guard.

**One shape decision, so it is not made twice:** the wrapper takes `ancestor_cap` because `open_store` requires one and its siblings do, but the **subcommand does not expose `--ancestor-cap`** and passes `DEFAULT_ANCESTOR_CAP`. Purge never folds ancestors, so a user-facing flag would be inert.

- [ ] **Step 1: Write the failing tests**

Add to `crates/gonzalo-cli/tests/cli.rs`. Its `run(root, args)` helper appends `--root <root>` itself, so the args array must not contain `--root`. Prefer `assert_eq!` over `let _ =` where a precondition is worth pinning.

```rust
#[test]
fn purge_removes_a_tombstone_and_lets_the_key_be_recreated() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);

    // Make it a retaining kind, so the guard would otherwise block recreation.
    let body = serde_json::to_value(gonzalo_core::Manifest::new().to_body()).unwrap();
    edit_record_file(
        &record_file(root.path(), "ns", "col", "note.md"),
        |record| {
            record.insert("kind".into(), serde_json::json!("GraphManifest"));
            record.insert("body".into(), body);
        },
    );

    let del = run(
        root.path(),
        &["delete", "--namespace", "ns", "--collection", "col", "--id", "note.md"],
    );
    assert_eq!(del.status.code(), Some(0), "{del:?}");

    let out = run(
        root.path(),
        &["purge", "--namespace", "ns", "--collection", "col", "--id", "note.md"],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        stdout(&out).starts_with("purged: ns/col/note.md"),
        "got {:?}",
        stdout(&out)
    );

    // The tombstone is physically gone, so the record reads as absent raw too.
    assert!(
        !record_file(root.path(), "ns", "col", "note.md").is_file(),
        "purge removes the record file"
    );

    // And the key accepts a create again — purge is not a dead end, which is
    // the whole reason it exists as the guard's escape hatch.
    seed(root.path(), "ns", "col", &["note.md"]);
    assert!(
        run(root.path(), &["get", "ns", "col", "note.md"]).status.success(),
        "the key is writable again after purge"
    );
}

// The trap in plan_purge must not reach operators: purging a LIVE record would
// leave no tombstone, and a peer that had not synced would resurrect it.
#[test]
fn purge_refuses_a_live_record_and_leaves_it_alone() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);

    let out = run(
        root.path(),
        &["purge", "--namespace", "ns", "--collection", "col", "--id", "note.md"],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).contains("live"),
        "the refusal must say the record is live, got {:?}",
        stderr(&out)
    );
    assert!(
        run(root.path(), &["get", "ns", "col", "note.md"]).status.success(),
        "the live record survives a refused purge"
    );
}

// Deliberately unlike `delete`, which exits 0 for an absent key: purge is a
// targeted destructive act, so a typo must be visible.
#[test]
fn purge_of_an_absent_key_exits_one() {
    let root = TempDir::new().unwrap();
    let out = run(
        root.path(),
        &["purge", "--namespace", "ns", "--collection", "col", "--id", "nope"],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "nothing on stdout when nothing was purged");
    assert!(stderr(&out).contains("not found"), "got {:?}", stderr(&out));
}

```

**Review Focus 3 goes in a different file, deliberately.** `crates/gonzalo-cli/tests/cli.rs` has **no** `index` tests at all, so driving the full indexer through the binary would mean building a source tree, grammar availability and flag plumbing from scratch. But `crates/gonzalo-cli/src/lib.rs`'s own test module already has the harness: a local `async fn index(root, src, repo, view) -> Result<IndexSummary>` helper (`lib.rs:1448`) that a dozen tests already call as `index(root.path(), src.path(), "r", "main")` (`lib.rs:1748`, `:1818`, `:1846`, …). Add this there, beside those tests, and follow whatever they use to build `src`:

```rust
    // Review Focus 3: the ergonomic regression this design accepts must be a
    // clean refusal with guidance — not a panic, and not a half-written
    // manifest. This is the graph-indexer half of #333's hazard: `index`
    // derives `expected` from a consumer `get`, which hides the tombstone.
    #[tokio::test]
    async fn index_over_a_deleted_manifest_is_refused_with_guidance() {
        let root = TempDir::new().unwrap();
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.rs"), "fn a() {}").unwrap();

        index(root.path(), src.path(), "r", "main").await.unwrap();

        // Delete the view's manifest, as `gonzalo delete` or `reset` would.
        let store = open_store(root.path(), DEFAULT_ANCESTOR_CAP).unwrap();
        let key = gonzalo_core::Manifest::key("r", "main");
        assert_eq!(
            store.delete(&key, None).await.unwrap(),
            gonzalo_core::DeleteResult::Deleted
        );
        let tomb = store.get_raw(&key).await.unwrap().expect("tombstone");
        let retained = tomb.body.clone();

        // Re-indexing must refuse rather than overwrite the restore window.
        let err = index(root.path(), src.path(), "r", "main")
            .await
            .expect_err("indexing over a deleted manifest must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("gonzalo undelete") && msg.contains("gonzalo purge"),
            "the refusal must tell the operator both ways out, got {msg}"
        );

        // Nothing was written: the window is intact.
        let after = store.get_raw(&key).await.unwrap().expect("tombstone survives");
        assert!(after.is_tombstone());
        assert_eq!(after.body, retained, "the retained body was not touched");
    }
```

`index` returns `anyhow::Result`, so the guard's `CoreError::Invalid` arrives wrapped; `format!("{err:#}")` is what renders the full chain, which is why the assertion matches on text rather than a variant. Check how the neighbouring tests build their `src` tree and match them — if they use a helper for that, use it instead of the bare `fs::write` above.

**Already covered, do not duplicate:** "purge of a tombstone then a sweep frees the blobs" is the spec's remaining test row, and `crates/gonzalo-store-fs/tests/blob_gc.rs::purging_the_tombstone_releases_the_shards` already proves it from #327. Confirm that test still passes and say so in your report rather than writing a second one.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p gonzalo-cli purge`
Expected: FAIL — clap exits 2 with "unrecognized subcommand", because `purge` does not exist.

- [ ] **Step 3: Add the wrapper**

In `crates/gonzalo-cli/src/lib.rs`, beside the `undelete` wrapper at `:1143`:

```rust
/// Physically remove the tombstone at `namespace/collection/id`, discarding the
/// restore window it holds.
///
/// Refuses anything that is not a tombstone. [`gonzalo_core::plan_purge`] does
/// not check the record's kind — it removes whatever matches the revision — so
/// without this check an operator could physically delete a live record leaving
/// no tombstone, which a peer that had not synced since would resurrect
/// (ADR 0021). Core keeps its contract because `collect` depends on it; the
/// check belongs here, at the operator surface (ADR 0030).
pub async fn purge(
    root: &Path,
    ancestor_cap: usize,
    namespace: &str,
    collection: &str,
    id: &str,
) -> Result<Revision> {
    let store = open_store(root, ancestor_cap)?;
    let key = RecordKey::new(namespace, collection, id);
    let Some(record) = store.get_raw(&key).await? else {
        anyhow::bail!("record not found: {key}");
    };
    if !record.is_tombstone() {
        anyhow::bail!(
            "{key} is live at revision {}; purge removes tombstones only. \
             Delete it first if that is what you meant.",
            record.revision.counter
        );
    }
    let revision = record.revision.clone();
    store.purge(&key, revision.clone()).await?;
    Ok(revision)
}
```

Match the file's error convention — if the neighbouring wrappers use `anyhow::Context` rather than `bail!`, follow them, and keep the message text so the tests' `contains("live")` and `contains("not found")` assertions hold.

- [ ] **Step 4: Add the subcommand**

In `crates/gonzalo-cli/src/main.rs`, beside `Commands::Delete` (`:161-182`):

```rust
    /// Physically remove a tombstone, discarding the restore window it holds.
    /// Refuses anything that is not a tombstone. This is the deliberate way
    /// past the guard that stops a write from recreating over a deleted
    /// manifest; to get the record back instead, use `gonzalo undelete`.
    Purge {
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
    },
```

and its handler beside the `Delete` one, matching how that arm prints:

```rust
        Commands::Purge {
            root,
            namespace,
            collection,
            id,
        } => {
            let key = RecordKey::new(&namespace, &collection, &id);
            let revision = purge(&root, DEFAULT_ANCESTOR_CAP, &namespace, &collection, &id).await?;
            println!("purged: {key}");
            println!("revision: {}", serde_json::to_string(&revision)?);
        }
```

Add `purge` to the `use gonzalo_cli::{…}` list at the top of `main.rs`. Copy the `root` argument's attributes from `Commands::Delete` verbatim rather than trusting the snippet.

- [ ] **Step 5: Run the tests and the gate**

Run: `cargo test -p gonzalo-cli`, then the full gate.
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "$(printf 'feat(cli): gonzalo purge removes one tombstone deliberately (#333)\n\nThe escape hatch for the recreate guard. Refuses anything that is not a\ntombstone, because plan_purge does not check the kind and purging a live record\nwould leave a peer free to resurrect it.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 5: ADR 0030 and the docs

**Files:**
- Create: `docs/adr/0030-manifest-tombstone-recreate-guard.md`
- Modify: `docs/adr/README.md`, `docs/adr/0029-manifest-tombstone-pin.md`
- Modify: `docs/guide/src/deletion.md`, `docs/guide/src/cli.md`, `CHANGELOG.md`

- [ ] **Step 1: Write ADR 0030**

Read `docs/adr/0029-manifest-tombstone-pin.md` first and match its header lines and section order, and `docs/adr/template.md` for the Consequences bullet style. Status `accepted`, date `2026-10-03`. The body must stand alone — no "see the spec". Cover:

- **Context:** ADR 0029's restore window; `plan_put`'s recreation arm treats a tombstone as absent (correctly, under ADR 0021, because consumers never see tombstones); so any consumer create discards the retained body and unpins the blobs. Name both reaching paths and *why* they reach it — each derives `expected` from a consumer `get`, which hides the tombstone: `RecordVectorIndex::open_with_shards` at `record_index.rs:165` feeding a `Record::create` with `expected: None`, and the graph indexer in `cli/src/lib.rs`. Note that the vector commit path already anticipated recreation-over-a-tombstone for revision bookkeeping (`record_index.rs:445`) without treating it as a data hazard.
- **Decision:** the guard in `plan_put`, keyed on the stored tombstone's `deleted_kind`; both manifest kinds; `PutPlan::Rejected` reusing the existing substrate mapping to `CoreError::Invalid`; `gonzalo purge` as the escape hatch, refusing non-tombstones at the operator surface.
- **Why the chokepoint and not `open`:** the hazard is a property of the consumer write path, not one function, so guarding `open` alone would leave the graph indexer and every future writer exposed.
- **Why both kinds:** symmetry with 0029, which retains both bodies for the same reason; an asymmetric rule would need justifying forever and would leave a graph manifest's window silently destroyable.
- **Why the purge check is in the CLI, not `plan_purge`:** `collect` depends on the current contract, and `Store::purge` is documented as collection's mechanism.
- **Consequences, positive:** the restore window 0029 promises now actually holds against ordinary writes; no new wire surface, plan variant or substrate change; operators get a precise single-key purge they did not have.
- **Consequences, negative:** `gonzalo reset` followed by `gonzalo index` now fails until the operator purges or restores — friction, by design; a `put` that used to succeed now errors, which is a behaviour change for any client writing to a manifest key; the refusal message is a `&'static str` and cannot name the key; and **a `RecordVectorIndex` opened over a tombstone still opens successfully and fails only at its first commit**, because `open` keeps reading through `store.get` — nothing is lost and the failure is loud, but the error surfaces from inside a commit rather than from the call that was wrong. Cite the follow-up ticket for that by number once it is filed.
- **Answer #333's open questions explicitly:** the guard went in `plan_put` rather than `open`; both kinds are covered; `gonzalo index` needs no separate treatment because the same arm covers it.
- **Revisit if:** a non-manifest kind starts retaining a body, at which point `deleted_kind.is_some()` stops being a synonym for "a restore window exists"; operators find purge-then-create too sharp; `PutPlan::Rejected` gains a dynamic message.

- [ ] **Step 2: Annotate the amendment on both sides**

ADR 0030 **amends** 0029. 0029 is not superseded and stays `accepted`.

- `docs/adr/README.md`: add the 0030 row, and extend 0029's status cell with a back-reference in the style already used there — ADR 0012's row is the model.
- `docs/adr/0029-manifest-tombstone-pin.md`: add a one-line "Amended by" note, and **rewrite the negative bullet that currently ends "#333 tracks it"**: the recreate hazard is now guarded by 0030. Keep the `RecordVectorIndex::open` late-failure limit, pointing it at 0030 and the new follow-up ticket. Change nothing else.

- [ ] **Step 3: Update the guide and changelog**

- `docs/guide/src/deletion.md`: the guard in the delete → collect → gc story, and `gonzalo purge` with its live-record refusal and its absent-key exit code.
- `docs/guide/src/cli.md`: a `gonzalo purge` row in the Records command table, in the table's existing style.
- `CHANGELOG.md`: add the guard and the new subcommand under the unreleased heading. The refusal is a behaviour change — a `put` that used to succeed now errors — so it does not belong under `### Added` alone; follow the file's existing conventions for that. **Also amend the existing #327 entry**, which currently ends "Reopening a deleted vector index before undeleting it discards the tombstone, and so does any other `put` that creates at the key" — that sentence describes the hazard this ticket just closed and is now wrong.

- [ ] **Step 4: Validate the ADR set**

Check and report each as pass or fail, with the command you used: body/index status parity; both-sided annotation for 0029↔0030; 0030's body self-sustaining; every path and inter-ADR link 0030 cites resolving; no gaps or duplicates across 0001–0030; 0030's header lines, sections and filename conforming to the template.

- [ ] **Step 5: Run the full gate and commit**

Doc-comment and Markdown edits can still break doctests and intra-doc links, so run the whole gate.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features
git add docs CHANGELOG.md
git commit -m "$(printf 'docs(core): ADR 0030 for the manifest tombstone recreate guard (#333)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```
