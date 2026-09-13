# Tombstones Slice 1: Core Model, Planners and Trait — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `gonzalo-core` everything every store needs to support tombstones: the record fields, the `Tombstone` kind, pure planner functions that make put/delete/purge decisions, the new `Store` trait methods (`delete_as`, `get_raw`, `list_raw`, `put_raw`, `purge`; `delete` becomes provided), a reference `MemStore`, and the `run_tombstone_conformance` suite. The whole workspace keeps compiling and passing through interim implementations.

**Architecture:** All tombstone semantics live in one pure module, `gonzalo-core/src/tombstone.rs`. A store reads the current record inside its existing OCC critical section, calls a planner, and carries out the plan it gets back (write / no-op / conflict / not-found / remove). `MemStore` is the reference implementation built only on planners, and it passes both conformance suites. No real store changes behaviour in this slice: their `get_raw`/`list_raw` delegate to `get`/`list` (no tombstones exist yet), and `purge` delegates to today's conditional physical delete.

**Tech Stack:** Rust 2024, serde, async-trait, tokio (dev), blake3 via `ContentHash::of`.

**Spec:** `docs/superpowers/specs/2026-09-13-tombstone-replication-design.md` (§3.1, §3.2, §3.9, §6.1). **Overview and shared contract:** `docs/superpowers/plans/2026-09-13-tombstones-00-overview.md`.

## Global Constraints

- Everything in the overview's Global Constraints applies.
- Tombstone domain string: `gonzalo:tombstone:v1`. Default ancestor cap: `32`. A cap of `0` is rejected.
- New `Record` fields: `#[serde(default, skip_serializing_if = "Vec::is_empty")] ancestors` and `#[serde(default, skip_serializing_if = "Option::is_none")] deleted_at`.
- No default implementations for `delete_as` / `get_raw` / `list_raw` / `put_raw` / `purge` on the trait. `delete` is the only provided method.
- This slice must not change any observable behaviour of fs/git/s3/server stores.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/gonzalo-core/src/record.rs` | modify | `RecordKind::Tombstone`, `Record.ancestors`, `Record.deleted_at`, `Record::is_tombstone`, merge class |
| `crates/gonzalo-core/src/tombstone.rs` | **create** | constants, `tombstone_hash`, `now_ms`, `validate_ancestor_cap`, `fold_ancestors`, `tombstone_of`, `plan_put`/`plan_delete`/`plan_purge` and plan enums |
| `crates/gonzalo-core/src/memstore.rs` | **create** | reference in-memory `Store` built on the planners |
| `crates/gonzalo-core/src/store.rs` | modify | trait gains `get_raw`, `list_raw`, `purge` |
| `crates/gonzalo-core/src/lib.rs` | modify | module declarations and re-exports |
| `crates/gonzalo-core/src/conformance.rs` | modify | `run_tombstone_conformance` and its cases |
| `crates/gonzalo-core/src/ancestry.rs` | modify | `AncestryStore` pass-throughs; `Mem` double interim methods |
| `crates/gonzalo-core/src/sync.rs` | modify (tests only) | interim methods on `MemStore`, `FlakyOnceStore`, `AlwaysRacyStore` |
| `crates/gonzalo-knowledge/src/lib.rs:273` | modify | `Tombstone` joins the not-indexable arm |
| `crates/gonzalo-store-{fs,git,s3,server}/src/lib.rs` | modify | interim raw/purge methods |
| `crates/gonzalo-server/src/{http,grpc}.rs` (tests) | modify | `DownStore` interim methods |
| `crates/gonzalo-soak/src/dispatch.rs` (tests) | modify | `MockStore`, `Conflicter` interim methods |
| `crates/gonzalo-ticket/src/ingest.rs` (tests) | modify | `ConflictStore` interim methods |
| every file with a `Record { .. }` literal (24 files, 49 literals) | modify | add the two new fields |

---

### Task 1: Record fields and the `Tombstone` kind

**Files:**
- Modify: `crates/gonzalo-core/src/record.rs:9-21` (enum), `:41-50` (`merge_class`), `:106-115` (struct), tests module at `:117`
- Modify: `crates/gonzalo-knowledge/src/lib.rs:273`
- Modify: every `Record { .. }` struct literal in the workspace (compiler-driven, Step 5)

**Interfaces:**
- Consumes: nothing
- Produces: `RecordKind::Tombstone` (serializes as `"Tombstone"`, `merge_class() == MergeClass::Opaque`), `Record.ancestors: Vec<Revision>`, `Record.deleted_at: Option<i64>`, `Record::is_tombstone(&self) -> bool`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `crates/gonzalo-core/src/record.rs`:

```rust
    fn plain_record() -> Record {
        let body = Body::Inline(b"hello".to_vec());
        Record {
            key: RecordKey::new("ns", "col", "id"),
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

    #[test]
    fn tombstone_kind_is_opaque_and_detected() {
        assert_eq!(RecordKind::Tombstone.merge_class(), MergeClass::Opaque);
        let mut r = plain_record();
        assert!(!r.is_tombstone());
        r.kind = RecordKind::Tombstone;
        assert!(r.is_tombstone());
    }

    #[test]
    fn tombstone_kind_serializes_as_its_name() {
        assert_eq!(
            serde_json::to_string(&RecordKind::Tombstone).unwrap(),
            "\"Tombstone\""
        );
    }

    #[test]
    fn empty_new_fields_are_omitted_from_json() {
        let json = serde_json::to_value(plain_record()).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("ancestors"));
        assert!(!obj.contains_key("deleted_at"));
    }

    #[test]
    fn legacy_json_without_new_fields_deserializes() {
        let mut json = serde_json::to_value(plain_record()).unwrap();
        let obj = json.as_object_mut().unwrap();
        obj.remove("ancestors");
        obj.remove("deleted_at");
        let back: Record = serde_json::from_value(json).unwrap();
        assert_eq!(back, plain_record());
    }

    #[test]
    fn populated_new_fields_roundtrip() {
        let mut r = plain_record();
        r.kind = RecordKind::Tombstone;
        r.ancestors = vec![Revision::initial(b"a"), Revision::initial(b"b")];
        r.deleted_at = Some(1_700_000_000_000);
        let back: Record = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }
```

Check that `serde_json` is available to core's unit tests: `rg -n serde_json crates/gonzalo-core/Cargo.toml`. It is a normal dependency (sync.rs and ancestry use it); if it is not listed, add `serde_json = { workspace = true }` under `[dependencies]`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core --all-features record::tests`
Expected: FAIL to compile, with `no variant named Tombstone`, `struct Record has no field named ancestors`, and `no method named is_tombstone`.

- [ ] **Step 3: Implement the model changes**

In `crates/gonzalo-core/src/record.rs`, add the variant as the last entry of `RecordKind`:

```rust
    /// A per-view code-graph manifest: `(repo, view_id) -> { path -> content_hash }`.
    /// Regenerable from source; reconciled last-writer-wins. See ADR 0012.
    GraphManifest,
    /// A deletion marker. Hidden from consumer reads (`get`/`list`); replicated
    /// by sync and pull through raw reads; physically removed only by
    /// `Store::purge`. See ADR 0021.
    Tombstone,
}
```

Extend `merge_class`:

```rust
            RecordKind::Checkpoint => MergeClass::Opaque,
            RecordKind::GraphManifest => MergeClass::Derived,
            // Sync and pull reconcile tombstones before any body merge runs;
            // the most conservative class guards a path that forgets to.
            RecordKind::Tombstone => MergeClass::Opaque,
```

Replace the `Record` struct and add the impl:

```rust
/// The universal persisted unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub key: RecordKey,
    pub kind: RecordKind,
    pub revision: Revision,
    pub parent: Option<Revision>,
    pub body: Body,
    pub meta: Meta,
    pub links: Vec<RecordKey>,
    /// Recent revisions this record descends from, newest first, bounded by the
    /// store's ancestor cap. Advisory: a missing or truncated list makes sync
    /// report a conflict instead of guessing. Empty on legacy records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<Revision>,
    /// Tombstones only: when the delete happened, in ms since the Unix epoch.
    /// Stamped by the store. `None` means collection never purges it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
}

impl Record {
    /// Whether this record is a deletion marker.
    pub fn is_tombstone(&self) -> bool {
        self.kind == RecordKind::Tombstone
    }
}
```

In `crates/gonzalo-knowledge/src/lib.rs`, extend the not-indexable arm at line 273:

```rust
        // A tombstone has no content; consumer reads never surface one, so
        // this arm is belt-and-braces (ADR 0021).
        RecordKind::Checkpoint | RecordKind::GraphManifest | RecordKind::Tombstone => {
            return Ok(None);
        }
```

- [ ] **Step 4: Run the record tests to verify they pass**

Run: `cargo test -p gonzalo-core --all-features record::tests`
Expected: PASS. The core crate itself may still fail to compile because of literals elsewhere in core; if so, do Step 5 for `gonzalo-core` first and then re-run.

- [ ] **Step 5: Add the new fields to every `Record` literal (compiler-driven)**

Run: `cargo build --workspace --all-targets --all-features 2>&1 | rg -A3 'E0063'`

Every `error[E0063]: missing fields ancestors and deleted_at in initializer of Record` names a file and line. At each one, add these two lines as the last fields of the literal:

```rust
            ancestors: Vec::new(),
            deleted_at: None,
```

Repeat the build until no `E0063` remains. Known locations (from `rg -c 'Record \{$'`), for cross-checking: `gonzalo-soak/src/oracle.rs` (6), `gonzalo-core/src/sync.rs` (4), `gonzalo-soak/src/workload.rs` (3), and 2 each in `gonzalo-store-server/src/lib.rs`, `gonzalo-store-s3/tests/integration.rs`, `gonzalo-store-git/tests/put_and_push.rs`, `gonzalo-store-git/tests/pull.rs`, `gonzalo-store-git/src/lib.rs`, `gonzalo-store-fs/tests/{list_ignores_stray_files,concurrent_put_no_lost_update,blob_store}.rs`, `gonzalo-soak/src/dispatch.rs`, `gonzalo-server/src/grpc.rs`, `gonzalo-mcp/src/lib.rs`, `gonzalo-knowledge/src/lib.rs`, `gonzalo-core/src/conformance.rs`, `gonzalo-core/src/ancestry.rs`, `gonzalo-cli/src/lib.rs`; 1 each in `gonzalo/src/lib.rs`, `gonzalo-ticket/src/ingest.rs`, `gonzalo-server/src/{service,http}.rs`, `gonzalo-integration-tests/tests/graph_http.rs`. Some of these counts include non-literal matches such as `pub struct Record {`; the compiler is authoritative.

Do **not** change sync's `build_merged` or git's `merged_record` beyond adding empty fields. Folding ancestors there is slice 5.

If the build instead reports `E0027` (a pattern `Record { .. }` that lists fields without `..`), add `ancestors: _, deleted_at: _` to that pattern.

- [ ] **Step 6: Build and test the workspace**

Run: `cargo build --workspace --all-targets --all-features`
Expected: builds with no errors.

Run: `cargo test --workspace --all-features`
Expected: PASS. Serialized records don't change because empty fields are omitted, so no fixture diffs.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(core): add Tombstone kind and ancestors/deleted_at record fields (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 2: Tombstone primitives: hash, clock, cap, ancestor fold, tombstone construction

**Files:**
- Create: `crates/gonzalo-core/src/tombstone.rs`
- Modify: `crates/gonzalo-core/src/lib.rs` (after the `record` re-export at line 13)

**Interfaces:**
- Consumes: `Record`, `RecordKind::Tombstone`, `Record::is_tombstone` (Task 1); `ContentHash::of`, `Revision`, `CoreError::Backend`, `Result`
- Produces:
  - `pub const DEFAULT_ANCESTOR_CAP: usize = 32;`
  - `pub const TOMBSTONE_DOMAIN: &[u8] = b"gonzalo:tombstone:v1";`
  - `pub fn tombstone_hash() -> ContentHash`
  - `pub fn now_ms() -> i64`
  - `pub fn validate_ancestor_cap(cap: usize) -> Result<usize>`
  - `pub fn fold_ancestors(stored: &Revision, incoming: &[Revision], current: Option<&Record>, cap: usize) -> Vec<Revision>`
  - `pub fn tombstone_of(current: &Record, now_ms: i64, cap: usize, author: Option<&Identity>) -> Record`

- [ ] **Step 1: Write the failing tests**

Create `crates/gonzalo-core/src/tombstone.rs` with only the test module for now:

```rust
//! Tombstone semantics shared by every [`Store`](crate::Store) implementation
//! (ADR 0021). Stores read the current record inside their own OCC critical
//! section, ask a planner here what to do, and carry out the returned plan, so
//! every substrate reaches identical decisions by construction.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Body, Identity, Meta, RecordKey, RecordKind};
    use std::collections::BTreeMap;

    fn rev(counter: u64, body: &[u8]) -> Revision {
        Revision {
            counter,
            hash: ContentHash::of(body),
        }
    }

    fn live(counter: u64, body: &[u8], ancestors: Vec<Revision>) -> Record {
        Record {
            key: RecordKey::new("ns", "col", "id"),
            kind: RecordKind::Topic,
            revision: rev(counter, body),
            parent: None,
            body: Body::Inline(body.to_vec()),
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

    #[test]
    fn tombstone_hash_is_domain_separated() {
        assert_eq!(tombstone_hash(), ContentHash::of(b"gonzalo:tombstone:v1"));
        assert_ne!(tombstone_hash(), ContentHash::of(b""));
    }

    #[test]
    fn now_ms_is_after_2020() {
        assert!(now_ms() > 1_577_836_800_000);
    }

    #[test]
    fn zero_cap_is_rejected() {
        assert!(validate_ancestor_cap(0).is_err());
        assert_eq!(validate_ancestor_cap(1).unwrap(), 1);
        assert_eq!(validate_ancestor_cap(DEFAULT_ANCESTOR_CAP).unwrap(), 32);
    }

    #[test]
    fn fold_with_nothing_is_empty() {
        assert!(fold_ancestors(&rev(0, b"a"), &[], None, 32).is_empty());
    }

    #[test]
    fn fold_adds_current_and_its_ancestors_newest_first() {
        let cur = live(2, b"c", vec![rev(1, b"b"), rev(0, b"a")]);
        let got = fold_ancestors(&rev(3, b"d"), &[], Some(&cur), 32);
        assert_eq!(got, vec![rev(2, b"c"), rev(1, b"b"), rev(0, b"a")]);
    }

    #[test]
    fn fold_dedups_incoming_and_current() {
        let cur = live(1, b"b", vec![rev(0, b"a")]);
        let incoming = [rev(1, b"b"), rev(0, b"a")];
        let got = fold_ancestors(&rev(2, b"c"), &incoming, Some(&cur), 32);
        assert_eq!(got, vec![rev(1, b"b"), rev(0, b"a")]);
    }

    #[test]
    fn fold_excludes_the_stored_revision() {
        let cur = live(4, b"t", vec![rev(3, b"x")]);
        // Writing the same revision back (sync winner onto the side holding it).
        let got = fold_ancestors(&rev(4, b"t"), &[rev(3, b"y")], Some(&cur), 32);
        assert!(!got.contains(&rev(4, b"t")));
        // Equal counters order by hash, descending.
        let mut expected = vec![rev(3, b"x"), rev(3, b"y")];
        expected.sort_by(|a, b| b.hash.cmp(&a.hash));
        assert_eq!(got, expected);
    }

    #[test]
    fn fold_truncates_to_cap_keeping_newest() {
        let ancestors: Vec<Revision> = (0..10).rev().map(|i| rev(i, &[i as u8])).collect();
        let cur = live(10, b"z", ancestors);
        let got = fold_ancestors(&rev(11, b"n"), &[], Some(&cur), 3);
        assert_eq!(got, vec![rev(10, b"z"), rev(9, &[9]), rev(8, &[8])]);
    }

    #[test]
    fn tombstone_of_follows_the_spec_table() {
        let cur = live(5, b"body", vec![rev(4, b"prev")]);
        let t = tombstone_of(&cur, 1_234, 32, None);
        assert_eq!(t.key, cur.key);
        assert_eq!(t.kind, RecordKind::Tombstone);
        assert_eq!(t.revision, Revision { counter: 6, hash: tombstone_hash() });
        assert_eq!(t.parent, Some(cur.revision.clone()));
        assert_eq!(t.body, Body::Inline(Vec::new()));
        assert_eq!(t.ancestors, vec![cur.revision.clone(), rev(4, b"prev")]);
        assert_eq!(t.deleted_at, Some(1_234));
        assert_eq!(t.meta, cur.meta);
        assert!(t.links.is_empty());
    }

    #[test]
    fn tombstone_of_stamps_the_deleter_when_given() {
        let cur = live(5, b"body", vec![]);
        let deleter = Identity::new("deleter");
        let t = tombstone_of(&cur, 1, 32, Some(&deleter));
        assert_eq!(t.meta.author, deleter);
        assert_eq!(t.meta.origin_system, cur.meta.origin_system);
    }

    #[test]
    fn independent_tombstones_of_the_same_revision_are_identical() {
        let cur = live(5, b"body", vec![]);
        assert_eq!(tombstone_of(&cur, 1, 32, None).revision, tombstone_of(&cur, 999, 32, None).revision);
    }

    #[test]
    fn tombstone_never_equals_an_empty_body_edit() {
        let cur = live(5, b"body", vec![]);
        assert_ne!(tombstone_of(&cur, 1, 32, None).revision, cur.revision.next(b""));
    }
}
```

`fold_excludes_the_stored_revision` uses two helpers that don't exist, so replace that test body with the explicit ordering before running. Two revisions with equal counters sort by hash, descending:

```rust
    #[test]
    fn fold_excludes_the_stored_revision() {
        let cur = live(4, b"t", vec![rev(3, b"x")]);
        // Writing the same revision back (sync winner onto the side holding it).
        let got = fold_ancestors(&rev(4, b"t"), &[rev(3, b"y")], Some(&cur), 32);
        assert!(!got.contains(&rev(4, b"t")));
        let mut expected = vec![rev(3, b"x"), rev(3, b"y")];
        expected.sort_by(|a, b| b.hash.cmp(&a.hash));
        assert_eq!(got, expected);
    }
```

Register the module in `crates/gonzalo-core/src/lib.rs` after line 13:

```rust
pub mod tombstone;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core --all-features tombstone::tests`
Expected: FAIL to compile with `cannot find function tombstone_hash` (and similar for the other functions).

- [ ] **Step 3: Implement the primitives**

Insert above `#[cfg(test)]` in `crates/gonzalo-core/src/tombstone.rs`:

```rust
use crate::{Body, ContentHash, CoreError, Identity, Record, RecordKind, Result, Revision};

/// Ancestor list length used when a store is not configured otherwise.
/// About 90 bytes per serialized entry: ~2.9 KB at the cap.
pub const DEFAULT_ANCESTOR_CAP: usize = 32;

/// Domain string hashed to form every tombstone's revision hash. Not the empty
/// body's hash: that would let a tombstone collide with a live record edited to
/// an empty body at the same counter, and sync would miss the divergence.
pub const TOMBSTONE_DOMAIN: &[u8] = b"gonzalo:tombstone:v1";

/// The revision hash shared by every tombstone.
pub fn tombstone_hash() -> ContentHash {
    ContentHash::of(TOMBSTONE_DOMAIN)
}

/// Wall-clock milliseconds since the Unix epoch. A clock set before the epoch
/// yields `i64::MAX`: collection treats a future stamp as too young, so a
/// broken clock can only delay purging a tombstone, never make it early
/// (spec §3.8).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(i64::MAX)
}

/// Reject an ancestor cap of zero: with no ancestors, sync could never
/// fast-forward and every divergence would surface as a conflict.
pub fn validate_ancestor_cap(cap: usize) -> Result<usize> {
    if cap == 0 {
        return Err(CoreError::Backend("ancestor cap must be at least 1".into()));
    }
    Ok(cap)
}

/// The ancestor list to persist for a record whose revision is `stored`,
/// replacing `current` (if any), given the ancestors the writer supplied.
///
/// `dedup(incoming ∪ {current.revision} ∪ current.ancestors) − {stored}`,
/// sorted by `(counter desc, hash desc)` and truncated to `cap`. The order is
/// total, so every substrate persists byte-identical lists.
pub fn fold_ancestors(
    stored: &Revision,
    incoming: &[Revision],
    current: Option<&Record>,
    cap: usize,
) -> Vec<Revision> {
    let from_current = current
        .into_iter()
        .flat_map(|c| std::iter::once(&c.revision).chain(c.ancestors.iter()));
    let mut folded: Vec<Revision> = Vec::new();
    for r in incoming.iter().chain(from_current) {
        if r != stored && !folded.contains(r) {
            folded.push(r.clone());
        }
    }
    folded.sort_by(|a, b| b.counter.cmp(&a.counter).then_with(|| b.hash.cmp(&a.hash)));
    folded.truncate(cap);
    folded
}

/// The tombstone that deleting the live record `current` produces. `author`,
/// when given, is the deleter and replaces `meta.author`; otherwise the
/// tombstone keeps the last live writer's metadata.
pub fn tombstone_of(
    current: &Record,
    now_ms: i64,
    cap: usize,
    author: Option<&Identity>,
) -> Record {
    let revision = Revision {
        counter: current.revision.counter + 1,
        hash: tombstone_hash(),
    };
    let ancestors = fold_ancestors(&revision, &[], Some(current), cap);
    let mut meta = current.meta.clone();
    if let Some(author) = author {
        meta.author = author.clone();
    }
    Record {
        key: current.key.clone(),
        kind: RecordKind::Tombstone,
        revision,
        parent: Some(current.revision.clone()),
        body: Body::Inline(Vec::new()),
        meta,
        links: Vec::new(),
        ancestors,
        deleted_at: Some(now_ms),
    }
}
```

Add the re-export in `crates/gonzalo-core/src/lib.rs` directly after `pub mod tombstone;`:

```rust
pub use tombstone::{
    DEFAULT_ANCESTOR_CAP, TOMBSTONE_DOMAIN, fold_ancestors, now_ms, tombstone_hash, tombstone_of,
    validate_ancestor_cap,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core --all-features tombstone::tests`
Expected: PASS (11 tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/gonzalo-core/src/tombstone.rs crates/gonzalo-core/src/lib.rs
git commit -m "feat(core): tombstone hash, ancestor fold and tombstone construction (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 3: Planners for put, delete and purge

**Files:**
- Modify: `crates/gonzalo-core/src/tombstone.rs`
- Modify: `crates/gonzalo-core/src/lib.rs` (extend the `tombstone` re-export)

**Interfaces:**
- Consumes: `fold_ancestors`, `tombstone_of`, `tombstone_hash` (Task 2); `crate::store::Conflict`
- Produces:
  - `pub enum PutPlan { Write(Record), Conflict(Box<Conflict>), NotFound }`
  - `pub fn plan_put(current: Option<&Record>, record: Record, expected: Option<Revision>, cap: usize) -> PutPlan`
  - `pub fn plan_put_raw(current: Option<&Record>, record: Record, expected: Option<Revision>, cap: usize) -> PutPlan`
  - `pub enum DeletePlan { Write(Record), Noop, Conflict(Box<Conflict>) }`
  - `pub fn plan_delete(current: Option<&Record>, expected: Option<Revision>, now_ms: i64, cap: usize, author: Option<&Identity>) -> DeletePlan`
  - `pub enum PurgePlan { Remove, Noop, Conflict(Box<Conflict>) }`
  - `pub fn plan_purge(current: Option<&Record>, expected: &Revision) -> PurgePlan`
  - All three enums derive `Clone, Debug, PartialEq, Eq`.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `crates/gonzalo-core/src/tombstone.rs`:

```rust
    fn tomb(counter: u64) -> Record {
        let prior = live(counter - 1, b"was", vec![]);
        tombstone_of(&prior, 42, 32, None)
    }

    // ---- plan_put ----

    #[test]
    fn put_create_on_absent_writes() {
        let rec = live(0, b"new", vec![]);
        assert_eq!(plan_put(None, rec.clone(), None, 32), PutPlan::Write(rec));
    }

    #[test]
    fn put_expected_on_absent_is_not_found() {
        let rec = live(0, b"new", vec![]);
        assert_eq!(plan_put(None, rec, Some(rev(0, b"x")), 32), PutPlan::NotFound);
    }

    #[test]
    fn put_update_with_matching_expected_folds_ancestors() {
        let cur = live(0, b"v0", vec![]);
        let mut next = live(1, b"v1", vec![]);
        next.parent = Some(cur.revision.clone());
        let PutPlan::Write(stored) = plan_put(Some(&cur), next.clone(), Some(cur.revision.clone()), 32) else {
            panic!("expected Write");
        };
        assert_eq!(stored.revision, next.revision);
        assert_eq!(stored.ancestors, vec![cur.revision.clone()]);
    }

    #[test]
    fn put_over_live_with_wrong_or_no_expected_conflicts() {
        let cur = live(0, b"v0", vec![]);
        for expected in [None, Some(rev(9, b"nope"))] {
            match plan_put(Some(&cur), live(1, b"v1", vec![]), expected.clone(), 32) {
                PutPlan::Conflict(c) => {
                    assert_eq!(c.current, cur);
                    assert_eq!(c.expected, expected);
                }
                other => panic!("expected Conflict, got {other:?}"),
            }
        }
    }

    #[test]
    fn put_none_over_tombstone_recreates_continuing_the_chain() {
        let t = tomb(3);
        let fresh = live(0, b"again", vec![]);
        let PutPlan::Write(stored) = plan_put(Some(&t), fresh, None, 32) else {
            panic!("expected Write");
        };
        assert_eq!(stored.revision, rev(t.revision.counter + 1, b"again"));
        assert_eq!(stored.parent, Some(t.revision.clone()));
        assert_eq!(stored.deleted_at, None);
        assert_eq!(stored.ancestors.first(), Some(&t.revision));
        assert_eq!(stored.kind, RecordKind::Topic);
    }

    #[test]
    fn put_none_tombstone_over_tombstone_keeps_the_tombstone_hash() {
        let t = tomb(3);
        let mut incoming = t.clone();
        incoming.revision = rev(0, b"");
        let PutPlan::Write(stored) = plan_put(Some(&t), incoming, None, 32) else {
            panic!("expected Write");
        };
        assert_eq!(stored.revision.hash, tombstone_hash());
    }

    #[test]
    fn consumer_put_with_any_expected_over_tombstone_is_not_found() {
        let t = tomb(3);
        // Even the tombstone's own revision: replication uses plan_put_raw.
        for expected in [rev(1, b"stale"), t.revision.clone()] {
            assert_eq!(
                plan_put(Some(&t), live(0, b"x", vec![]), Some(expected), 32),
                PutPlan::NotFound
            );
        }
    }

    // ---- plan_put_raw ----

    #[test]
    fn put_raw_create_on_absent_writes_verbatim_including_tombstones() {
        let t = tomb(3);
        assert_eq!(plan_put_raw(None, t.clone(), None, 32), PutPlan::Write(t));
    }

    #[test]
    fn put_raw_expected_on_absent_is_not_found() {
        assert_eq!(
            plan_put_raw(None, live(0, b"x", vec![]), Some(rev(0, b"x")), 32),
            PutPlan::NotFound
        );
    }

    #[test]
    fn put_raw_create_over_tombstone_conflicts_carrying_it() {
        let t = tomb(3);
        match plan_put_raw(Some(&t), live(0, b"copy", vec![]), None, 32) {
            PutPlan::Conflict(c) => {
                assert!(c.current.is_tombstone());
                assert_eq!(c.current, t);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn put_raw_overwrite_of_tombstone_never_restamps() {
        let t = tomb(3);
        let mut incoming = live(9, b"peer", vec![t.revision.clone()]);
        incoming.parent = Some(t.revision.clone());
        let PutPlan::Write(stored) =
            plan_put_raw(Some(&t), incoming.clone(), Some(t.revision.clone()), 32)
        else {
            panic!("expected Write");
        };
        assert_eq!(stored.revision, incoming.revision, "revision stored unchanged");
        assert_eq!(stored.ancestors.first(), Some(&t.revision));
    }

    #[test]
    fn put_raw_over_live_with_wrong_expected_conflicts() {
        let cur = live(0, b"v0", vec![]);
        assert!(matches!(
            plan_put_raw(Some(&cur), live(1, b"v1", vec![]), Some(rev(7, b"no")), 32),
            PutPlan::Conflict(_)
        ));
    }

    // ---- plan_delete ----

    #[test]
    fn delete_absent_is_noop() {
        assert_eq!(plan_delete(None, None, 1, 32, None), DeletePlan::Noop);
        assert_eq!(plan_delete(None, Some(rev(0, b"x")), 1, 32, None), DeletePlan::Noop);
    }

    #[test]
    fn delete_tombstone_is_noop() {
        let t = tomb(2);
        assert_eq!(plan_delete(Some(&t), None, 1, 32, None), DeletePlan::Noop);
        assert_eq!(plan_delete(Some(&t), Some(rev(7, b"x")), 1, 32, None), DeletePlan::Noop);
    }

    #[test]
    fn delete_live_writes_its_tombstone() {
        let cur = live(4, b"v", vec![]);
        let expected_tomb = tombstone_of(&cur, 77, 32, None);
        assert_eq!(plan_delete(Some(&cur), None, 77, 32, None), DeletePlan::Write(expected_tomb.clone()));
        assert_eq!(
            plan_delete(Some(&cur), Some(cur.revision.clone()), 77, 32, None),
            DeletePlan::Write(expected_tomb)
        );
    }

    #[test]
    fn delete_live_with_stale_expected_conflicts() {
        let cur = live(4, b"v", vec![]);
        match plan_delete(Some(&cur), Some(rev(1, b"old")), 77, 32, None) {
            DeletePlan::Conflict(c) => assert_eq!(c.current, cur),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    // ---- plan_purge ----

    #[test]
    fn purge_absent_is_noop() {
        assert_eq!(plan_purge(None, &rev(0, b"x")), PurgePlan::Noop);
    }

    #[test]
    fn purge_matching_revision_removes() {
        let t = tomb(2);
        assert_eq!(plan_purge(Some(&t), &t.revision), PurgePlan::Remove);
    }

    #[test]
    fn purge_after_recreation_conflicts() {
        let t = tomb(2);
        let PutPlan::Write(recreated) = plan_put(Some(&t), live(0, b"back", vec![]), None, 32) else {
            panic!("expected Write");
        };
        match plan_purge(Some(&recreated), &t.revision) {
            PurgePlan::Conflict(c) => {
                assert_eq!(c.current, recreated);
                assert_eq!(c.expected, Some(t.revision.clone()));
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core --all-features tombstone::tests`
Expected: FAIL to compile with `cannot find type PutPlan` / `cannot find function plan_put`.

- [ ] **Step 3: Implement the planners**

Extend the `use` line at the top of `tombstone.rs`:

```rust
use crate::{
    Body, ContentHash, CoreError, Identity, Record, RecordKind, Result, Revision, store::Conflict,
};
```

Append above `#[cfg(test)]`:

```rust
/// What a store must do for a `put`. Decided with the store's current record
/// in hand, inside its OCC critical section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PutPlan {
    /// Persist exactly this record, then return
    /// `PutResult::Committed(record.revision)`. On recreation the revision was
    /// re-stamped; ancestors are always already folded.
    Write(Record),
    /// Persist nothing; return `PutResult::Conflict`.
    Conflict(Box<Conflict>),
    /// Persist nothing; return `Err(CoreError::NotFound(key))`.
    NotFound,
}

/// Decide a consumer `put` (spec §3.2). A tombstone counts as absent: a create
/// (`expected == None`) recreates the key past the tombstone, and any
/// `Some(_)` is `NotFound`. Replication writes use [`plan_put_raw`].
pub fn plan_put(
    current: Option<&Record>,
    mut record: Record,
    expected: Option<Revision>,
    cap: usize,
) -> PutPlan {
    match current {
        None => {
            if expected.is_some() {
                return PutPlan::NotFound;
            }
            record.ancestors = fold_ancestors(&record.revision, &record.ancestors, None, cap);
            PutPlan::Write(record)
        }
        Some(t) if t.is_tombstone() => match expected {
            None => {
                // Recreation: continue the chain past the tombstone so the new
                // record is never ordered before the delete it follows.
                let hash = if record.is_tombstone() {
                    tombstone_hash()
                } else {
                    ContentHash::of(record.body.bytes())
                };
                record.revision = Revision {
                    counter: t.revision.counter + 1,
                    hash,
                };
                record.parent = Some(t.revision.clone());
                if !record.is_tombstone() {
                    record.deleted_at = None;
                }
                record.ancestors = fold_ancestors(&record.revision, &record.ancestors, Some(t), cap);
                PutPlan::Write(record)
            }
            // Consumers never learn a tombstone's revision, so any `Some` here
            // is stale; replication writes over tombstones use `plan_put_raw`.
            Some(_) => PutPlan::NotFound,
        },
        Some(c) => {
            if expected.as_ref() == Some(&c.revision) {
                record.ancestors = fold_ancestors(&record.revision, &record.ancestors, Some(c), cap);
                PutPlan::Write(record)
            } else {
                PutPlan::Conflict(Box::new(Conflict {
                    key: record.key.clone(),
                    expected,
                    current: c.clone(),
                }))
            }
        }
    }
}

/// Decide a replication write (`Store::put_raw`, spec §3.2). Never re-stamps:
/// a tombstone is an ordinary record here. A create that finds anything stored
/// (live or tombstone) is a `Conflict` carrying it, so sync re-reads instead of
/// turning a copy into a recreation that would resurrect a deleted record.
pub fn plan_put_raw(
    current: Option<&Record>,
    mut record: Record,
    expected: Option<Revision>,
    cap: usize,
) -> PutPlan {
    match (current, expected) {
        (None, None) => {
            record.ancestors = fold_ancestors(&record.revision, &record.ancestors, None, cap);
            PutPlan::Write(record)
        }
        (None, Some(_)) => PutPlan::NotFound,
        (Some(c), Some(e)) if e == c.revision => {
            record.ancestors = fold_ancestors(&record.revision, &record.ancestors, Some(c), cap);
            PutPlan::Write(record)
        }
        (Some(c), expected) => PutPlan::Conflict(Box::new(Conflict {
            key: record.key.clone(),
            expected,
            current: c.clone(),
        })),
    }
}

/// What a store must do for a `delete`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeletePlan {
    /// Persist this tombstone, then return `DeleteResult::Deleted`.
    Write(Record),
    /// Persist nothing; return `DeleteResult::Deleted`.
    Noop,
    /// Persist nothing; return `DeleteResult::Conflict`.
    Conflict(Box<Conflict>),
}

/// Decide a conditional `delete` (spec §3.2). Deleting an absent key writes
/// nothing: there is no revision to order a tombstone against a peer's copy.
/// Deleting a tombstone writes nothing: the chain does not advance.
pub fn plan_delete(
    current: Option<&Record>,
    expected: Option<Revision>,
    now_ms: i64,
    cap: usize,
    author: Option<&Identity>,
) -> DeletePlan {
    match current {
        None => DeletePlan::Noop,
        Some(c) if c.is_tombstone() => DeletePlan::Noop,
        Some(c) if expected.is_none() || expected.as_ref() == Some(&c.revision) => {
            DeletePlan::Write(tombstone_of(c, now_ms, cap, author))
        }
        Some(c) => DeletePlan::Conflict(Box::new(Conflict {
            key: c.key.clone(),
            expected,
            current: c.clone(),
        })),
    }
}

/// What a store must do for a `purge`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PurgePlan {
    /// Physically remove the stored record; return `DeleteResult::Deleted`.
    Remove,
    /// Nothing is stored; return `DeleteResult::Deleted`.
    Noop,
    /// Remove nothing; return `DeleteResult::Conflict`.
    Conflict(Box<Conflict>),
}

/// Decide a `purge`: remove only the exact revision named, so a key recreated
/// after its tombstone was listed survives collection.
pub fn plan_purge(current: Option<&Record>, expected: &Revision) -> PurgePlan {
    match current {
        None => PurgePlan::Noop,
        Some(c) if &c.revision == expected => PurgePlan::Remove,
        Some(c) => PurgePlan::Conflict(Box::new(Conflict {
            key: c.key.clone(),
            expected: Some(expected.clone()),
            current: c.clone(),
        })),
    }
}
```

Replace the `tombstone` re-export in `crates/gonzalo-core/src/lib.rs`:

```rust
pub use tombstone::{
    DEFAULT_ANCESTOR_CAP, DeletePlan, PurgePlan, PutPlan, TOMBSTONE_DOMAIN, fold_ancestors,
    now_ms, plan_delete, plan_purge, plan_put, plan_put_raw, tombstone_hash, tombstone_of,
    validate_ancestor_cap,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core --all-features tombstone::tests`
Expected: PASS (30 tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/gonzalo-core/src/tombstone.rs crates/gonzalo-core/src/lib.rs
git commit -m "feat(core): put/delete/purge planners for tombstone semantics (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 4: `Store` trait methods and interim implementations

**Files:**
- Modify: `crates/gonzalo-core/src/store.rs:40-64`
- Modify: `crates/gonzalo-core/src/ancestry.rs:38-64` (`AncestryStore`), `:82-132` (`Mem` test double)
- Modify: `crates/gonzalo-core/src/sync.rs` test doubles at `:224`, `:287`, `:360`
- Modify: `crates/gonzalo-store-fs/src/lib.rs` (impl ends `:78`), `crates/gonzalo-store-git/src/lib.rs` (impl ends `:608`), `crates/gonzalo-store-s3/src/lib.rs` (impl ends `:294`), `crates/gonzalo-store-server/src/lib.rs` (impl ends `:297`)
- Modify: `crates/gonzalo-server/src/http.rs:505-522`, `crates/gonzalo-server/src/grpc.rs:563-595`, `crates/gonzalo-soak/src/dispatch.rs:116-148` and `:244-262`, `crates/gonzalo-ticket/src/ingest.rs:283-317`

**Interfaces:**
- Consumes: nothing new
- Produces: required `Store::delete_as(&self, &RecordKey, Option<Revision>, Option<Identity>) -> Result<DeleteResult>`, `Store::get_raw(&self, &RecordKey) -> Result<Option<Record>>`, `Store::list_raw(&self, &KeyPrefix) -> Result<Vec<RecordKey>>`, `Store::put_raw(&self, Record, Option<Revision>) -> Result<PutResult>`, `Store::purge(&self, &RecordKey, Revision) -> Result<DeleteResult>`; provided `Store::delete(&self, &RecordKey, Option<Revision>) -> Result<DeleteResult>` calling `delete_as(.., None)`

This task is compiler-driven. Adding required methods breaks every implementation, and the test that proves it is done is a green build plus the existing suite.

- [ ] **Step 1: Add the trait methods**

In `crates/gonzalo-core/src/store.rs`, replace the `delete` declaration and its doc comment (`store.rs:54-63`) with a required `delete_as` and a provided `delete`:

```rust
    /// Conditionally delete the record at `key`. `expected` is the revision the
    /// caller believes is current: `None` deletes unconditionally; `Some(rev)`
    /// deletes only if the current revision matches, else
    /// `DeleteResult::Conflict`. Deleting an absent key is a no-op `Deleted`.
    /// `Some(author)` records who deleted (stores that write tombstones stamp
    /// it on the tombstone's `meta.author`). See ADR 0018 and ADR 0021.
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult>;

    /// [`delete_as`](Store::delete_as) without an author. Provided; stores
    /// implement `delete_as`.
    async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult> {
        self.delete_as(key, expected, None).await
    }
```

Add `Identity` to the `use crate::{..}` line at `store.rs:3`. Then append these methods after `delete`:

```rust

    /// Like [`get`](Store::get), but also returns tombstones. For replication
    /// (sync, pull, collection) only: consumers must use `get`. A store must
    /// never implement this by calling `get`, which would hide the deletions
    /// replication exists to carry. See ADR 0021.
    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>>;

    /// Like [`list`](Store::list), but also includes tombstoned keys. For
    /// replication only. See ADR 0021.
    async fn list_raw(&self, prefix: &crate::KeyPrefix) -> Result<Vec<RecordKey>>;

    /// Replication write: store `record` exactly as given, never re-stamping.
    /// A create (`expected == None`) that finds anything stored, tombstones
    /// included, is a `Conflict` carrying it. Sync and pull use this; consumers
    /// must use `put`. A store must never implement this by calling `put`,
    /// whose recreation rule would resurrect records mid-sync. See ADR 0021.
    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult>;

    /// Physically remove the record at `key` only if its current revision is
    /// `expected`; otherwise `DeleteResult::Conflict`. Absent is an idempotent
    /// `Deleted`. The only physical removal in the system: used by tombstone
    /// collection, which must pass the tombstone's revision so a record
    /// recreated in the meantime survives. See ADR 0021.
    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult>;
```

- [ ] **Step 2: Confirm which implementations are now broken**

Run: `cargo build --workspace --all-targets --all-features 2>&1 | rg 'E0046' -A2`
Expected: `not all trait items implemented, missing: delete_as, get_raw, list_raw, put_raw, purge` for each of the 14 implementations listed in Files.

- [ ] **Step 2b: Rename `delete` to `delete_as` and add an interim `put_raw` in every implementation except `AncestryStore`**

The trait now provides `delete`, so implementations provide `delete_as` instead. Step 3 handles `AncestryStore`. In each of the other 13 implementations (4 real stores, 9 test doubles):

1. Rename `async fn delete(` to `async fn delete_as(` and add a last parameter `_author: Option<Identity>`. Leave the body unchanged: no store writes tombstones yet, so there is nothing to attribute. Import `Identity` (`gonzalo_core::Identity`, or `crate::Identity` inside `gonzalo-core`) if the file doesn't already.
2. Add an interim `put_raw` that delegates to the implementation's own `put`. No store holds tombstones yet, so `put` never re-stamps, and this has replication semantics until each store's tombstone slice replaces it:

```rust
    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        <Self as Store>::put(self, record, expected).await
    }
```

Use each file's own spellings. In `gonzalo-server/src/http.rs` the return type is `CoreResult<PutResult>`. In `gonzalo-server/src/grpc.rs` and `gonzalo-ticket/src/ingest.rs` it is `gonzalo_core::Result<PutResult>`. Where the impl is written `impl gonzalo_core::Store for ...` (git, s3, ingest), use `<Self as gonzalo_core::Store>::put`. The `DownStore` doubles may return the same `Err` as their `put` instead of delegating; either is fine.

The interim `purge` bodies in Steps 4–6 call `delete`, which still compiles because the trait provides it.

- [ ] **Step 3: `AncestryStore` pass-throughs**

In `crates/gonzalo-core/src/ancestry.rs`, replace `AncestryStore`'s `delete` (`ancestry.rs:58-63`) with a `delete_as` that forwards the author:

```rust
    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        // Retained ancestry blobs stay: they may back a later divergence's
        // 3-way merge (ADR 0016).
        self.inner.delete_as(key, expected, author).await
    }
```

Add `Identity` to the `use crate::{..}` block at `ancestry.rs:7-9`. Then append inside `impl<S: Store, B: BlobStore> Store for AncestryStore<S, B>`:

```rust

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.inner.get_raw(key).await
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.inner.list_raw(prefix).await
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Replicated bodies are retained exactly like local puts, so a later
        // divergence can still find this version as its merge base.
        let body_bytes = record.body.bytes().to_vec();
        let outcome = self.inner.put_raw(record, expected).await?;
        if matches!(outcome, PutResult::Committed(_)) {
            self.ancestry.put_blob(&body_bytes).await?;
        }
        Ok(outcome)
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        // Retained ancestry bodies stay: they are content-addressed and may
        // back other records' merges (ADR 0016).
        self.inner.purge(key, expected).await
    }
```

- [ ] **Step 4: Interim methods for the four real stores**

Append this block inside each real store's `impl Store` (`FsStore` in `gonzalo-store-fs/src/lib.rs`, `GitStore` in `gonzalo-store-git/src/lib.rs`, `S3Store` in `gonzalo-store-s3/src/lib.rs`, `ServerStore` in `gonzalo-store-server/src/lib.rs`). Each already imports `Record`, `RecordKey`, `KeyPrefix`, `Revision`, `DeleteResult` and `Result` for its existing methods. If the compiler reports a missing name, add it to that file's `gonzalo_core::{..}` import.

```rust

    // Interim (gonzalo#203 slice 1): this store does not write tombstones yet,
    // so raw reads equal consumer reads and purge is the existing conditional
    // physical delete. Replaced by the store's tombstone slice.
    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        Store::get(self, key).await
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        Store::list(self, prefix).await
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        Store::delete(self, key, Some(expected)).await
    }
```

In `gonzalo-store-git/src/lib.rs` and `gonzalo-store-s3/src/lib.rs` the impl is written `impl gonzalo_core::Store for ...`. Use `gonzalo_core::Store::get(self, key)` (and the same for `list`, `delete` and `put`) in those two files.

- [ ] **Step 5: Interim methods for core test doubles**

In `crates/gonzalo-core/src/sync.rs`, append inside each of `impl Store for MemStore`, `impl Store for FlakyOnceStore` and `impl Store for AlwaysRacyStore`:

```rust
        async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.get(key).await
        }
        async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.list(prefix).await
        }
        async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
            self.delete(key, Some(expected)).await
        }
```

In `crates/gonzalo-core/src/ancestry.rs`, append the same block inside `impl Store for Mem`.

- [ ] **Step 6: Interim methods for the other crates' test doubles**

`crates/gonzalo-server/src/http.rs`, inside `impl Store for DownStore`:

```rust
        async fn get_raw(&self, _key: &RecordKey) -> CoreResult<Option<Record>> {
            Err(CoreError::Backend("store unreachable".into()))
        }
        async fn list_raw(&self, _prefix: &KeyPrefix) -> CoreResult<Vec<RecordKey>> {
            Err(CoreError::Backend("store unreachable".into()))
        }
        async fn purge(&self, _key: &RecordKey, _expected: Revision) -> CoreResult<DeleteResult> {
            Err(CoreError::Backend("store unreachable".into()))
        }
```

`crates/gonzalo-server/src/grpc.rs`, inside `impl Store for DownStore`:

```rust
        async fn get_raw(&self, _key: &RecordKey) -> gonzalo_core::Result<Option<Record>> {
            Err(gonzalo_core::CoreError::Backend(
                "s3://secret-bucket".into(),
            ))
        }
        async fn list_raw(
            &self,
            _prefix: &gonzalo_core::KeyPrefix,
        ) -> gonzalo_core::Result<Vec<RecordKey>> {
            Err(gonzalo_core::CoreError::Backend(
                "s3://secret-bucket".into(),
            ))
        }
        async fn purge(
            &self,
            _key: &RecordKey,
            _expected: Revision,
        ) -> gonzalo_core::Result<DeleteResult> {
            Err(gonzalo_core::CoreError::Backend(
                "s3://secret-bucket".into(),
            ))
        }
```

`crates/gonzalo-soak/src/dispatch.rs`, inside `impl Store for MockStore`:

```rust
        async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.get(key).await
        }
        async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.list(prefix).await
        }
        async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
            self.delete(key, Some(expected)).await
        }
```

The same file, inside `impl Store for Conflicter` in `conflict_is_returned_not_retried`:

```rust
            async fn get_raw(&self, _k: &RecordKey) -> Result<Option<Record>> {
                Ok(None)
            }
            async fn list_raw(&self, _p: &KeyPrefix) -> Result<Vec<RecordKey>> {
                Ok(Vec::new())
            }
            async fn purge(&self, _k: &RecordKey, _e: Revision) -> Result<DeleteResult> {
                Ok(DeleteResult::Deleted)
            }
```

`crates/gonzalo-ticket/src/ingest.rs`, inside `impl gonzalo_core::Store for ConflictStore`:

```rust

        async fn get_raw(
            &self,
            _key: &gonzalo_core::RecordKey,
        ) -> gonzalo_core::Result<Option<Record>> {
            Ok(None)
        }

        async fn list_raw(
            &self,
            _prefix: &gonzalo_core::KeyPrefix,
        ) -> gonzalo_core::Result<Vec<gonzalo_core::RecordKey>> {
            Ok(vec![])
        }

        async fn purge(
            &self,
            _key: &gonzalo_core::RecordKey,
            _expected: Revision,
        ) -> gonzalo_core::Result<gonzalo_core::DeleteResult> {
            Ok(gonzalo_core::DeleteResult::Deleted)
        }
```

- [ ] **Step 7: Check for implementations outside the known list**

Run: `cargo build --workspace --all-targets --all-features 2>&1 | rg 'E0046' -A2`
Expected: no output. If a new implementation shows up (added to `main` after this plan was written), give it the interim block from Step 4 if it's a real store, or Step 5's if it's a test double.

- [ ] **Step 8: Run the full test suite**

Run: `cargo test --workspace --all-features`
Expected: PASS, with no behaviour change.

- [ ] **Step 9: Commit**

```bash
cargo fmt --all
git add -A
git commit -m "feat(core): add delete_as/get_raw/list_raw/put_raw/purge to Store with interim impls (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 5: Reference `MemStore` built on the planners

**Files:**
- Create: `crates/gonzalo-core/src/memstore.rs`
- Modify: `crates/gonzalo-core/src/lib.rs` (after `pub mod sync;` re-export, line 37)

**Interfaces:**
- Consumes: `plan_put`, `plan_delete`, `plan_purge`, plan enums, `DEFAULT_ANCESTOR_CAP`, `now_ms` (Tasks 2–3); the full `Store` trait (Task 4)
- Produces: `gonzalo_core::memstore::MemStore` (`#[cfg(any(test, feature = "conformance"))]`) with `new() -> Self`, `impl Default` (same as `new`), `with_ancestor_cap(self, usize) -> Self` (panics on 0), `with_clock(self, i64) -> Self`, `raw_snapshot(&self) -> BTreeMap<RecordKey, Record>`, and `impl Store`

- [ ] **Step 1: Write the failing tests**

Create `crates/gonzalo-core/src/memstore.rs` with only a test module for now:

```rust
//! A reference in-memory [`Store`] whose every decision comes from the planners
//! in [`crate::tombstone`]. Core tests use it (sync, reset, collect), and it is
//! the conformance suite's self-test: if `MemStore` and a real substrate
//! disagree, the substrate is wrong.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Body, DeleteResult, Identity, Meta, PutResult, RecordKind};

    fn rec(id: &str, body: &[u8]) -> Record {
        let body = Body::Inline(body.to_vec());
        Record {
            key: RecordKey::new("ns", "col", id),
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

    #[tokio::test]
    async fn delete_hides_from_consumer_reads_but_not_raw() {
        let store = MemStore::new().with_clock(5);
        let r = rec("a", b"x");
        assert!(matches!(store.put(r.clone(), None).await.unwrap(), PutResult::Committed(_)));
        assert_eq!(store.delete(&r.key, None).await.unwrap(), DeleteResult::Deleted);

        assert_eq!(store.get(&r.key).await.unwrap(), None);
        assert!(store.list(&KeyPrefix::default()).await.unwrap().is_empty());

        let raw = store.get_raw(&r.key).await.unwrap().unwrap();
        assert!(raw.is_tombstone());
        assert_eq!(raw.deleted_at, Some(5));
        assert_eq!(store.list_raw(&KeyPrefix::default()).await.unwrap(), vec![r.key.clone()]);
        assert_eq!(store.raw_snapshot().len(), 1);
    }

    #[tokio::test]
    async fn purge_removes_the_tombstone() {
        let store = MemStore::new();
        let r = rec("a", b"x");
        let _ = store.put(r.clone(), None).await.unwrap();
        let _ = store.delete(&r.key, None).await.unwrap();
        let t = store.get_raw(&r.key).await.unwrap().unwrap();
        assert_eq!(store.purge(&r.key, t.revision).await.unwrap(), DeleteResult::Deleted);
        assert!(store.raw_snapshot().is_empty());
    }

    #[tokio::test]
    async fn ancestor_cap_is_applied() {
        let store = MemStore::new().with_ancestor_cap(2);
        let mut r = rec("a", b"0");
        let mut rev = match store.put(r.clone(), None).await.unwrap() {
            PutResult::Committed(rev) => rev,
            PutResult::Conflict(c) => panic!("{c:?}"),
        };
        for i in 1..=4u8 {
            r.body = Body::Inline(vec![i]);
            r.revision = rev.next(&[i]);
            rev = match store.put(r.clone(), Some(rev.clone())).await.unwrap() {
                PutResult::Committed(rev) => rev,
                PutResult::Conflict(c) => panic!("{c:?}"),
            };
        }
        assert_eq!(store.get_raw(&r.key).await.unwrap().unwrap().ancestors.len(), 2);
    }

    #[test]
    #[should_panic(expected = "ancestor cap must be at least 1")]
    fn zero_cap_panics() {
        let _ = MemStore::new().with_ancestor_cap(0);
    }
}
```

Register it in `crates/gonzalo-core/src/lib.rs` after the `sync` re-export:

```rust
#[cfg(any(test, feature = "conformance"))]
pub mod memstore;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core --all-features memstore::tests`
Expected: FAIL to compile with `cannot find struct MemStore`.

- [ ] **Step 3: Implement `MemStore`**

Insert above `#[cfg(test)]` in `crates/gonzalo-core/src/memstore.rs`:

```rust
use crate::{
    DEFAULT_ANCESTOR_CAP, DeletePlan, DeleteResult, Identity, KeyPrefix, PurgePlan, PutPlan,
    PutResult, Record, RecordKey, Result, Revision, Store, now_ms, plan_delete, plan_purge,
    plan_put, plan_put_raw, validate_ancestor_cap,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Reference in-memory store. See the module docs.
pub struct MemStore {
    records: Mutex<BTreeMap<RecordKey, Record>>,
    cap: usize,
    clock: Option<i64>,
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemStore {
    /// An empty store with the default ancestor cap and the wall clock.
    pub fn new() -> Self {
        Self {
            records: Mutex::new(BTreeMap::new()),
            cap: DEFAULT_ANCESTOR_CAP,
            clock: None,
        }
    }

    /// Use `cap` ancestors. Panics on 0; this is a test helper, and real stores
    /// return the error from `validate_ancestor_cap` instead.
    pub fn with_ancestor_cap(mut self, cap: usize) -> Self {
        self.cap = validate_ancestor_cap(cap).unwrap_or_else(|e| panic!("{e}"));
        self
    }

    /// Stamp every tombstone with `now_ms` instead of the wall clock, for
    /// deterministic collection tests.
    pub fn with_clock(mut self, now_ms: i64) -> Self {
        self.clock = Some(now_ms);
        self
    }

    /// Every stored record, tombstones included.
    pub fn raw_snapshot(&self) -> BTreeMap<RecordKey, Record> {
        self.records.lock().unwrap().clone()
    }

    fn now(&self) -> i64 {
        self.clock.unwrap_or_else(now_ms)
    }
}

#[async_trait]
impl Store for MemStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .get(key)
            .filter(|r| !r.is_tombstone())
            .cloned())
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let mut g = self.records.lock().unwrap();
        let key = record.key.clone();
        match plan_put(g.get(&key), record, expected, self.cap) {
            PutPlan::Write(stored) => {
                let rev = stored.revision.clone();
                g.insert(key, stored);
                Ok(PutResult::Committed(rev))
            }
            PutPlan::Conflict(c) => Ok(PutResult::Conflict(c)),
            PutPlan::NotFound => Err(crate::CoreError::NotFound(key)),
        }
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .values()
            .filter(|r| !r.is_tombstone() && prefix.matches(&r.key))
            .map(|r| r.key.clone())
            .collect())
    }

    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        let now = self.now();
        let mut g = self.records.lock().unwrap();
        match plan_delete(g.get(key), expected, now, self.cap, author.as_ref()) {
            DeletePlan::Write(tombstone) => {
                g.insert(key.clone(), tombstone);
                Ok(DeleteResult::Deleted)
            }
            DeletePlan::Noop => Ok(DeleteResult::Deleted),
            DeletePlan::Conflict(c) => Ok(DeleteResult::Conflict(c)),
        }
    }

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        Ok(self.records.lock().unwrap().get(key).cloned())
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .keys()
            .filter(|k| prefix.matches(k))
            .cloned()
            .collect())
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let mut g = self.records.lock().unwrap();
        let key = record.key.clone();
        match plan_put_raw(g.get(&key), record, expected, self.cap) {
            PutPlan::Write(stored) => {
                let rev = stored.revision.clone();
                g.insert(key, stored);
                Ok(PutResult::Committed(rev))
            }
            PutPlan::Conflict(c) => Ok(PutResult::Conflict(c)),
            PutPlan::NotFound => Err(crate::CoreError::NotFound(key)),
        }
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        let mut g = self.records.lock().unwrap();
        match plan_purge(g.get(key), &expected) {
            PurgePlan::Remove => {
                g.remove(key);
                Ok(DeleteResult::Deleted)
            }
            PurgePlan::Noop => Ok(DeleteResult::Deleted),
            PurgePlan::Conflict(c) => Ok(DeleteResult::Conflict(c)),
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core --all-features memstore::tests`
Expected: PASS (4 tests).

Run: `cargo clippy -p gonzalo-core --all-targets --all-features -- -D warnings`
Expected: no warnings. The `Mutex` guard is never held across an `.await`, because every method locks, decides and returns synchronously.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/gonzalo-core/src/memstore.rs crates/gonzalo-core/src/lib.rs
git commit -m "feat(core): reference MemStore built on the tombstone planners (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 6: `run_tombstone_conformance`

**Files:**
- Modify: `crates/gonzalo-core/src/conformance.rs` (imports at `:5-8`; new public function after `run_store_conformance` at `:47`; new cases; self-test module at end of file)

**Interfaces:**
- Consumes: the `Store` trait (Task 4), `MemStore` (Task 5), `tombstone_hash` (Task 2), `DEFAULT_ANCESTOR_CAP`
- Produces: `pub async fn run_tombstone_conformance<S, F, Fut>(factory: F, cap: usize) where S: Store, F: Fn() -> Fut, Fut: std::future::Future<Output = S>`

- [ ] **Step 1: Write the self-test that calls the not-yet-existing suite**

Append to the end of `crates/gonzalo-core/src/conformance.rs`:

```rust

#[cfg(test)]
mod self_test {
    use super::*;
    use crate::memstore::MemStore;

    #[tokio::test]
    async fn memstore_passes_store_conformance() {
        run_store_conformance(|| async { MemStore::new() }).await;
    }

    #[tokio::test]
    async fn memstore_passes_tombstone_conformance_default_cap() {
        run_tombstone_conformance(|| async { MemStore::new() }, crate::DEFAULT_ANCESTOR_CAP).await;
    }

    #[tokio::test]
    async fn memstore_passes_tombstone_conformance_small_cap() {
        run_tombstone_conformance(|| async { MemStore::new().with_ancestor_cap(3) }, 3).await;
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p gonzalo-core --all-features conformance::self_test`
Expected: FAIL to compile with `cannot find function run_tombstone_conformance`.

- [ ] **Step 3: Implement the suite**

Replace the import block at `crates/gonzalo-core/src/conformance.rs:5-8`:

```rust
use crate::{
    BlobStore, Body, ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, Meta, PutResult,
    Record, RecordKey, RecordKind, Revision, Store, tombstone_hash,
};
```

Make sure `sample()` (`:11-28`) includes the new fields. Task 1 Step 5 already added them:

```rust
        links: Vec::new(),
        ancestors: Vec::new(),
        deleted_at: None,
        key,
```

Insert after the closing brace of `run_store_conformance` (`:47`):

```rust

/// Tombstone, recreation and purge semantics every `Store` must share
/// (ADR 0021, spec §6.1). `factory` must build fresh, empty stores whose
/// ancestor cap is `cap`.
pub async fn run_tombstone_conformance<S, F, Fut>(factory: F, cap: usize)
where
    S: Store,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = S>,
{
    delete_hides_from_get_and_list(&factory().await).await;
    delete_visible_to_raw_reads(&factory().await).await;
    delete_stale_expected_writes_no_tombstone(&factory().await).await;
    delete_of_absent_key_writes_nothing(&factory().await).await;
    delete_of_tombstone_is_noop(&factory().await).await;
    independent_deletes_are_identical(&factory().await, &factory().await).await;
    tombstone_never_collides_with_empty_body(&factory().await).await;
    recreate_continues_chain(&factory().await).await;
    put_some_over_tombstone_is_not_found(&factory().await).await;
    replication_overwrite_of_tombstone(&factory().await).await;
    put_raw_create_over_tombstone_conflicts(&factory().await).await;
    put_raw_never_restamps(&factory().await).await;
    delete_as_stamps_author(&factory().await).await;
    purge_removes_physically(&factory().await).await;
    purge_absent_is_noop(&factory().await).await;
    purge_conflicts_after_recreation(&factory().await).await;
    ancestors_capped_and_ordered(&factory().await, cap).await;
}

fn tomb_key(id: &str) -> RecordKey {
    RecordKey::new("ns", "tomb", id)
}

fn tomb_prefix() -> KeyPrefix {
    KeyPrefix {
        namespace: Some("ns".into()),
        collection: Some("tomb".into()),
    }
}

async fn committed<S: Store>(store: &S, rec: Record, expected: Option<Revision>) -> Revision {
    match store.put(rec, expected).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    }
}

async fn put_then_delete<S: Store>(store: &S, key: &RecordKey, payload: &[u8]) -> (Revision, Record) {
    let rev = committed(store, sample(key.clone(), payload), None).await;
    assert_eq!(store.delete(key, None).await.unwrap(), DeleteResult::Deleted);
    let tomb = store
        .get_raw(key)
        .await
        .unwrap()
        .expect("tombstone visible to get_raw");
    (rev, tomb)
}

/// A deleted key is absent to consumers; its sibling is unaffected.
async fn delete_hides_from_get_and_list<S: Store>(store: &S) {
    let gone = tomb_key("gone");
    let kept = tomb_key("kept");
    committed(store, sample(kept.clone(), b"stay"), None).await;
    let _ = put_then_delete(store, &gone, b"bye").await;
    assert_eq!(store.get(&gone).await.unwrap(), None);
    assert_eq!(store.list(&tomb_prefix()).await.unwrap(), vec![kept]);
}

/// Raw reads see the tombstone, shaped exactly per spec §3.1.
async fn delete_visible_to_raw_reads<S: Store>(store: &S) {
    let key = tomb_key("raw");
    let (rev, t) = put_then_delete(store, &key, b"bye").await;
    assert!(t.is_tombstone());
    assert_eq!(t.kind, RecordKind::Tombstone);
    assert_eq!(t.body, Body::Inline(Vec::new()));
    assert!(t.deleted_at.is_some(), "tombstone carries deleted_at");
    assert_eq!(t.revision.counter, rev.counter + 1);
    assert_eq!(t.revision.hash, tombstone_hash());
    assert_eq!(t.parent, Some(rev.clone()));
    assert_eq!(t.ancestors.first(), Some(&rev));
    assert!(store.list_raw(&tomb_prefix()).await.unwrap().contains(&key));
}

/// A stale conditional delete conflicts and leaves the live record in place.
async fn delete_stale_expected_writes_no_tombstone<S: Store>(store: &S) {
    let key = tomb_key("stale");
    let rev = committed(store, sample(key.clone(), b"keep"), None).await;
    let wrong = Revision::initial(b"a-revision-that-was-never-current");
    assert!(matches!(
        store.delete(&key, Some(wrong)).await.unwrap(),
        DeleteResult::Conflict(_)
    ));
    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert!(!raw.is_tombstone());
    assert_eq!(raw.revision, rev);
}

/// Deleting a key the store never held writes nothing.
async fn delete_of_absent_key_writes_nothing<S: Store>(store: &S) {
    let key = tomb_key("never");
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
}

/// Deleting a tombstone does not advance its chain.
async fn delete_of_tombstone_is_noop<S: Store>(store: &S) {
    let key = tomb_key("twice");
    let (_, first) = put_then_delete(store, &key, b"bye").await;
    assert_eq!(store.delete(&key, None).await.unwrap(), DeleteResult::Deleted);
    let second = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(second.revision, first.revision);
}

/// Two stores deleting the same revision independently agree on the result.
async fn independent_deletes_are_identical<S: Store>(a: &S, b: &S) {
    let key = tomb_key("same");
    let (_, ta) = put_then_delete(a, &key, b"shared").await;
    let (_, tb) = put_then_delete(b, &key, b"shared").await;
    assert_eq!(ta.revision, tb.revision);
}

/// A tombstone's revision never equals an empty-body edit at the same counter.
async fn tombstone_never_collides_with_empty_body<S: Store>(store: &S) {
    let key = tomb_key("empty");
    let (rev, t) = put_then_delete(store, &key, b"x").await;
    let empty_edit = rev.next(b"");
    assert_eq!(t.revision.counter, empty_edit.counter);
    assert_ne!(t.revision, empty_edit);
}

/// A create over a tombstone continues the revision chain.
async fn recreate_continues_chain<S: Store>(store: &S) {
    let key = tomb_key("recreate");
    let (_, t) = put_then_delete(store, &key, b"v0").await;
    let r = committed(store, sample(key.clone(), b"v1"), None).await;
    assert_eq!(r.counter, t.revision.counter + 1);
    assert_eq!(r.hash, ContentHash::of(b"v1"));
    let live = store.get(&key).await.unwrap().expect("recreated record is visible");
    assert_eq!(live.revision, r);
    assert_eq!(live.parent, Some(t.revision.clone()));
    assert_eq!(live.deleted_at, None);
    assert_eq!(live.ancestors.first(), Some(&t.revision));
}

/// A tombstone is absent to a conditional put naming any other revision.
async fn put_some_over_tombstone_is_not_found<S: Store>(store: &S) {
    let key = tomb_key("late");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    let with_tomb_rev = store
        .put(sample(key.clone(), b"late"), Some(t.revision.clone()))
        .await;
    assert!(
        matches!(with_tomb_rev, Err(CoreError::NotFound(_))),
        "consumer put naming the tombstone's revision must be NotFound, got {with_tomb_rev:?}"
    );
    let out = store
        .put(
            sample(key.clone(), b"late"),
            Some(Revision::initial(b"a-revision-that-was-never-current")),
        )
        .await;
    assert!(matches!(out, Err(CoreError::NotFound(_))), "got {out:?}");
}

/// Replication naming the tombstone's revision overwrites it unchanged.
async fn replication_overwrite_of_tombstone<S: Store>(store: &S) {
    let key = tomb_key("replicated");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    let mut incoming = sample(key.clone(), b"from-peer");
    incoming.revision = Revision {
        counter: t.revision.counter + 5,
        hash: ContentHash::of(b"from-peer"),
    };
    incoming.parent = Some(t.revision.clone());
    incoming.ancestors = vec![t.revision.clone()];
    let r = match store
        .put_raw(incoming.clone(), Some(t.revision.clone()))
        .await
        .unwrap()
    {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    };
    assert_eq!(r, incoming.revision);
    assert_eq!(store.get(&key).await.unwrap().unwrap().revision, incoming.revision);
}

/// A replication create over a tombstone conflicts carrying it; nothing is written.
async fn put_raw_create_over_tombstone_conflicts<S: Store>(store: &S) {
    let key = tomb_key("raw-create");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    match store.put_raw(sample(key.clone(), b"copy"), None).await.unwrap() {
        PutResult::Conflict(c) => {
            assert!(c.current.is_tombstone());
            assert_eq!(c.current.revision, t.revision);
        }
        PutResult::Committed(rev) => {
            panic!("put_raw must never recreate over a tombstone, committed {rev:?}")
        }
    }
    assert_eq!(store.get_raw(&key).await.unwrap().unwrap().revision, t.revision);
    assert_eq!(store.get(&key).await.unwrap(), None);
}

/// `put_raw` stores the caller's revision verbatim, even a non-sequential one.
async fn put_raw_never_restamps<S: Store>(store: &S) {
    let key = tomb_key("verbatim");
    let rev = committed(store, sample(key.clone(), b"v0"), None).await;
    let mut incoming = sample(key.clone(), b"peer");
    incoming.revision = Revision {
        counter: rev.counter + 9,
        hash: ContentHash::of(b"peer"),
    };
    incoming.parent = Some(rev.clone());
    let stored = match store.put_raw(incoming.clone(), Some(rev.clone())).await.unwrap() {
        PutResult::Committed(r) => r,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    };
    assert_eq!(stored, incoming.revision);
    let got = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(got.revision, incoming.revision);
    assert_eq!(got.ancestors.first(), Some(&rev));
}

/// `delete_as` records the deleter on the tombstone.
async fn delete_as_stamps_author<S: Store>(store: &S) {
    let key = tomb_key("author");
    committed(store, sample(key.clone(), b"mine"), None).await;
    let deleter = Identity::new("deleter");
    assert_eq!(
        store.delete_as(&key, None, Some(deleter.clone())).await.unwrap(),
        DeleteResult::Deleted
    );
    let t = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(t.meta.author, deleter);
}

/// Purge physically removes the tombstone.
async fn purge_removes_physically<S: Store>(store: &S) {
    let key = tomb_key("purged");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    assert_eq!(store.purge(&key, t.revision).await.unwrap(), DeleteResult::Deleted);
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
    assert!(!store.list_raw(&tomb_prefix()).await.unwrap().contains(&key));
}

/// Purging an absent key is an idempotent no-op.
async fn purge_absent_is_noop<S: Store>(store: &S) {
    let key = tomb_key("purge-absent");
    assert_eq!(
        store.purge(&key, Revision::initial(b"x")).await.unwrap(),
        DeleteResult::Deleted
    );
}

/// Purge naming a tombstone that was since recreated conflicts; the record survives.
async fn purge_conflicts_after_recreation<S: Store>(store: &S) {
    let key = tomb_key("purge-race");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    let r = committed(store, sample(key.clone(), b"back"), None).await;
    match store.purge(&key, t.revision).await.unwrap() {
        DeleteResult::Conflict(c) => assert_eq!(c.current.revision, r),
        DeleteResult::Deleted => panic!("purge must not remove a recreated record"),
    }
    assert_eq!(store.get(&key).await.unwrap().unwrap().revision, r);
}

/// After more updates than the cap, ancestors hold exactly the newest `cap`.
async fn ancestors_capped_and_ordered<S: Store>(store: &S, cap: usize) {
    let key = tomb_key("cap");
    let mut rec = sample(key.clone(), b"0");
    let mut rev = committed(store, rec.clone(), None).await;
    let mut history = vec![rev.clone()];
    for i in 1..=(cap + 5) {
        let body = i.to_string().into_bytes();
        rec.body = Body::Inline(body.clone());
        rec.parent = Some(rev.clone());
        rec.revision = rev.next(&body);
        rev = committed(store, rec.clone(), Some(rev.clone())).await;
        history.push(rev.clone());
    }
    let stored = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(stored.revision, rev);
    let expected: Vec<Revision> = history.iter().rev().skip(1).take(cap).cloned().collect();
    assert_eq!(stored.ancestors, expected);
}
```

- [ ] **Step 4: Run the self-test to verify it passes**

Run: `cargo test -p gonzalo-core --all-features conformance::self_test`
Expected: PASS (3 tests).

- [ ] **Step 5: Confirm the real stores are unaffected**

Real stores don't call `run_tombstone_conformance` yet. Their slices wire it in.

Run: `cargo test --workspace --all-features`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/gonzalo-core/src/conformance.rs
git commit -m "test(core): run_tombstone_conformance suite with MemStore self-test (#203)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu"
```

---

### Task 7: Gate and pull request

**Files:** none new.

**Interfaces:**
- Consumes: Tasks 1–6
- Produces: a merged slice 1 PR, which unblocks slices 2 and 3

- [ ] **Step 1: Run the full gate as bare commands**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features
```

Expected: each command exits 0. If any fails, fix the cause and re-run **all four**.

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin "$(git branch --show-current)"
gh pr create --base main --title "feat(core): tombstone model, planners and Store raw/purge methods (#203 slice 1)" --body "$(cat <<'EOF'
Part of #203. Slice 1 of the replicated-deletion plan
(docs/superpowers/plans/2026-09-13-tombstones-00-overview.md).

- `RecordKind::Tombstone`, `Record.ancestors`, `Record.deleted_at` (serde-additive)
- `gonzalo_core::tombstone`: domain-separated tombstone hash, ancestor fold,
  `plan_put` / `plan_delete` / `plan_purge`
- `Store::delete_as` / `get_raw` / `list_raw` / `put_raw` / `purge` (required,
  no defaults); `delete` is now provided and calls `delete_as(.., None)`
- Reference `MemStore` and `run_tombstone_conformance`, self-tested

No store changes behaviour yet: real stores get interim raw/purge methods
that match today's semantics. Breaking for external `Store` implementers
(0.7.0).

https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
)"
```

- [ ] **Step 3: Watch CI and merge when it's green**

```bash
gh pr checks --watch
gh pr merge --squash --delete-branch
```

Expected: every check passes before the merge. If a check fails, pull the failing log with `gh run view <id> --log-failed`, fix it, push, and watch again.

---

## Self-Review Notes

- **Spec coverage (slice 1 portion):** §3.1 fields, kind and hash (Tasks 1–2); §3.2 trait and put/delete tables (Tasks 3–4); ancestor fold, including stored-revision exclusion (Task 2); §3.9 cap constant and validation (Task 2); §6.1 conformance cases (Task 6). The spec's `delete_conflicts_on_stale_expected` is covered by the existing `delete_stale_expected_conflicts` plus the new `delete_stale_expected_writes_no_tombstone`. Per-store builders, filtering, daemon routes, sync, pull, reset, collect, soak and docs belong to slices 2–7.
- **Reconciled contract:** `put_raw`/`plan_put_raw` close the sync copy resurrection window (spec §3.2 "Replication writes"); `delete_as` carries the deleter (spec §3.1). Consumer `plan_put` no longer has a replication branch.
- **Existing delete conformance cases** (`conformance.rs:49-109`) only assert `get == None` after delete, so they hold under tombstones and stay as they are.
- **Recreation of a tombstone by a tombstone** (`put(tombstone, None)` over a tombstone) keeps the tombstone hash. The spec doesn't list this case; it closes a collision hole the §3.1 rationale would otherwise reopen.
