# Tombstones Slice 3: S3 Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `S3Store` write tombstones on `delete_as`, hide them from `get`/`list`, expose them through `get_raw`/`list_raw`, accept replication writes through `put_raw`, physically remove them only through `purge`, and prove it with `run_tombstone_conformance` against RustFS (gonzalo#203, spec §3.3 s3 row).

**Architecture:** Every mutating method (`put`, `put_raw`, `delete_as`, `purge`) goes through one loop, `S3Store::write_planned`:
1. `GetObject` the record and its ETag.
2. Run the shared core planner against it (`plan_put` / `plan_put_raw` / `plan_delete` / `plan_purge`).
3. Do one conditional S3 write. That is `If-None-Match: *` when no object was read, and `If-Match: <etag>` whenever one was, tombstones included.

On a 412 (a concurrent writer won), the loop re-reads, re-plans and retries. After `MAX_WRITE_ATTEMPTS = 8` attempts it gives up with a `Backend` error. s3 then behaves exactly like the lock-based fs and git stores: the planner always decides against the true current record. The retry loop (`retry_on_lost_race`) and the plan-to-S3-action mapping (`put_step` / `delete_step` / `purge_step`) are pure, so they are unit-tested without an endpoint.

**Tech Stack:** Rust 2024 (MSRV 1.95), aws-sdk-s3 1.x (`rt-tokio`, `rustls`), aws-config, tokio, async-trait, serde_json. RustFS `1.0.0-beta.8` (ADR 0019) for live tests.

**Spec:** `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` (§3.2, §3.3, §3.9, §6.1, §8.4). Shared contract: `docs/superpowers/plans/2026-09-13-tombstones-00-overview.md`, with names taken from `docs/superpowers/plans/2026-09-13-tombstones-01-core-model-and-trait.md` plus the reconciled contract changes (`put_raw`, `plan_put_raw`, `delete_as`, `author` on `tombstone_of`/`plan_delete`). ADR: `docs/adr/0019-s3-backend-qualification-rustfs.md`.

## Global Constraints

- Verification gate before every push and PR, matching CI exactly, run as bare commands one per line (never joined with `&&`):
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo build --workspace --all-targets --all-features`
  - `cargo test --workspace --all-features`
- Always open a PR and merge it after CI is green. Never push to `main`.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`. PR bodies end with `https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.
- PR bodies say `Part of #203`. Only slice 6 says `Closes #203`.
- Default ancestor cap: `32` (`DEFAULT_ANCESTOR_CAP`). A cap of `0` is rejected at construction (`validate_ancestor_cap`).
- `deleted_at` unit: milliseconds since the Unix epoch, `i64`, from `gonzalo_core::now_ms()`.
- `get_raw`, `list_raw`, `put_raw`, `delete_as` and `purge` are required trait methods, and `delete` is provided (it calls `delete_as(key, expected, None)`). Stores implement `delete_as`, **not** `delete`. **Never** fall back from a raw read or write to a consumer one.
- A stored object that fails to deserialize is **still included** by consumer `list` (fs and git behave this way, and s3 must match).
- Lost-race cap: at most `8` conditional-write attempts per call. On exhaustion return exactly `CoreError::Backend(format!("s3: conditional write for {key} lost {n} consecutive races"))` with `n = 8`.
- A conformance factory returns a fresh, empty store on every call.
- Workspace lints: `unsafe_code = "forbid"`, clippy `all = warn`, promoted to errors by `-D warnings`.
- No release is tagged until slices 1–5 have all merged.
- Use the contract names exactly:
  - `plan_put(current, record, expected, cap) -> PutPlan`
  - `plan_put_raw(current, record, expected, cap) -> PutPlan`
  - `plan_delete(current, expected, now_ms, cap, author: Option<&Identity>) -> DeletePlan`
  - `plan_purge(current, &expected) -> PurgePlan`
  - `tombstone_of(current, now_ms, cap, author: Option<&Identity>)`
  - `now_ms`, `validate_ancestor_cap`, `DEFAULT_ANCESTOR_CAP`, `Record::is_tombstone`
  - `run_tombstone_conformance(factory, cap)`

## Preconditions

- Slice 1 (`2026-09-13-tombstones-01-core-model-and-trait.md`) has merged to `main`, with these interim `S3Store` impls inside `impl gonzalo_core::Store for S3Store`:
  - `get_raw` / `list_raw` delegate to `get` / `list`.
  - `put_raw` delegates to the old `put`.
  - `delete_as` holds the old physical-delete body, with the author ignored.
  - `purge` delegates to the old conditional physical delete.
- Every `Record { .. }` literal in this crate already has `ancestors: Vec::new(), deleted_at: None` (slice 1 had to add them to compile).
- Start from fresh `main` on a new branch:

```bash
git switch main
git pull --ff-only
git switch -c feat/203-s3-tombstones
```

- Line numbers below are for `crates/gonzalo-store-s3/src/lib.rs` **before slice 1**. Slice 1 shifts them (it adds interim methods inside the `Store` impl and renames `delete` to `delete_as`). Find each block by the quoted code, not by line number alone.

## Live-endpoint tests: what needs S3 and how to run it

| Test | Needs S3 | Where |
|---|---|---|
| All `#[cfg(test)] mod tests` in `src/lib.rs` (incl. the retry-loop termination tests) | **No** | `cargo test -p gonzalo-store-s3 --all-features --lib` |
| Everything in `tests/integration.rs` | **Yes**. Each test prints `skipping: ...` and passes when unconfigured | `cargo test -p gonzalo-store-s3 --all-features --test integration` |

Env vars the integration tests read. `scripts/rustfs-up.sh` prints all of them:

- `GONZALO_S3_TEST_ENDPOINT` (required; skip when unset)
- `GONZALO_S3_TEST_BUCKET` (required; skip when unset)
- `GONZALO_S3_TEST_REGION` (optional; used as the SDK region. RustFS exports `us-east-1`)
- `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` (RustFS: `rustfsadmin` / `rustfsadmin`)

Run RustFS locally (needs Docker; this is the ADR 0019-qualified backend):

```bash
eval "$(scripts/rustfs-up.sh)"
cargo test -p gonzalo-store-s3 --all-features --test integration -- --nocapture
scripts/rustfs-down.sh --purge
```

`ci.yml` provisions no S3, so without a change these tests always skip in CI. Task 4 adds a step to `.github/workflows/ha-soak.yml`, which already starts RustFS and triggers on `crates/gonzalo-store-s3/**`, so the s3 conformance really runs on this PR.

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/gonzalo-store-s3/src/lib.rs` | Modify | `cap` and `with_ancestor_cap`; `precondition` keyed on ETag presence; pure `visible`, `listed_as_live`, `Planned`, `put_step` / `delete_step` / `purge_step`, `Step`, `retry_on_lost_race`; S3 write helpers and the `write_planned` loop; the `Store` impl on the planners; unit tests |
| `crates/gonzalo-store-s3/tests/integration.rs` | Modify | fresh-bucket factory; region passthrough; tombstone conformance at default cap and cap 3; two live race tests |
| `.github/workflows/ha-soak.yml` | Modify | run the s3 integration tests against the RustFS it already starts |

No soak or integration-test changes. `crates/gonzalo-soak` and `crates/gonzalo-integration-tests` never call `S3Store::delete`. The soak's only `delete` impls are its own mock stores (`crates/gonzalo-soak/src/dispatch.rs:140`, `:259`), which slice 1 already migrated. The soak workload (`crates/gonzalo-soak/src/workload.rs`) calls only `get` / `put` through gonzalod, so the new per-key `GetObject` in `list` doesn't touch it.

---

### Task 1: Ancestor cap builder and pure decision logic

**Files:**
- Modify: `crates/gonzalo-store-s3/src/lib.rs:4-10` (imports), `:16-29` (struct + `new`), `:103-113` (`precondition`), `:115-119` (after `is_precondition_failed`: new pure items), `:166` (interim `put` call site), `:466-486` (tests)

**Interfaces:**
- Consumes (slice 1): `gonzalo_core::{DEFAULT_ANCESTOR_CAP, validate_ancestor_cap, PutPlan, DeletePlan, PurgePlan, plan_put, tombstone_of, Record::is_tombstone}`
- Produces (used by Task 3):
  - `pub fn S3Store::with_ancestor_cap(self, cap: usize) -> gonzalo_core::Result<S3Store>`; private field `S3Store::cap: usize`
  - `fn precondition(etag: Option<&str>) -> Precondition` (**signature change**: the `expected` parameter is removed)
  - `fn visible(record: Option<Record>) -> Option<Record>`
  - `fn listed_as_live(read: Result<Option<Record>>) -> Result<bool>`
  - `enum Planned<T> { Finish(T), Put(Record, T), Remove(T) }`
  - `fn put_step(key: &RecordKey, plan: PutPlan) -> Result<Planned<PutResult>>`
  - `fn delete_step(plan: DeletePlan) -> Planned<DeleteResult>`
  - `fn purge_step(plan: PurgePlan) -> Planned<DeleteResult>`
  - `const MAX_WRITE_ATTEMPTS: usize = 8`
  - `enum Step<T> { Done(T), Retry }`
  - `async fn retry_on_lost_race<T, F, Fut>(key: &RecordKey, attempt: F) -> Result<T> where F: FnMut() -> Fut, Fut: Future<Output = Result<Step<T>>>`

Why `precondition` has to change: today it is chosen from `expected` (`lib.rs:108-113`):

```rust
fn precondition(expected: &Option<Revision>, etag: Option<&str>) -> Precondition {
    match (expected, etag) {
        (Some(_), Some(tag)) => Precondition::IfMatch(tag.to_string()),
        _ => Precondition::IfAbsent,
    }
}
```

Recreation is `put(record, None)` over a stored **tombstone**. The object exists, so `If-None-Match: *` would get a 412 every time, and with the retry loop that means 8 wasted attempts and then an error. The right precondition depends only on whether an object was read. For the interim `put` this change is behaviour-neutral: that code only reaches the write when `current_rev == expected`, so `expected.is_some()` and "an ETag was read" are always the same.

Why a retry loop instead of mapping the 412 to an outcome: fs and git decide under a lock, so their planner always sees the true current record. Re-planning after a 412 gives s3 the same guarantee. For example, `delete(key, None)` that loses to a concurrent edit re-plans against the edited record and tombstones it, instead of reporting a `Conflict` that the lock-based stores would never produce. The attempt cap stops a pathologically hot key from spinning forever.

Clippy will warn that `cap` and the new pure items are unused (dead code) until Task 3 wires them in. That is expected mid-slice. Don't add `#[allow]` attributes, because Task 3 removes the warnings and the gate runs in Task 5.

- [ ] **Step 1: Write the failing unit tests**

In `crates/gonzalo-store-s3/src/lib.rs`, replace the test block at `:466-486`, which is currently:

```rust
    fn rev() -> Revision {
        Revision::initial(b"x")
    }

    #[test]
    fn create_uses_if_absent() {
        // expected = None → create-only, regardless of any etag.
        assert_eq!(precondition(&None, None), Precondition::IfAbsent);
        assert_eq!(
            precondition(&None, Some("\"etag\"")),
            Precondition::IfAbsent
        );
    }

    #[test]
    fn update_uses_if_match_on_the_read_etag() {
        assert_eq!(
            precondition(&Some(rev()), Some("\"abc123\"")),
            Precondition::IfMatch("\"abc123\"".to_string())
        );
    }
```

with:

```rust
    use gonzalo_core::store::Conflict;
    use gonzalo_core::{Body, Identity, Meta, RecordKind, plan_put, tombstone_of};
    use std::cell::Cell;
    use std::collections::BTreeMap;

    fn live(key: &RecordKey, payload: &[u8]) -> Record {
        Record {
            key: key.clone(),
            kind: RecordKind::Topic,
            revision: Revision::initial(payload),
            parent: None,
            body: Body::Inline(payload.to_vec()),
            meta: Meta {
                author: Identity::new("tester"),
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

    fn tomb(key: &RecordKey, payload: &[u8]) -> Record {
        tombstone_of(&live(key, payload), 1_000, DEFAULT_ANCESTOR_CAP, None)
    }

    fn conflict(key: &RecordKey) -> Box<Conflict> {
        Box::new(Conflict {
            key: key.clone(),
            expected: Some(Revision::initial(b"stale")),
            current: live(key, b"current"),
        })
    }

    /// An `S3Store` that never touches the network: building a client does no
    /// I/O, so the builder can be unit-tested without an endpoint.
    fn offline_store() -> S3Store {
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        S3Store::new(Client::from_conf(conf), "offline")
    }

    // ---- preconditions ----

    #[test]
    fn create_uses_if_absent() {
        // No object was read → create-only.
        assert_eq!(precondition(None), Precondition::IfAbsent);
    }

    #[test]
    fn update_uses_if_match_on_the_read_etag() {
        assert_eq!(
            precondition(Some("\"abc123\"")),
            Precondition::IfMatch("\"abc123\"".to_string())
        );
    }

    #[test]
    fn recreation_over_tombstone_uses_if_match() {
        // `put(record, None)` over a tombstone is a recreation the planner
        // writes. The tombstone object exists, so the write must be `If-Match`
        // on its ETag. `If-None-Match: *` would 412 on every attempt.
        let k = RecordKey::new("ns", "col", "recreate");
        let plan = plan_put(Some(&tomb(&k, b"old")), live(&k, b"new"), None, DEFAULT_ANCESTOR_CAP);
        assert!(matches!(plan, PutPlan::Write(_)));
        assert_eq!(
            precondition(Some("\"tomb-etag\"")),
            Precondition::IfMatch("\"tomb-etag\"".to_string())
        );
    }

    // ---- consumer read filtering ----

    #[test]
    fn visible_hides_tombstones_and_keeps_live() {
        let k = RecordKey::new("ns", "col", "vis");
        assert_eq!(visible(None), None);
        assert_eq!(visible(Some(tomb(&k, b"x"))), None);
        let rec = live(&k, b"x");
        assert_eq!(visible(Some(rec.clone())), Some(rec));
    }

    #[test]
    fn list_filter_excludes_tombstones_and_vanished_keys() {
        let k = RecordKey::new("ns", "col", "listed");
        assert!(listed_as_live(Ok(Some(live(&k, b"x")))).unwrap());
        assert!(!listed_as_live(Ok(Some(tomb(&k, b"x")))).unwrap());
        // Purged between the listing and the read.
        assert!(!listed_as_live(Ok(None)).unwrap());
    }

    #[test]
    fn list_filter_keeps_undecodable_objects_and_propagates_backend_errors() {
        // An object that fails to decode stays listed, as on fs and git. `get`
        // on it is what surfaces the error.
        assert!(listed_as_live(Err(CoreError::Serde("bad json".into()))).unwrap());
        assert!(matches!(
            listed_as_live(Err(CoreError::Backend("503".into()))),
            Err(CoreError::Backend(_))
        ));
    }

    // ---- plan → S3 action ----

    #[test]
    fn put_step_writes_and_commits_the_planned_revision() {
        let k = RecordKey::new("ns", "col", "put-write");
        let rec = live(&k, b"v");
        match put_step(&k, PutPlan::Write(rec.clone())) {
            Ok(Planned::Put(stored, answer)) => {
                assert_eq!(answer, PutResult::Committed(rec.revision.clone()));
                assert_eq!(stored, rec);
            }
            other => panic!("expected Put, got {other:?}"),
        }
    }

    #[test]
    fn put_step_finishes_on_conflict() {
        let k = RecordKey::new("ns", "col", "put-conflict");
        match put_step(&k, PutPlan::Conflict(conflict(&k))) {
            Ok(Planned::Finish(PutResult::Conflict(c))) => assert_eq!(c, conflict(&k)),
            other => panic!("expected Finish(Conflict), got {other:?}"),
        }
    }

    #[test]
    fn put_step_maps_not_found_to_error() {
        let k = RecordKey::new("ns", "col", "put-missing");
        match put_step(&k, PutPlan::NotFound) {
            Err(CoreError::NotFound(got)) => assert_eq!(got, k),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn delete_step_maps_each_plan() {
        let k = RecordKey::new("ns", "col", "del");
        let t = tomb(&k, b"x");
        assert_eq!(
            delete_step(DeletePlan::Write(t.clone())),
            Planned::Put(t, DeleteResult::Deleted)
        );
        assert_eq!(
            delete_step(DeletePlan::Noop),
            Planned::Finish(DeleteResult::Deleted)
        );
        assert_eq!(
            delete_step(DeletePlan::Conflict(conflict(&k))),
            Planned::Finish(DeleteResult::Conflict(conflict(&k)))
        );
    }

    #[test]
    fn purge_step_maps_each_plan() {
        let k = RecordKey::new("ns", "col", "purge");
        assert_eq!(
            purge_step(PurgePlan::Remove),
            Planned::Remove(DeleteResult::Deleted)
        );
        assert_eq!(
            purge_step(PurgePlan::Noop),
            Planned::Finish(DeleteResult::Deleted)
        );
        assert_eq!(
            purge_step(PurgePlan::Conflict(conflict(&k))),
            Planned::Finish(DeleteResult::Conflict(conflict(&k)))
        );
    }

    // ---- lost-race retry loop ----

    #[tokio::test]
    async fn retry_gives_up_after_max_attempts() {
        let k = RecordKey::new("ns", "col", "hot");
        let calls = Cell::new(0usize);
        let calls_ref = &calls;
        let out: Result<()> = retry_on_lost_race(&k, move || async move {
            calls_ref.set(calls_ref.get() + 1);
            Ok(Step::Retry)
        })
        .await;
        assert_eq!(calls.get(), MAX_WRITE_ATTEMPTS);
        assert_eq!(MAX_WRITE_ATTEMPTS, 8);
        match out {
            Err(CoreError::Backend(msg)) => assert_eq!(
                msg,
                format!("s3: conditional write for {k} lost 8 consecutive races")
            ),
            other => panic!("expected Backend error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_returns_as_soon_as_an_attempt_lands() {
        // Losing every attempt but the last still succeeds, with no extra call.
        let k = RecordKey::new("ns", "col", "warm");
        let calls = Cell::new(0usize);
        let calls_ref = &calls;
        let out = retry_on_lost_race(&k, move || async move {
            calls_ref.set(calls_ref.get() + 1);
            if calls_ref.get() < MAX_WRITE_ATTEMPTS {
                Ok(Step::Retry)
            } else {
                Ok(Step::Done("landed"))
            }
        })
        .await;
        assert_eq!(out.unwrap(), "landed");
        assert_eq!(calls.get(), MAX_WRITE_ATTEMPTS);
    }

    #[tokio::test]
    async fn retry_propagates_errors_without_retrying() {
        let k = RecordKey::new("ns", "col", "broken");
        let calls = Cell::new(0usize);
        let calls_ref = &calls;
        let out: Result<()> = retry_on_lost_race(&k, move || async move {
            calls_ref.set(calls_ref.get() + 1);
            Err(CoreError::Backend("access denied".into()))
        })
        .await;
        assert!(matches!(out, Err(CoreError::Backend(m)) if m == "access denied"));
        assert_eq!(calls.get(), 1);
    }

    // ---- ancestor cap ----

    #[tokio::test]
    async fn new_store_uses_default_ancestor_cap() {
        assert_eq!(offline_store().cap, DEFAULT_ANCESTOR_CAP);
    }

    #[tokio::test]
    async fn with_ancestor_cap_sets_cap() {
        match offline_store().with_ancestor_cap(3) {
            Ok(store) => assert_eq!(store.cap, 3),
            Err(e) => panic!("cap 3 must be accepted: {e}"),
        }
    }

    #[tokio::test]
    async fn with_ancestor_cap_rejects_zero() {
        assert!(matches!(
            offline_store().with_ancestor_cap(0),
            Err(CoreError::Backend(_))
        ));
    }
```

(`S3Store` doesn't implement `Debug`, so the cap tests use `match` / `matches!` instead of `unwrap_err`.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-store-s3 --all-features --lib`
Expected: compile FAIL. Errors include `this function takes 2 arguments but 1 argument was supplied` (`precondition`), `cannot find function 'visible'`, `cannot find function 'listed_as_live'`, `cannot find type 'Planned'`, `cannot find function 'retry_on_lost_race'`, `no field 'cap' on type 'S3Store'`, `no method named 'with_ancestor_cap'`.

- [ ] **Step 3: Implement the cap builder**

Replace the imports at `lib.rs:4-10`:

```rust
use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DeleteResult, KeyPrefix, PutResult, Record, RecordKey,
    Result, Revision, decode_segment, object_key, store::Conflict,
};
```

with:

```rust
use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DEFAULT_ANCESTOR_CAP, DeletePlan, DeleteResult, KeyPrefix,
    PurgePlan, PutPlan, PutResult, Record, RecordKey, Result, Revision, decode_segment, object_key,
    store::Conflict, validate_ancestor_cap,
};
```

(`store::Conflict` is still used by the interim `put` until Task 3 removes it.)

Replace the struct and `new` at `lib.rs:16-29`:

```rust
pub struct S3Store {
    client: Client,
    bucket: String,
}

impl S3Store {
    /// Build a store from an explicit client and bucket. Use
    /// [`S3Store::connect`] for the common env/endpoint path.
    pub fn new(client: Client, bucket: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
        }
    }
```

with:

```rust
pub struct S3Store {
    client: Client,
    bucket: String,
    /// Maximum `Record::ancestors` length kept on every committed write
    /// (spec §3.9). Defaults to [`DEFAULT_ANCESTOR_CAP`].
    cap: usize,
}

impl S3Store {
    /// Build a store from an explicit client and bucket. Use
    /// [`S3Store::connect`] for the common env/endpoint path.
    pub fn new(client: Client, bucket: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
            cap: DEFAULT_ANCESTOR_CAP,
        }
    }

    /// Override the ancestor cap (spec §3.9). A cap of `0` is rejected.
    pub fn with_ancestor_cap(mut self, cap: usize) -> Result<Self> {
        self.cap = validate_ancestor_cap(cap)?;
        Ok(self)
    }
```

(`connect` at `:35-50` ends in `Self::new(client, bucket)`, so it picks up the default cap without any change.)

- [ ] **Step 4: Implement `precondition` on ETag presence**

Replace `lib.rs:103-113`:

```rust
/// Map `(expected, etag)` to the write precondition. A create (`expected =
/// None`) requires the object to still be absent; an update (`expected =
/// Some`, so the object was read with an `etag`) requires that exact ETag. The
/// business-level OCC check runs first, so the `Some`-without-etag case can't
/// reach here; `IfAbsent` is a safe total default for it.
fn precondition(expected: &Option<Revision>, etag: Option<&str>) -> Precondition {
    match (expected, etag) {
        (Some(_), Some(tag)) => Precondition::IfMatch(tag.to_string()),
        _ => Precondition::IfAbsent,
    }
}
```

with:

```rust
/// Map the ETag read for the object to the write precondition. If an object
/// was read (a live record *or a tombstone*), the write must replace exactly
/// that version (`If-Match`). If none was read, it must still be absent
/// (`If-None-Match: *`). This depends on what's stored, not on the caller's
/// `expected`: recreation is `put(_, None)` over an existing tombstone object,
/// so keying off `expected` would pick `IfAbsent` and fail with 412 every time.
fn precondition(etag: Option<&str>) -> Precondition {
    match etag {
        Some(tag) => Precondition::IfMatch(tag.to_string()),
        None => Precondition::IfAbsent,
    }
}
```

Update the single interim call site in `put` (`lib.rs:166`, still the interim body until Task 3):

```rust
        req = match precondition(&expected, current.as_ref().map(|(_, tag)| tag.as_str())) {
```

to:

```rust
        req = match precondition(current.as_ref().map(|(_, tag)| tag.as_str())) {
```

- [ ] **Step 5: Implement the pure filtering, mapping and retry items**

Insert directly after `is_precondition_failed` (`lib.rs:115-119`):

```rust
/// Consumer view of a raw read: a tombstone reads as absent (spec §3.2).
fn visible(record: Option<Record>) -> Option<Record> {
    record.filter(|r| !r.is_tombstone())
}

/// Whether consumer `list` includes a key, given the raw read of its object.
/// Tombstones and keys purged since the listing are excluded. An object that
/// fails to decode stays listed, matching fs and git (and s3 before tombstones,
/// whose `list` never read objects): `get` on that key surfaces the error.
/// Any other read error propagates.
fn listed_as_live(read: Result<Option<Record>>) -> Result<bool> {
    match read {
        Ok(record) => Ok(visible(record).is_some()),
        Err(CoreError::Serde(_)) => Ok(true),
        Err(e) => Err(e),
    }
}

/// One planner decision, translated into the S3 action that carries it out
/// and the answer to return once that action lands.
#[derive(Debug, PartialEq, Eq)]
enum Planned<T> {
    /// No write needed; return this answer.
    Finish(T),
    /// `PutObject` this record under [`precondition`], then return the answer.
    Put(Record, T),
    /// `DeleteObject` with `If-Match` on the read ETag, then return the answer.
    Remove(T),
}

/// Translate a [`PutPlan`] (from `plan_put` or `plan_put_raw`).
fn put_step(key: &RecordKey, plan: PutPlan) -> Result<Planned<PutResult>> {
    match plan {
        PutPlan::Write(record) => {
            let committed = PutResult::Committed(record.revision.clone());
            Ok(Planned::Put(record, committed))
        }
        PutPlan::Conflict(conflict) => Ok(Planned::Finish(PutResult::Conflict(conflict))),
        PutPlan::NotFound => Err(CoreError::NotFound(key.clone())),
        // Only consumer `plan_put` produces this, for a `RecordKind::Tombstone`
        // record: deletes go through `delete_as`, replication through `put_raw`.
        PutPlan::Rejected(reason) => Err(CoreError::Backend(reason.to_string())),
    }
}

/// Translate a [`DeletePlan`]: a tombstone is written with `PutObject`.
fn delete_step(plan: DeletePlan) -> Planned<DeleteResult> {
    match plan {
        DeletePlan::Write(tombstone) => Planned::Put(tombstone, DeleteResult::Deleted),
        DeletePlan::Noop => Planned::Finish(DeleteResult::Deleted),
        DeletePlan::Conflict(conflict) => Planned::Finish(DeleteResult::Conflict(conflict)),
    }
}

/// Translate a [`PurgePlan`]: the only plan that physically removes an object.
fn purge_step(plan: PurgePlan) -> Planned<DeleteResult> {
    match plan {
        PurgePlan::Remove => Planned::Remove(DeleteResult::Deleted),
        PurgePlan::Noop => Planned::Finish(DeleteResult::Deleted),
        PurgePlan::Conflict(conflict) => Planned::Finish(DeleteResult::Conflict(conflict)),
    }
}

/// Most read → plan → conditional-write attempts one call makes before giving
/// up on a key that other writers keep changing underneath it.
const MAX_WRITE_ATTEMPTS: usize = 8;

/// Result of one attempt: finished with an answer, or lost the race (412) and
/// must re-read and re-plan.
#[derive(Debug, PartialEq, Eq)]
enum Step<T> {
    Done(T),
    Retry,
}

/// Run `attempt` until it finishes, at most [`MAX_WRITE_ATTEMPTS`] times. An
/// error from an attempt ends the loop immediately. Each retry re-reads and
/// re-plans, so the planner always decides against the true current record,
/// just as it does under the fs and git stores' locks.
async fn retry_on_lost_race<T, F, Fut>(key: &RecordKey, mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Step<T>>>,
{
    for _ in 0..MAX_WRITE_ATTEMPTS {
        if let Step::Done(value) = attempt().await? {
            return Ok(value);
        }
    }
    Err(CoreError::Backend(format!(
        "s3: conditional write for {key} lost {MAX_WRITE_ATTEMPTS} consecutive races"
    )))
}
```

- [ ] **Step 6: Run the unit tests to verify they pass**

Run: `cargo test -p gonzalo-store-s3 --all-features --lib`
Expected: PASS. The output ends `test result: ok. 24 passed; 0 failed` (7 pre-existing tests left untouched, plus the 17 in Step 1). Dead-code warnings for `cap`, `visible`, `listed_as_live`, `Planned`, the `*_step` functions and `retry_on_lost_race` are expected until Task 3.

- [ ] **Step 7: Commit**

```bash
git add crates/gonzalo-store-s3/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(store-s3): ancestor cap, tombstone plan mapping, lost-race retry loop (#203)

precondition() now keys on whether an object was read, so recreating over
a tombstone uses If-Match. Pure helpers translate the core planners into
S3 actions. retry_on_lost_race re-plans after a 412, up to 8 attempts.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

---

### Task 2: Live harness with fresh buckets and tombstone conformance (red)

**Files:**
- Modify: `crates/gonzalo-store-s3/tests/integration.rs` (whole file, 166 lines)

**Interfaces:**
- Consumes: `gonzalo_core::conformance::{run_store_conformance, run_tombstone_conformance}`, `gonzalo_core::DEFAULT_ANCESTOR_CAP`, `S3Store::new`, `S3Store::connect`, `S3Store::with_ancestor_cap` (Task 1), `Store::get_raw` / `Store::delete` (slice 1)
- Produces: `async fn fresh_bucket_store(endpoint: &str) -> S3Store`, `fn test_region() -> Option<String>` (test-local)

Why fresh buckets: a conformance factory must return a fresh, empty store on every call, but today's factory (`integration.rs:47-49`) hands out the **same** bucket every time. The tombstone cases use fixed keys and need genuinely fresh stores. For example, `delete_of_absent_key` asserts `get_raw` is `None`, and `independent_deletes_are_identical` deletes on two separate stores. A shared bucket also can't be rerun. So every conformance factory call creates a new, uniquely named bucket. Buckets are not removed afterwards, and `scripts/rustfs-down.sh --purge` drops the whole volume.

Why pass the region: `scripts/rustfs-up.sh` exports `GONZALO_S3_TEST_REGION`, not `AWS_REGION`, and every existing test here passes `region = None`. Under a pure RustFS environment (the CI step in Task 4), SigV4 signing then has no region. All tests now pass `test_region()`.

- [ ] **Step 1: Replace `tests/integration.rs` with the new harness**

Write the whole file:

```rust
use gonzalo_core::conformance::{run_store_conformance, run_tombstone_conformance};
use gonzalo_core::{
    BlobStore, Body, ContentHash, CoreError, DEFAULT_ANCESTOR_CAP, DeleteResult, Identity, Meta,
    PutResult, Record, RecordKey, RecordKind, Revision, Store,
};
use gonzalo_store_s3::S3Store;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Small cap for the second tombstone-conformance run, so cap truncation
/// (`ancestors_capped_and_ordered`) is exercised in a handful of writes.
const SMALL_CAP: usize = 3;

/// `(endpoint, bucket)` from the env, or `None` (skip) when unset.
fn test_target() -> Option<(String, String)> {
    match (
        std::env::var("GONZALO_S3_TEST_ENDPOINT"),
        std::env::var("GONZALO_S3_TEST_BUCKET"),
    ) {
        (Ok(e), Ok(b)) => Some((e, b)),
        _ => {
            eprintln!("skipping: set GONZALO_S3_TEST_ENDPOINT and GONZALO_S3_TEST_BUCKET to run");
            None
        }
    }
}

/// Optional SDK region. `scripts/rustfs-up.sh` exports `GONZALO_S3_TEST_REGION`
/// rather than `AWS_REGION`, and SigV4 signing needs one.
fn test_region() -> Option<String> {
    std::env::var("GONZALO_S3_TEST_REGION")
        .ok()
        .filter(|r| !r.trim().is_empty())
}

static BUCKET_SEQ: AtomicU64 = AtomicU64::new(0);

/// A store over a brand-new, empty bucket. A conformance factory must return a
/// fresh empty store on every call: the cases use fixed keys, assert `get_raw`
/// of an absent key is `None`, and compare deletes across two stores. One
/// shared bucket can't give that across cases or reruns. Buckets are left
/// behind; `scripts/rustfs-down.sh --purge` drops the volume.
async fn fresh_bucket_store(endpoint: &str) -> S3Store {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let bucket = format!(
        "gz-test-{}-{nanos}-{}",
        std::process::id(),
        BUCKET_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let base = aws_config::load_from_env().await;
    let mut builder = aws_sdk_s3::config::Builder::from(&base)
        .endpoint_url(endpoint)
        .force_path_style(true);
    if let Some(r) = test_region() {
        builder = builder.region(aws_sdk_s3::config::Region::new(r));
    }
    let client = aws_sdk_s3::Client::from_conf(builder.build());
    if let Err(e) = client.create_bucket().bucket(&bucket).send().await {
        panic!("create bucket {bucket}: {}", e.into_service_error());
    }
    S3Store::new(client, bucket)
}

fn sample(key: RecordKey, payload: &[u8], revision: Revision, parent: Option<Revision>) -> Record {
    let body = Body::Inline(payload.to_vec());
    Record {
        revision,
        parent,
        body,
        kind: RecordKind::Topic,
        meta: Meta {
            author: Identity::new("tester"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        ancestors: Vec::new(),
        deleted_at: None,
        key,
    }
}

/// Seed `key` in `store` and return the committed base revision.
async fn seed(store: &S3Store, key: &RecordKey) -> Revision {
    match store
        .put(sample(key.clone(), b"v1", Revision::initial(b"v1"), None), None)
        .await
        .unwrap()
    {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("a fresh bucket must accept the seed"),
    }
}

#[tokio::test]
async fn s3_store_passes_conformance_when_endpoint_configured() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let endpoint = endpoint.as_str();
    run_store_conformance(move || fresh_bucket_store(endpoint)).await;
}

/// Spec §6.1 tombstone cases at the default cap (RustFS-qualified, ADR 0019).
#[tokio::test]
async fn s3_store_passes_tombstone_conformance_at_default_cap() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let endpoint = endpoint.as_str();
    run_tombstone_conformance(move || fresh_bucket_store(endpoint), DEFAULT_ANCESTOR_CAP).await;
}

/// The same cases at a small cap, so truncation is exercised.
#[tokio::test]
async fn s3_store_passes_tombstone_conformance_at_small_cap() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let endpoint = endpoint.as_str();
    run_tombstone_conformance(
        move || async move {
            fresh_bucket_store(endpoint)
                .await
                .with_ancestor_cap(SMALL_CAP)
                .expect("a cap of 3 is valid")
        },
        SMALL_CAP,
    )
    .await;
}

/// Live coverage of the S3 `BlobStore` impl (gonzalo#62): put/get/list/delete
/// against MinIO. Self-cleaning and asserts on membership rather than a global
/// empty set, so it doesn't depend on a pristine bucket (which the shared
/// `run_blob_store_conformance` — designed for fresh-per-call stores — assumes).
#[tokio::test]
async fn s3_blob_store_put_get_list_delete() {
    let Some((endpoint, bucket)) = test_target() else {
        return;
    };
    let store = S3Store::connect(bucket, Some(endpoint), test_region()).await;

    let content = b"content-addressed slice for #62";
    let hash = store.put_blob(content).await.unwrap();
    assert_eq!(hash, ContentHash::of(content), "hash is content-addressed");

    // Round-trips.
    assert_eq!(
        store.get_blob(&hash).await.unwrap().as_deref(),
        Some(&content[..])
    );

    // Idempotent re-put yields the same hash and leaves content intact.
    assert_eq!(store.put_blob(content).await.unwrap(), hash);
    assert_eq!(
        store.get_blob(&hash).await.unwrap().as_deref(),
        Some(&content[..])
    );

    // Listed among the stored blobs.
    assert!(
        store.list_blobs().await.unwrap().contains(&hash),
        "put blob must appear in list_blobs"
    );

    // Delete removes it and is idempotent.
    store.delete_blob(&hash).await.unwrap();
    assert_eq!(store.get_blob(&hash).await.unwrap(), None);
    store.delete_blob(&hash).await.unwrap();
    assert!(!store.list_blobs().await.unwrap().contains(&hash));
}

/// The TOCTOU acceptance test for gonzalo#5: many writers that all read the
/// same `expected` revision then race to update. Native conditional writes
/// (`If-Match`) must let **exactly one** commit. Each loser's 412 re-reads and
/// re-plans against the winner's revision, which is a recoverable `Conflict`.
/// Without conditional writes the read-then-write window lets several
/// "commit" and silently clobber.
#[tokio::test]
async fn concurrent_updates_with_same_expected_let_exactly_one_win() {
    let Some((endpoint, bucket)) = test_target() else {
        return;
    };
    let key = RecordKey::new("race", "col", "one");

    // Seed the object and capture the revision every racer will hold.
    let store = S3Store::connect(bucket.clone(), Some(endpoint.clone()), test_region()).await;
    let v1 = sample(key.clone(), b"v1", Revision::initial(b"v1"), None);
    // Best-effort clean slate if a prior run left the key behind.
    let base_rev = loop {
        match store.put(v1.clone(), None).await.unwrap() {
            PutResult::Committed(rev) => break rev,
            PutResult::Conflict(c) => {
                // Overwrite whatever is there back to a known v1.
                let reset = sample(
                    key.clone(),
                    b"v1",
                    c.current.revision.next(b"v1"),
                    Some(c.current.revision.clone()),
                );
                if let PutResult::Committed(rev) =
                    store.put(reset, Some(c.current.revision)).await.unwrap()
                {
                    break rev;
                }
            }
        }
    };

    // Fan out N concurrent updaters, each holding `base_rev`.
    let n = 8;
    let mut handles = Vec::new();
    for i in 0..n {
        let (endpoint, bucket, key, base_rev) = (
            endpoint.clone(),
            bucket.clone(),
            key.clone(),
            base_rev.clone(),
        );
        handles.push(tokio::spawn(async move {
            let store = S3Store::connect(bucket, Some(endpoint), test_region()).await;
            let payload = format!("racer-{i}");
            let rec = sample(
                key,
                payload.as_bytes(),
                base_rev.next(payload.as_bytes()),
                Some(base_rev.clone()),
            );
            store.put(rec, Some(base_rev)).await.unwrap()
        }));
    }

    let mut committed = 0;
    let mut conflicts = 0;
    for h in handles {
        match h.await.unwrap() {
            PutResult::Committed(_) => committed += 1,
            PutResult::Conflict(_) => conflicts += 1,
        }
    }
    assert_eq!(
        committed, 1,
        "exactly one racer may commit (got {committed})"
    );
    assert_eq!(conflicts, n - 1, "the rest must conflict (got {conflicts})");
}

/// What one updater in the race tests got.
#[derive(Debug)]
enum PutOutcome {
    Committed,
    Conflict,
    NotFound,
}

async fn racing_update(store: Arc<S3Store>, key: RecordKey, base_rev: Revision, i: usize) -> PutOutcome {
    let payload = format!("updater-{i}");
    let rec = sample(
        key,
        payload.as_bytes(),
        base_rev.next(payload.as_bytes()),
        Some(base_rev.clone()),
    );
    match store.put(rec, Some(base_rev)).await {
        Ok(PutResult::Committed(_)) => PutOutcome::Committed,
        Ok(PutResult::Conflict(_)) => PutOutcome::Conflict,
        Err(CoreError::NotFound(_)) => PutOutcome::NotFound,
        Err(e) => panic!("unexpected put error: {e}"),
    }
}

/// Conditional deletes and updates that all hold one base revision. Exactly
/// one *kind* of write wins. If a delete wins, every deleter reports `Deleted`
/// (the rest re-plan onto the tombstone, a no-op) and every updater re-plans
/// onto the tombstone (`NotFound`). If an update wins, exactly one updater
/// commits and every deleter re-plans onto the new revision (`Conflict`). A
/// mix means the conditional tombstone write wasn't atomic.
#[tokio::test]
async fn racing_deletes_and_updates_on_one_revision_stay_atomic() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let store = Arc::new(fresh_bucket_store(&endpoint).await);
    let key = RecordKey::new("race", "col", "delete-vs-update");
    let base_rev = seed(&store, &key).await;

    let per_kind = 4;
    let mut deleters = Vec::new();
    let mut updaters = Vec::new();
    for i in 0..per_kind {
        let (s, k, b) = (store.clone(), key.clone(), base_rev.clone());
        deleters.push(tokio::spawn(async move { s.delete(&k, Some(b)).await.unwrap() }));
        let (s, k, b) = (store.clone(), key.clone(), base_rev.clone());
        updaters.push(tokio::spawn(racing_update(s, k, b, i)));
    }

    let (mut deleted, mut delete_conflicts) = (0, 0);
    for h in deleters {
        match h.await.unwrap() {
            DeleteResult::Deleted => deleted += 1,
            DeleteResult::Conflict(_) => delete_conflicts += 1,
        }
    }
    let (mut committed, mut put_conflicts, mut put_not_found) = (0, 0, 0);
    for h in updaters {
        match h.await.unwrap() {
            PutOutcome::Committed => committed += 1,
            PutOutcome::Conflict => put_conflicts += 1,
            PutOutcome::NotFound => put_not_found += 1,
        }
    }

    let raw = store
        .get_raw(&key)
        .await
        .unwrap()
        .expect("the key is still stored (tombstone or live)");
    if raw.is_tombstone() {
        assert_eq!(committed, 0, "no update may commit over a winning delete");
        assert_eq!(deleted, per_kind, "every deleter sees the key gone");
        assert_eq!(delete_conflicts, 0);
        assert_eq!(put_not_found, per_kind, "every updater sees a tombstone");
        assert_eq!(raw.revision.counter, base_rev.counter + 1);
        assert_eq!(store.get(&key).await.unwrap(), None);
    } else {
        assert_eq!(committed, 1, "exactly one updater commits");
        assert_eq!(put_conflicts, per_kind - 1);
        assert_eq!(delete_conflicts, per_kind, "every deleter conflicts");
        assert_eq!(deleted, 0);
    }
}

/// Unconditional deletes never conflict because they lost a race: a 412
/// re-plans against whatever is now current and tombstones it, as the fs and
/// git stores do under their locks. So every deleter reports `Deleted`, the
/// key always ends tombstoned, and at most one updater committed first.
#[tokio::test]
async fn unconditional_deletes_racing_updates_always_end_deleted() {
    let Some((endpoint, _bucket)) = test_target() else {
        return;
    };
    let store = Arc::new(fresh_bucket_store(&endpoint).await);
    let key = RecordKey::new("race", "col", "unconditional-delete");
    let base_rev = seed(&store, &key).await;

    let per_kind = 4;
    let mut deleters = Vec::new();
    let mut updaters = Vec::new();
    for i in 0..per_kind {
        let (s, k) = (store.clone(), key.clone());
        deleters.push(tokio::spawn(async move { s.delete(&k, None).await.unwrap() }));
        let (s, k, b) = (store.clone(), key.clone(), base_rev.clone());
        updaters.push(tokio::spawn(racing_update(s, k, b, i)));
    }

    for h in deleters {
        assert_eq!(
            h.await.unwrap(),
            DeleteResult::Deleted,
            "an unconditional delete must never conflict"
        );
    }
    let mut committed = 0;
    for h in updaters {
        if let PutOutcome::Committed = h.await.unwrap() {
            committed += 1;
        }
    }
    assert!(committed <= 1, "at most one updater commits (got {committed})");

    let raw = store
        .get_raw(&key)
        .await
        .unwrap()
        .expect("the key ends as a tombstone");
    assert!(raw.is_tombstone(), "the key must end deleted");
    // The tombstone sits on the update if one landed first, else on the seed.
    assert_eq!(raw.revision.counter, base_rev.counter + 1 + committed);
    assert_eq!(store.get(&key).await.unwrap(), None);
}
```

- [ ] **Step 2: Confirm the harness compiles and skips when unconfigured**

Run (in a shell with no `GONZALO_S3_TEST_*` set):
`cargo test -p gonzalo-store-s3 --all-features --test integration`
Expected: PASS, `test result: ok. 7 passed; 0 failed`, with `skipping: set GONZALO_S3_TEST_ENDPOINT and GONZALO_S3_TEST_BUCKET to run` on stderr (visible with `-- --nocapture`).

- [ ] **Step 3: Run against RustFS to verify the tombstone tests fail on the interim store**

```bash
eval "$(scripts/rustfs-up.sh)"
cargo test -p gonzalo-store-s3 --all-features --test integration -- --nocapture
```

Expected: FAIL.
- Both `s3_store_passes_tombstone_conformance_*` tests panic inside a `run_tombstone_conformance` case, because the interim `delete_as` physically removes the object, so the raw-read tombstone assertions fail.
- `unconditional_deletes_racing_updates_always_end_deleted` panics at `expect("the key ends as a tombstone")`.
- `racing_deletes_and_updates_on_one_revision_stay_atomic` panics at `expect("the key is still stored ...")` whenever a delete wins.
- `s3_store_passes_conformance_when_endpoint_configured`, `s3_blob_store_put_get_list_delete` and `concurrent_updates_with_same_expected_let_exactly_one_win` PASS.

Leave RustFS running for Task 3. If Docker isn't available, note it and continue. The Task 4 workflow step runs these tests on the PR.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-store-s3/tests/integration.rs
git commit -m "$(cat <<'EOF'
test(store-s3): tombstone conformance over fresh buckets (#203)

Each conformance factory call now creates its own bucket. Tombstone
conformance runs at the default cap and at cap 3, plus two live races:
conditional delete vs update, and unconditional delete vs update. The
region comes from GONZALO_S3_TEST_REGION.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

---

### Task 3: Rebuild the `Store` impl on the planners and the retry loop (green)

**Files:**
- Modify: `crates/gonzalo-store-s3/src/lib.rs` imports (Task 1 version); `:52-91` (inherent impl: add `list_keys`, `put_record_if`, `delete_record_if_match`, `write_planned`); after the Task 1 pure items (add `WriteOutcome`); `:132-294` (the whole `impl gonzalo_core::Store for S3Store` block, **including every interim method slice 1 put there**: `get_raw`, `list_raw`, `put_raw`, `delete_as`, `purge`)

**Interfaces:**
- Consumes: Task 1 (`precondition`, `visible`, `listed_as_live`, `Planned`, `put_step`, `delete_step`, `purge_step`, `Step`, `retry_on_lost_race`, `S3Store::cap`); contract `plan_put`, `plan_put_raw`, `plan_delete(.., author: Option<&Identity>)`, `plan_purge`, `now_ms`, `Identity`
- Produces: `S3Store` satisfying the contract:
  - `get` returns `None` for a tombstone.
  - `list` excludes tombstoned keys but keeps undecodable objects.
  - `put` follows `plan_put`, `put_raw` follows `plan_put_raw` (never re-stamps), `delete_as` follows `plan_delete` (the provided `delete` calls it with `author = None`), and `purge` follows `plan_purge`.
  - `get_raw` / `list_raw` are unfiltered.
  - A write that loses 8 consecutive races returns `CoreError::Backend("s3: conditional write for {key} lost 8 consecutive races")`.

Today's code being replaced, for reference:
- `get` (`:134-136`) returns `self.read(key)` unfiltered. It becomes `get_raw`, and `get` filters.
- `list` (`:194-232`) is the paginated `ListObjectsV2` loop. It moves into `S3Store::list_keys`, which is `list_raw`. `list` becomes `list_keys` plus a `GetObject` per key through `listed_as_live`.
- `put` (`:138-192`) does a business-level `current_rev != expected` check and a conditional `PutObject`, then on 412 does a single re-read that maps to `Conflict` / `NotFound`. The check becomes `plan_put`. The single re-read is replaced by `write_planned`'s re-read, re-plan and retry.
- `delete` (`:234-293`, renamed `delete_as` by slice 1) does an unconditional `DeleteObject` when `expected = None`, otherwise read, revision check, `DeleteObject` with `If-Match`, and on 412 a re-read. That conditional-`DeleteObject` body becomes `purge` (`plan_purge` → `Planned::Remove` → `delete_record_if_match`). `delete_as` becomes `plan_delete` → a conditional tombstone `PutObject`.

- [ ] **Step 1: Confirm the red state**

Run (RustFS env from Task 2 still exported):
`cargo test -p gonzalo-store-s3 --all-features --test integration s3_store_passes_tombstone_conformance_at_default_cap -- --nocapture`
Expected: FAIL, the same `run_tombstone_conformance` panic as Task 2 Step 3.

- [ ] **Step 2: Update the imports**

Replace the `gonzalo_core` import from Task 1:

```rust
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DEFAULT_ANCESTOR_CAP, DeletePlan, DeleteResult, KeyPrefix,
    PurgePlan, PutPlan, PutResult, Record, RecordKey, Result, Revision, decode_segment, object_key,
    store::Conflict, validate_ancestor_cap,
};
```

with (`store::Conflict` is dropped because only the test module uses it now, and it imports it itself):

```rust
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DEFAULT_ANCESTOR_CAP, DeletePlan, DeleteResult, Identity,
    KeyPrefix, PurgePlan, PutPlan, PutResult, Record, RecordKey, Result, Revision, decode_segment,
    now_ms, object_key, plan_delete, plan_purge, plan_put, plan_put_raw, validate_ancestor_cap,
};
```

- [ ] **Step 3: Add `WriteOutcome`**

Directly after `retry_on_lost_race` (added in Task 1), insert:

```rust
/// Result of one conditional S3 write: it applied, or its precondition failed
/// (412) because a concurrent writer changed the object after our read.
#[derive(Debug, PartialEq, Eq)]
enum WriteOutcome {
    Applied,
    PreconditionFailed,
}
```

- [ ] **Step 4: Add the S3 helpers and the `write_planned` loop to the inherent impl**

Inside `impl S3Store { ... }`, directly after `read_with_etag` (ends at `lib.rs:90`), insert:

```rust
    /// Every record key under `prefix`, tombstones included (the raw listing).
    /// Paginates `ListObjectsV2` off the continuation token (see
    /// [`next_continuation`]).
    async fn list_keys(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let mut s3_prefix = String::new();
        if let Some(ns) = &prefix.namespace {
            s3_prefix.push_str(&gonzalo_core::segment(ns));
            s3_prefix.push('/');
            if let Some(col) = &prefix.collection {
                s3_prefix.push_str(&gonzalo_core::segment(col));
                s3_prefix.push('/');
            }
        }
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self.client.list_objects_v2().bucket(&self.bucket);
            if !s3_prefix.is_empty() {
                req = req.prefix(&s3_prefix);
            }
            if let Some(token) = &continuation {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| CoreError::Backend(e.into_service_error().to_string()))?;
            for obj in resp.contents() {
                if let Some(k) = obj.key()
                    && let Some(key) = parse_object_key(k)
                    && prefix.matches(&key)
                {
                    out.push(key);
                }
            }
            match next_continuation(resp.is_truncated(), resp.next_continuation_token()) {
                Some(token) => continuation = Some(token),
                None => break,
            }
        }
        Ok(out)
    }

    /// Serialize `record` and `PutObject` it at its key under `pre`. A writer
    /// that changed the object after our read makes this a 412, reported as
    /// [`WriteOutcome::PreconditionFailed`] instead of clobbering its write.
    async fn put_record_if(&self, record: &Record, pre: Precondition) -> Result<WriteOutcome> {
        let bytes =
            serde_json::to_vec_pretty(record).map_err(|e| CoreError::Serde(e.to_string()))?;
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key(&record.key))
            .body(bytes.into());
        req = match pre {
            Precondition::IfAbsent => req.if_none_match("*"),
            Precondition::IfMatch(tag) => req.if_match(tag),
        };
        match req.send().await {
            Ok(_) => Ok(WriteOutcome::Applied),
            Err(e) => {
                let svc = e.into_service_error();
                if is_precondition_failed(svc.code()) {
                    Ok(WriteOutcome::PreconditionFailed)
                } else {
                    Err(CoreError::Backend(svc.to_string()))
                }
            }
        }
    }

    /// `DeleteObject` at `key` only if it still carries `etag` (`If-Match`).
    async fn delete_record_if_match(&self, key: &RecordKey, etag: String) -> Result<WriteOutcome> {
        match self
            .client
            .delete_object()
            .bucket(&self.bucket)
            .key(object_key(key))
            .if_match(etag)
            .send()
            .await
        {
            Ok(_) => Ok(WriteOutcome::Applied),
            Err(e) => {
                let svc = e.into_service_error();
                if is_precondition_failed(svc.code()) {
                    Ok(WriteOutcome::PreconditionFailed)
                } else {
                    Err(CoreError::Backend(svc.to_string()))
                }
            }
        }
    }

    /// The compare-and-swap loop behind every mutating method. Each attempt
    /// reads the object and its ETag, asks `plan` what to do given that exact
    /// record, and carries it out with a conditional write gated on the same
    /// ETag. A 412 means a concurrent writer changed the object after our read,
    /// so the next attempt re-reads and re-plans. The planner therefore always
    /// decides against the true current record, as it does under the fs and git
    /// stores' locks. Gives up after [`MAX_WRITE_ATTEMPTS`] attempts.
    async fn write_planned<T, P>(&self, key: &RecordKey, plan: P) -> Result<T>
    where
        T: Send,
        P: Fn(Option<&Record>) -> Result<Planned<T>> + Sync,
    {
        let plan = &plan;
        retry_on_lost_race(key, move || async move {
            let current = self.read_with_etag(key).await?;
            let etag = current.as_ref().map(|(_, tag)| tag.as_str());
            let (outcome, answer) = match plan(current.as_ref().map(|(rec, _)| rec))? {
                Planned::Finish(answer) => return Ok(Step::Done(answer)),
                Planned::Put(record, answer) => {
                    (self.put_record_if(&record, precondition(etag)).await?, answer)
                }
                Planned::Remove(answer) => {
                    // Planners only remove a record they were given, so an ETag
                    // was read. The empty fallback can never match; it would
                    // just 412 and re-plan.
                    let tag = etag.unwrap_or_default().to_string();
                    (self.delete_record_if_match(key, tag).await?, answer)
                }
            };
            Ok(match outcome {
                WriteOutcome::Applied => Step::Done(answer),
                WriteOutcome::PreconditionFailed => Step::Retry,
            })
        })
        .await
    }
```

- [ ] **Step 5: Replace the whole `Store` impl**

Delete the entire `#[async_trait] impl gonzalo_core::Store for S3Store { ... }` block. Before slice 1 it is `lib.rs:132-294`; after slice 1 it also holds the interim `get_raw`, `list_raw`, `put_raw`, `delete_as` and `purge`, and no `delete`. Put this in its place:

```rust
#[async_trait]
impl gonzalo_core::Store for S3Store {
    // ---- consumer surface (tombstones hidden) ----

    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        Ok(visible(self.read(key).await?))
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        // Spec §8.4: a tombstone lives at the record's normal object key, so the
        // listing alone can't tell it apart from a live record. Hiding tombstones
        // costs one GetObject per key, on top of the ListObjectsV2 pages. That's
        // expensive for large namespaces, but acceptable at current sizes and
        // tracked as a follow-up (a kind marker in the key suffix, or a
        // per-collection tombstone index; both are layout changes needing their
        // own design). Don't optimise it here.
        let mut out = Vec::new();
        for key in self.list_keys(prefix).await? {
            if listed_as_live(self.read(&key).await)? {
                out.push(key);
            }
        }
        Ok(out)
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Consumer write: a tombstone counts as absent, so `expected = None`
        // over one is a recreation that re-stamps the revision (`plan_put`).
        let key = record.key.clone();
        self.write_planned(&key, |current| {
            put_step(
                &key,
                plan_put(current, record.clone(), expected.clone(), self.cap),
            )
        })
        .await
    }

    // `delete` is the trait's provided method: `delete_as(key, expected, None)`.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        // Deletion writes a tombstone (spec §3.1). `now_ms()` is taken per
        // attempt, so a retried delete stamps when it actually landed.
        self.write_planned(key, |current| {
            Ok(delete_step(plan_delete(
                current,
                expected.clone(),
                now_ms(),
                self.cap,
                author.as_ref(),
            )))
        })
        .await
    }

    // ---- replication surface (tombstones visible) ----

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.read(key).await
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.list_keys(prefix).await
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Replication write: stores the caller's revision verbatim, never
        // re-stamps, and a tombstone is a real current record it must name in
        // `expected` (`plan_put_raw`).
        let key = record.key.clone();
        self.write_planned(&key, |current| {
            put_step(
                &key,
                plan_put_raw(current, record.clone(), expected.clone(), self.cap),
            )
        })
        .await
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        // The only physical removal: a conditional DeleteObject, and only when
        // the stored revision is still `expected`, so a recreation that lands
        // mid-purge survives.
        self.write_planned(key, |current| {
            Ok(purge_step(plan_purge(current, &expected)))
        })
        .await
    }
}
```

Also update the doc comment on `read_with_etag` (`lib.rs:56-58`):

```rust
    /// Like [`read`](Self::read) but also returns the object's S3 ETag, which
    /// [`put`](gonzalo_core::Store::put) feeds back as an `If-Match` precondition
    /// to make the compare-and-swap atomic (closing the read-then-write TOCTOU).
```

to:

```rust
    /// Like [`read`](Self::read) but also returns the object's S3 ETag, which
    /// [`write_planned`](Self::write_planned) feeds back as the write
    /// precondition to make every compare-and-swap atomic (closing the
    /// read-then-write TOCTOU). Returns tombstones: this is a raw read.
```

- [ ] **Step 6: Run the unit tests (no endpoint)**

Run: `cargo test -p gonzalo-store-s3 --all-features --lib`
Expected: PASS, `test result: ok. 24 passed; 0 failed`, and no dead-code warnings.

- [ ] **Step 7: Run the live integration tests to verify they pass**

Run (RustFS env exported):
`cargo test -p gonzalo-store-s3 --all-features --test integration -- --nocapture`
Expected: PASS, `test result: ok. 7 passed; 0 failed`, with none of them skipping. That includes slice 1's `put_raw_create_over_tombstone_conflicts`, `put_raw_never_restamps`, `delete_as_stamps_author` and `consumer_put_of_a_tombstone_is_rejected` cases inside both tombstone-conformance runs.

Then tear down:

```bash
scripts/rustfs-down.sh --purge
```

- [ ] **Step 8: Lint the crate**

Run: `cargo clippy -p gonzalo-store-s3 --all-targets --all-features -- -D warnings`
Expected: `Finished` with no warnings. If the compiler rejects `write_planned` with "captured variable cannot escape `FnMut` closure body", check that the `let plan = &plan;` rebinding and both `move` keywords are present. Every capture (`self`, `key`, `plan`) must be a shared reference, which is `Copy`.

- [ ] **Step 9: Commit**

```bash
cargo fmt --all
git add crates/gonzalo-store-s3/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(store-s3): tombstone delete_as, put_raw, filtered reads, purge (#203)

put, put_raw, delete_as and purge share write_planned: read the object
and its ETag, run the core planner, do one conditional write, and on 412
re-read and re-plan (at most 8 attempts). get and list hide tombstones;
list does a GetObject per key (spec §8.4) and keeps undecodable objects.
get_raw and list_raw are the unfiltered reads.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

---

### Task 4: Run the s3 conformance over RustFS in CI

**Files:**
- Modify: `.github/workflows/ha-soak.yml:52-56`

**Interfaces:**
- Consumes: `scripts/rustfs-up.sh` exports (`GONZALO_S3_TEST_ENDPOINT`, `GONZALO_S3_TEST_BUCKET`, `GONZALO_S3_TEST_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`), all written to `$GITHUB_ENV` by the existing step
- Produces: a CI step that fails the PR if `S3Store` fails conformance on RustFS

`ci.yml` provisions no S3, so `tests/integration.rs` has always skipped in CI. `ha-soak.yml` already starts RustFS and triggers on `crates/gonzalo-store-s3/**`. This task adds one step so slice 3 is actually RustFS-qualified (spec §7 slice 3, ADR 0019).

- [ ] **Step 1: Add the step**

In `.github/workflows/ha-soak.yml`, replace:

```yaml
      - name: Start RustFS + export the target
        run: bash scripts/rustfs-up.sh | sed 's/^export //' >> "$GITHUB_ENV"

      - name: Bounded HA soak gate
        run: cargo test -p gonzalo-soak --test ha_soak -- --nocapture
```

with:

```yaml
      - name: Start RustFS + export the target
        run: bash scripts/rustfs-up.sh | sed 's/^export //' >> "$GITHUB_ENV"

      # S3 store conformance (incl. tombstone conformance, gonzalo#203) against
      # the qualified backend (ADR 0019). Each conformance case creates its own
      # bucket, so it can't disturb the soak's bucket.
      - name: S3 store conformance over RustFS
        run: cargo test -p gonzalo-store-s3 --all-features --test integration -- --nocapture

      - name: Bounded HA soak gate
        run: cargo test -p gonzalo-soak --test ha_soak -- --nocapture
```

- [ ] **Step 2: Check the YAML parses**

Run: `ruby -ryaml -e 'YAML.load_file(".github/workflows/ha-soak.yml"); puts "ok"'`
Expected: `ok`

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ha-soak.yml
git commit -m "$(cat <<'EOF'
ci(ha-soak): run S3 store conformance over RustFS (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

---

### Task 5: Full gate, push, PR

**Files:** none changed (verification only)

**Interfaces:**
- Consumes: everything above
- Produces: an open PR against `main`, CI green, merged

- [ ] **Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: no output, exit 0. If it prints a diff, run `cargo fmt --all`, commit (`style(store-s3): cargo fmt`, ending with the `Claude-Session:` line), and re-run.

- [ ] **Step 2: Lint**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: `Finished`, zero warnings.

- [ ] **Step 3: Build**

Run: `cargo build --workspace --all-targets --all-features`
Expected: `Finished`.

- [ ] **Step 4: Test**

Run: `cargo test --workspace --all-features`
Expected: every `test result:` line is `ok`, and there are no `FAILED` lines. The s3 integration tests print `skipping: ...` unless RustFS env is exported, which is fine because Task 4's CI step runs them live.

- [ ] **Step 5: Push**

```bash
git push -u origin feat/203-s3-tombstones
```

- [ ] **Step 6: Open the PR**

```bash
gh pr create --base main --title "feat(store-s3): tombstones, put_raw, filtered reads and purge (#203 slice 3)" --body "$(cat <<'EOF'
Part of #203. Slice 3 of the replicated-deletion plan (`docs/superpowers/plans/2026-09-13-tombstones-03-s3-store.md`).

## What

- `put`, `put_raw`, `delete_as` and `purge` share one compare-and-swap loop:
  - read the object and its ETag;
  - run the core planner (`plan_put` / `plan_put_raw` / `plan_delete` / `plan_purge`);
  - do one conditional write (`If-None-Match: *` if nothing was read, `If-Match: <etag>` otherwise);
  - on 412, re-read and re-plan, at most 8 attempts, then a `Backend` error.
- As a result, s3 decides exactly like the lock-based fs and git stores. For example, an unconditional delete that loses a race tombstones the new current record instead of conflicting.
- `delete_as` writes a tombstone with a conditional `PutObject`, and `purge` is the conditional `DeleteObject`.
- `get` / `list` hide tombstones. `list` does one `GetObject` per key (spec §8.4, a known cost tracked as a follow-up) and still lists undecodable objects, like fs and git. `get_raw` / `list_raw` are unfiltered.
- `with_ancestor_cap(n)` builder (default 32, 0 rejected).

## Tests

- Unit (no endpoint): precondition choice incl. recreation, list filtering (tombstone, vanished, undecodable), plan→S3-action mapping, retry-loop termination (gives up at 8, succeeds on the 8th, errors don't retry), cap builder.
- Live (RustFS): `run_store_conformance`, `run_tombstone_conformance` at cap 32 and cap 3, each over fresh buckets; two race tests (conditional delete vs update stays atomic; unconditional delete vs update always ends deleted and never conflicts).
- `ha-soak.yml` now runs the s3 integration tests against RustFS (they always skipped in `ci`).

https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

Expected: prints the PR URL.

- [ ] **Step 7: Wait for CI and merge**

Run: `gh pr checks --watch`
Expected: all checks pass, including `ci` and `HA soak — gonzalod replicas over RustFS` (with the new `S3 store conformance over RustFS` step). Then:

```bash
gh pr merge --squash --delete-branch
```

---

## Self-Review

**Spec and contract coverage**
- §3.2 `delete` table → `plan_delete` in `delete_as` (Task 3). Live proof: `run_tombstone_conformance` (Task 2), incl. `delete_as_stamps_author`.
- §3.2 `put` over a tombstone (recreation only; `Some(_)` → `NotFound`) → `plan_put` + `precondition(etag)` (Tasks 1 and 3). Unit: `recreation_over_tombstone_uses_if_match`.
- §3.2 consumer `put` of a `RecordKind::Tombstone` record is rejected → `plan_put`'s `Rejected` arm mapped by `put_step` (Task 3). Live: `consumer_put_of_a_tombstone_is_rejected` (now part of `run_tombstone_conformance`, Task 2).
- Contract change 1, `put_raw` → `plan_put_raw` through `put_step` (Task 3). Live: `put_raw_create_over_tombstone_conflicts`, `put_raw_never_restamps`.
- §3.2 ancestor maintenance inside the critical section → planners fold against the record read under the ETag that gates the write, and each retry re-folds against the fresh read (Task 3). Live: the cap-3 run.
- §3.3 s3 row: delete = `If-Match` PutObject; get/list = GetObject per key and filter; raw = today's reads; purge = conditional DeleteObject (Task 3).
- Contract change 5: undecodable objects stay listed (`listed_as_live`, Task 1).
- Lost-race decision: re-read, re-plan, retry ≤ 8, exact error text (Task 1 unit tests; Task 2 live races).
- §3.9 `with_ancestor_cap`, 0 rejected (Task 1).
- §6.1 conformance on s3 at two caps over fresh buckets (Task 2), running in CI on RustFS (Task 4).
- §8.4 cost comment in `list`, not optimised (Task 3).

**Placeholder scan:** every code step has complete code. No TBD or "similar to" references.

**Type consistency**
- `precondition(Option<&str>)` is defined in Task 1 and called that way in Task 3's `write_planned`.
- `Planned<T>`, `Step<T>` and `retry_on_lost_race` are defined in Task 1 and consumed in Task 3.
- `put_step(&RecordKey, PutPlan) -> Result<Planned<PutResult>>` is used as the `write_planned` closure body for `put` / `put_raw`. `delete_step` / `purge_step` return a bare `Planned<DeleteResult>`, wrapped in `Ok(..)` in their closures.
- `WriteOutcome` is defined in Task 3 Step 3 and used in Step 4.
- `plan_delete` takes 5 arguments ending in `author.as_ref()`, and `tombstone_of` takes 4 ending in `None` (test helper), both per contract change 3.
- `with_ancestor_cap(self, usize) -> Result<Self>` matches the overview's per-store builder contract.
