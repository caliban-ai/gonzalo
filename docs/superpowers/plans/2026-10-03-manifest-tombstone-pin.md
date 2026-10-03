# Manifest Tombstone Pin and Undelete Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make ADR 0024's promise — a tombstone keeps a deleted record's bytes until `collect` — hold for manifests, and let an operator put a deleted index back.

**Architecture:** A manifest tombstone retains its body instead of discarding it, and records `deleted_kind` so GC knows which parser to use for the blob references inside. `undelete` is a core function built from `get_raw` + `has_blob` + `put_raw`; it needs no new planner arm because `plan_put_raw` already writes a non-tombstone over a tombstone on a matching revision and never re-stamps, which is what preserves `meta.created`.

**Tech Stack:** Rust (edition 2024), `serde`, `async-trait`, `tokio`, `clap`.

**Spec:** `docs/superpowers/specs/2026-10-03-manifest-tombstone-pin-design.md`

## Global Constraints

- Only `RecordKind::GraphManifest` and `RecordKind::VectorManifest` retain their body. Every other kind's tombstone is unchanged: empty body, `deleted_blob` pinning a `Body::Blob` hash, `deleted_kind: None`.
- `deleted_kind` carries `#[serde(default, skip_serializing_if = "Option::is_none")]`, matching `deleted_at` and `deleted_blob`, so records already on disk deserialise unchanged.
- Two peers deleting the same revision must still produce **byte-identical** tombstones.
- `collect` is **unchanged**. Removing the tombstone unmarks the hashes and the next sweep frees them.
- `undelete` works for manifest kinds only, and must **refuse** rather than write when: there is no tombstone, the record is live, `deleted_kind` is `None`, the key was re-created, or a blob the retained body names is missing.
- `undelete` must **not** add a planner arm, a `Store` trait method, a daemon route, or a `.proto` change.
- `undelete` verifies blobs **before** writing. A record for an index that cannot be opened is worse than a refusal.
- `gonzalo-core`'s dev-dependencies are **tokio only**, and `MemStore` implements `Store` but not `BlobStore`. Nothing in this plan adds a dev-dependency to `gonzalo-core` — tests needing a real `Store + BlobStore` go in `crates/gonzalo-store-fs/tests/`, which already depends on core and has `tempfile`.
- The gate is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace --all-targets --all-features`, `cargo test --workspace --all-features`.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.

## Review Focus

Five conditions the spec implies that no task's happy path reaches. Each one's test is added to the task that owns the code.

1. **`reset` over a namespace containing a vector index.** This is the realistic bulk trigger the spec names — it tombstones *every* key in a namespace. The shards must end up pinned, exactly as for a single `delete`. → Task 2.
2. **Deleting an already-deleted manifest.** `delete_of_tombstone_is_noop` is existing conformance; the second delete must not rewrite the tombstone and discard the retained body, which would unpin the shards. → Task 1.
3. **`purge` of a manifest tombstone.** The admin escape hatch must still work: after purge the shards are unmarked and the next sweep frees them. Without this, retained bodies could make a manifest's blobs unreclaimable by any route. → Task 2.
4. **A `deleted_kind` that disagrees with the body** — a hand-written or corrupted tombstone claiming `VectorManifest` over an unparseable body. GC must error loudly rather than sweep, matching the existing undecodable-manifest behaviour. → Task 2.
5. **Marking a hash the store does not hold.** A manifest tombstone synced to a peer that never received the blobs makes the mark set name absent hashes. The sweep must not error or miscount. → Task 2.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/gonzalo-core/src/record.rs` | **Modify.** The `deleted_kind` field. |
| `crates/gonzalo-core/src/tombstone.rs` | **Modify.** `tombstone_of` retains manifest bodies, plus its unit tests. |
| `crates/gonzalo-core/src/gc.rs` | **Modify.** Mark through a retained body, plus its unit tests. |
| `crates/gonzalo-core/src/undelete.rs` | **Create.** The `undelete` function. No test module — see Task 4. |
| `crates/gonzalo-core/src/lib.rs` | **Modify.** Module and re-exports. |
| `crates/gonzalo-core/src/conformance.rs` | **Modify.** The round-trip case every substrate runs. |
| `crates/gonzalo-store-fs/tests/blob_gc.rs` | **Modify.** End-to-end pin, collect, purge and reset tests. |
| `crates/gonzalo-store-fs/tests/undelete.rs` | **Create.** `undelete`'s tests, against a real store with blobs. |
| `crates/gonzalo-vector/tests/durability.rs` | **Modify.** The end-to-end acceptance test: delete, sweep, restore, query. |
| `crates/gonzalo-cli/src/lib.rs` | **Modify.** The `undelete` wrapper. |
| `crates/gonzalo-cli/src/main.rs` | **Modify.** `gonzalo undelete`. |
| `crates/gonzalo-cli/tests/cli.rs` | **Modify.** The subcommand's integration test. |
| `crates/gonzalo/src/lib.rs` | **Modify.** Facade re-export. |
| `docs/adr/0029-manifest-tombstone-pin.md` | **Create.** The decision record. |

**Verified API shapes** — checked against the tree at this plan's HEAD. Use them verbatim; they are the names the code blocks below assume.

- `ContentHash(pub String)`, with no `Display` impl — use `hash.0` in a format string, never `{hash}`.
- `CoreError::{NotFound(RecordKey), Serde(String), Invalid(String), Backend(String)}`.
- `Record::create(key, kind, body, meta)` sets `revision: Revision::initial(body.bytes())` itself.
- `VectorManifest::new(space: impl Into<String>, dim: usize, shards: u16)`, `::key(namespace, index_id)`, `to_body()`, `from_body(&Body)`, `entries: BTreeMap<u16, ContentHash>`.
- `Manifest::new()`, `::key(repo, view_id)`, `insert(path, hash)`, `to_body()`, `from_body(&Body)`, `entries: BTreeMap<String, ContentHash>`.
- `KeyPrefix { namespace: Option<String>, collection: Option<String> }` — a plain struct literal.
- `collect(store, prefix, horizon, now_ms) -> CollectReport { purged: Vec<RecordKey>, unstamped, conflicts }`.
- `reset(store, prefix) -> ResetReport { deleted: Vec<RecordKey>, conflicts }`.
- `BlobStore::has_blob(&hash) -> Result<bool>` (defaulted), `delete_blob(&hash) -> Result<()>`.
- `gc.rs` test helpers: `h(s)`, `record(id, kind, body)`, `blob_body(s)`, `meta()`, `immediate()`, `aged(Vec<(ContentHash, SystemTime)>)`, `at(secs)`, `policy(now_secs, min_age_secs)`, `FakeBlobs { listed, deleted }`.
- `blob_gc.rs` helpers: `fresh_store()`, `meta()`, `committed(store, record)` — **two** args, `put_manifest(store, repo, view, &manifest)`, `put_blob_backed(store, key, content)`, `immediate()`.
- `conformance.rs` helpers: `sample(key, payload)` (kind `Topic`), `tomb_key(id)` → `ns/tomb/id`, `committed(store, rec, expected)` — **three** args.
- `cli.rs` helpers: `run(root, args)` — it appends `--root <root>` itself, so never pass `--root` in `args`; `stdout(out)`, `stderr(out)`, `seed(root, ns, col, ids)`, `record_file(root, ns, col, id)`, `edit_record_file(path, edit)`.

---

### Task 1: `deleted_kind`, and a manifest tombstone that keeps its body

**Files:**
- Modify: `crates/gonzalo-core/src/record.rs` (the `Record` struct), `crates/gonzalo-core/src/tombstone.rs` (`tombstone_of` and its `mod tests`)

**Interfaces:**
- Produces: `Record.deleted_kind: Option<RecordKind>`; `tombstone_of` retaining manifest bodies.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/gonzalo-core/src/tombstone.rs`. Read two neighbouring tests first and match how they build a record; add only the imports the module lacks.

```rust
    fn vector_manifest_record() -> Record {
        let mut m = crate::VectorManifest::new("space-a", 3, 256);
        m.entries.insert(0, ContentHash::of(b"shard-zero"));
        m.entries.insert(9, ContentHash::of(b"shard-nine"));
        Record::create(
            crate::VectorManifest::key("ns", "memories"),
            RecordKind::VectorManifest,
            m.to_body(),
            Meta::new(Identity::new("tester"), "test"),
        )
    }

    // The whole point: a manifest's body names its blobs out of line, so
    // discarding it loses both the pin and the shard-to-blob mapping.
    #[test]
    fn a_vector_manifest_tombstone_keeps_its_body_and_kind() {
        let live = vector_manifest_record();
        let t = tombstone_of(&live, 1_000, 8, None);

        assert!(t.is_tombstone());
        assert_eq!(t.body, live.body, "the manifest body is retained verbatim");
        assert_eq!(t.deleted_kind, Some(RecordKind::VectorManifest));
        assert_eq!(t.deleted_blob, None, "an inline body pins nothing this way");
        assert_eq!(t.deleted_at, Some(1_000));
    }

    #[test]
    fn a_graph_manifest_tombstone_keeps_its_body_and_kind() {
        let mut gm = crate::Manifest::new();
        gm.insert("src/lib.rs", ContentHash::of(b"slice"));
        let live = Record::create(
            crate::Manifest::key("repo", "main"),
            RecordKind::GraphManifest,
            gm.to_body(),
            Meta::new(Identity::new("tester"), "test"),
        );
        let t = tombstone_of(&live, 1_000, 8, None);
        assert_eq!(t.body, live.body);
        assert_eq!(t.deleted_kind, Some(RecordKind::GraphManifest));
    }

    // Nothing else moves. A blob-bodied record still pins through deleted_blob
    // and still gets an empty body.
    #[test]
    fn a_blob_bodied_tombstone_is_unchanged() {
        let live = Record::create(
            RecordKey::new("ns", "coll", "doc"),
            RecordKind::Topic,
            Body::blob(b"out of line"),
            Meta::new(Identity::new("tester"), "test"),
        );

        let t = tombstone_of(&live, 1_000, 8, None);
        assert_eq!(t.body, Body::Inline(Vec::new()));
        assert_eq!(t.deleted_blob, Some(ContentHash::of(b"out of line")));
        assert_eq!(t.deleted_kind, None);
    }

    #[test]
    fn an_inline_non_manifest_tombstone_is_unchanged() {
        let live = Record::create(
            RecordKey::new("ns", "coll", "topic"),
            RecordKind::Topic,
            Body::Inline(b"{}".to_vec()),
            Meta::new(Identity::new("tester"), "test"),
        );
        let t = tombstone_of(&live, 1_000, 8, None);
        assert_eq!(t.body, Body::Inline(Vec::new()));
        assert_eq!(t.deleted_kind, None);
        assert_eq!(t.deleted_blob, None);
    }

    // Replication depends on this: two peers deleting the same revision must
    // produce the same bytes, or sync sees a divergence that isn't one.
    #[test]
    fn two_peers_deleting_one_manifest_revision_agree_byte_for_byte() {
        let live = vector_manifest_record();
        let a = tombstone_of(&live, 1_000, 8, None);
        let b = tombstone_of(&live, 1_000, 8, None);
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap()
        );
    }

    // Review Focus 2: a second delete must not rewrite the tombstone and throw
    // the retained body away, which would silently unpin every shard. The
    // store's own guard is `plan_delete`, pinned by the existing conformance
    // case `delete_of_tombstone_is_noop`; this test documents what the
    // function itself does if a future caller reaches it directly.
    #[test]
    fn tombstoning_a_tombstone_retains_nothing() {
        let live = vector_manifest_record();
        let first = tombstone_of(&live, 1_000, 8, None);
        let again = tombstone_of(&first, 2_000, 8, None);
        assert_eq!(
            again.body,
            Body::Inline(Vec::new()),
            "a tombstone is not a manifest kind, so this path retains nothing"
        );
        assert_eq!(again.deleted_kind, None);
    }

    // Every tombstone already on disk predates the field.
    #[test]
    fn a_tombstone_without_deleted_kind_deserialises() {
        let json = r#"{"key":{"namespace":"ns","collection":"coll","id":"x"},
            "kind":"Tombstone","revision":{"counter":1,"hash":"aa"},
            "body":{"Inline":[]},"meta":{"author":{"name":"t"},
            "origin_system":"test","created":0,"updated":0,"labels":{}},
            "links":[],"deleted_at":1}"#;
        let r: Record = serde_json::from_str(json).unwrap();
        assert_eq!(r.deleted_kind, None);
    }
```

If the last test fails on a field name or on `Revision`'s shape rather than on `deleted_kind`, serialise a real tombstone, print it, and fix the literal to match. Do not weaken the assertion.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core tombstone`
Expected: FAIL to compile — `Record` has no field `deleted_kind`.

- [ ] **Step 3: Add the field**

In `crates/gonzalo-core/src/record.rs`, after `deleted_blob`:

```rust
    /// Tombstones only: the kind of the record that was deleted.
    ///
    /// Set when the deleted body is **retained** — the manifest kinds, whose
    /// bodies name blobs out of line — so GC knows which parser to use for the
    /// references inside, and `undelete` knows what kind to recreate. A
    /// manifest's body is the only record of which blob was which shard, so
    /// discarding it would leave the pinned bytes unnameable (ADR 0029).
    ///
    /// `None` on a tombstone whose body was discarded, and on every tombstone
    /// written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_kind: Option<RecordKind>,
```

That is a new field on a struct constructed in many places. `cargo build --workspace --all-targets --all-features` lists every site; add `deleted_kind: None` to each struct literal. `Record::create` and `Record::update` must set it to `None` — a live record never carries it.

- [ ] **Step 4: Retain manifest bodies in `tombstone_of`**

In `crates/gonzalo-core/src/tombstone.rs`, before the returned `Record` literal:

```rust
    // A manifest's body names its blobs out of line, so it is the only record
    // of which blob was which shard. Discarding it would drop the hashes out of
    // the GC mark set AND lose the mapping, leaving any pinned bytes
    // unnameable — so these kinds keep their body (ADR 0029).
    let retains_body = matches!(
        current.kind,
        RecordKind::GraphManifest | RecordKind::VectorManifest
    );
```

and in the literal, replacing the existing `body` and adding `deleted_kind` beside `deleted_blob` — leave `deleted_blob` and its doc comment exactly as they are, since a retaining kind always has an inline body and so yields `None` there without a special case:

```rust
        body: if retains_body {
            current.body.clone()
        } else {
            Body::Inline(Vec::new())
        },
        deleted_kind: retains_body.then_some(current.kind),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core`, then `cargo test --workspace --all-features`
Expected: PASS. If a test elsewhere asserts a whole `Record` by equality and now fails on the new field, add `deleted_kind: None` to its expectation — do not weaken the assertion.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "$(printf 'feat(core): a manifest tombstone keeps its body and kind (#327)\n\nA manifest body names its blobs out of line, so discarding it on delete lost\nboth the GC pin and the shard-to-blob mapping. Retaining kinds now keep the\nbody and record deleted_kind.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 2: GC marks through a retained body

This is the task that fixes the bug. Without it, Task 1 retains a body nothing reads.

**Files:**
- Modify: `crates/gonzalo-core/src/gc.rs` (`live_blob_hashes` and its `mod tests`), `crates/gonzalo-store-fs/tests/blob_gc.rs`

**Interfaces:**
- Consumes: `Record.deleted_kind` and retained manifest bodies (Task 1).
- Produces: no new API — `live_blob_hashes` gains behaviour.

- [ ] **Step 1: Write the failing unit tests**

Add to `mod tests` in `crates/gonzalo-core/src/gc.rs`, using its existing helpers (`h`, `record`, `aged`, `at`, `policy`). Add `tombstone_of` and `VectorManifest` to the module's imports if absent.

```rust
    fn vector_manifest_body(entries: &[(u16, &str)]) -> Body {
        let mut m = crate::VectorManifest::new("space-a", 3, 256);
        for (id, content) in entries {
            m.entries.insert(*id, h(content));
        }
        m.to_body()
    }

    // The bug. A deleted vector index's shards must survive until the tombstone
    // is collected, exactly as a deleted blob-bodied record's blob does.
    #[test]
    fn a_vector_manifest_tombstone_pins_its_shards() {
        let live = record(
            "memories",
            RecordKind::VectorManifest,
            vector_manifest_body(&[(0, "shard-zero"), (9, "shard-nine")]),
        );
        let t = tombstone_of(&live, 1_000, 8, None);

        let marked = live_blob_hashes([&t]).unwrap();
        assert!(marked.contains(&h("shard-zero")));
        assert!(marked.contains(&h("shard-nine")));
    }

    #[test]
    fn a_graph_manifest_tombstone_pins_its_slices() {
        let mut gm = Manifest::new();
        gm.insert("src/lib.rs", h("slice"));
        let live = record("main", RecordKind::GraphManifest, gm.to_body());
        let t = tombstone_of(&live, 1_000, 8, None);

        assert!(live_blob_hashes([&t]).unwrap().contains(&h("slice")));
    }

    // Review Focus 4: a tombstone claiming a manifest kind over a body that
    // will not parse must error, never sweep. Same rule as the live arm.
    #[test]
    fn a_tombstone_whose_retained_body_will_not_parse_is_an_error() {
        let mut t = record(
            "memories",
            RecordKind::Tombstone,
            Body::Inline(b"not json".to_vec()),
        );
        t.deleted_kind = Some(RecordKind::VectorManifest);
        assert!(matches!(live_blob_hashes([&t]), Err(CoreError::Serde(_))));
    }

    // A tombstone of an ordinary kind marks nothing new, and one written before
    // the field existed has deleted_kind: None.
    #[test]
    fn a_tombstone_without_deleted_kind_marks_only_its_pinned_blob() {
        let mut t = record("doc", RecordKind::Tombstone, Body::Inline(Vec::new()));
        t.deleted_blob = Some(h("pinned"));
        let marked = live_blob_hashes([&t]).unwrap();
        assert_eq!(marked, BTreeSet::from([h("pinned")]));
    }

    // Review Focus 5: a manifest tombstone synced to a peer that never received
    // the blobs makes the mark set name hashes the store does not hold. That
    // must not error or distort the counts.
    #[tokio::test]
    async fn marking_a_hash_the_store_does_not_hold_is_harmless() {
        let blobs = aged(vec![(h("present"), at(0))]);
        let live = BTreeSet::from([h("present"), h("never-stored")]);
        let report = sweep_blobs_with(&blobs, &live, policy(100_000, 0))
            .await
            .unwrap();
        assert!(report.freed.is_empty());
        assert_eq!(report.retained, 1, "only the hash actually listed counts");
        assert_eq!(report.deferred, 0);
    }
```

`live_blob_hashes` takes an iterator of `&Record`; check its exact parameter and match it — if `[&t]` does not satisfy the bound, pass what the neighbouring tests pass.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core gc`
Expected: FAIL — `a_vector_manifest_tombstone_pins_its_shards` and `a_graph_manifest_tombstone_pins_its_slices` find the hashes absent from the mark set, because nothing reads a tombstone's retained body.

- [ ] **Step 3: Add the marking arm**

In `crates/gonzalo-core/src/gc.rs`, inside `live_blob_hashes`'s loop, after the existing `VectorManifest` arm:

```rust
        // A manifest tombstone retains its body, so the references inside it
        // are still live: the bytes survive until the tombstone is collected,
        // exactly like `deleted_blob` for a blob-bodied record (ADR 0029).
        // `deleted_kind` names the parser; guessing by trying both would make an
        // unrelated inline body that happens to parse look like a manifest.
        if record.kind == RecordKind::Tombstone {
            match record.deleted_kind {
                Some(RecordKind::VectorManifest) => live.extend(
                    crate::VectorManifest::from_body(&record.body)?
                        .entries
                        .into_values(),
                ),
                Some(RecordKind::GraphManifest) => live.extend(
                    crate::Manifest::from_body(&record.body)?
                        .entries
                        .into_values(),
                ),
                _ => {}
            }
        }
```

Match the surrounding arms' style — if the existing manifest arms are written as a `match record.kind`, fold these in as another arm rather than adding a separate `if`.

- [ ] **Step 4: Run the unit tests**

Run: `cargo test -p gonzalo-core gc`
Expected: PASS.

- [ ] **Step 5: Write the end-to-end tests, including three Review Focus items**

Add to `crates/gonzalo-store-fs/tests/blob_gc.rs`, using its real helpers — `fresh_store()`, `meta()`, `committed(store, record)` (two args), `immediate()`. Extend that file's `use gonzalo_core::{…}` list with `VectorManifest`, `reset`, and whatever else these need.

```rust
/// Store `manifest` as `(ns, index_id)`'s vector-manifest record, the shape GC
/// reads shards out of.
async fn put_vector_manifest(store: &FsStore, ns: &str, index_id: &str, m: &VectorManifest) {
    committed(
        store,
        Record::create(
            VectorManifest::key(ns, index_id),
            RecordKind::VectorManifest,
            m.to_body(),
            meta(),
        ),
    )
    .await;
}

/// Seed one vector index with a single shard blob, returning the manifest's key
/// and the shard's hash.
async fn store_with_vector_index(store: &FsStore) -> (RecordKey, ContentHash) {
    let shard = store.put_blob(b"shard bytes").await.unwrap();
    let mut m = VectorManifest::new("space-a", 3, 256);
    m.entries.insert(0, shard.clone());
    put_vector_manifest(store, "ns", "memories", &m).await;
    (VectorManifest::key("ns", "memories"), shard)
}

// End to end over a real store: delete a vector index, sweep with no grace
// period, and the shards are still there.
#[tokio::test]
async fn deleting_a_vector_index_does_not_free_its_shards() {
    let store = fresh_store();
    let (key, shard) = store_with_vector_index(&store).await;

    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );

    let report = gc_blobs_with(&store, immediate()).await.unwrap();
    assert!(report.freed.is_empty(), "the tombstone pins the shard");
    assert!(store.has_blob(&shard).await.unwrap());
}

// The horizon still ends. Collect the tombstone and the shard becomes
// collectable — pinning must not turn into leaking.
#[tokio::test]
async fn collecting_the_tombstone_releases_the_shards() {
    let store = fresh_store();
    let (key, shard) = store_with_vector_index(&store).await;
    store.delete(&key, None).await.unwrap();

    let collected = collect(&store, &KeyPrefix::default(), Duration::ZERO, now_ms() + 1)
        .await
        .unwrap();
    assert_eq!(collected.purged.len(), 1);

    let report = gc_blobs_with(&store, immediate()).await.unwrap();
    assert_eq!(report.freed, vec![shard]);
}

// Review Focus 3: purge is the admin escape hatch, and it must still release
// the shards — otherwise a retained body could make them unreclaimable.
#[tokio::test]
async fn purging_the_tombstone_releases_the_shards() {
    let store = fresh_store();
    let (key, shard) = store_with_vector_index(&store).await;
    store.delete(&key, None).await.unwrap();

    let t = store.get_raw(&key).await.unwrap().expect("tombstone");
    store.purge(&key, t.revision).await.unwrap();

    let report = gc_blobs_with(&store, immediate()).await.unwrap();
    assert_eq!(report.freed, vec![shard]);
}

// Review Focus 1: `reset` tombstones every key in a namespace, which is the
// realistic way a vector index gets deleted by accident. Its shards must be
// pinned just as a single delete pins them.
#[tokio::test]
async fn resetting_a_namespace_pins_a_vector_index_in_it() {
    let store = fresh_store();
    let (_key, shard) = store_with_vector_index(&store).await;

    let report = reset(
        &store,
        &KeyPrefix {
            namespace: Some("ns".into()),
            collection: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.deleted.len(), 1);

    let swept = gc_blobs_with(&store, immediate()).await.unwrap();
    assert!(
        swept.freed.is_empty(),
        "reset's tombstone pins the shard too"
    );
    assert!(store.has_blob(&shard).await.unwrap());
}
```

`VectorManifest::key` decides which namespace and collection the record lands in, so check what it returns before assuming `reset`'s prefix covers it: if its namespace is not the first argument, adjust the prefix. Keep the `report.deleted.len()` assertion either way — a reset that matched nothing would otherwise make this test pass vacuously.

- [ ] **Step 6: Run them and the full gate**

Run: `cargo test -p gonzalo-store-fs`, then the full gate from Global Constraints.
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates
git commit -m "$(printf 'fix(core): a manifest tombstone pins the blobs its body names (#327)\n\nlive_blob_hashes now marks through a retained manifest body, so deleting a\nvector index no longer frees its shards on the next sweep. Collect and purge\nstill release them, so the horizon ends as ADR 0024 intends.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 3: hold every substrate to it

**Files:**
- Modify: `crates/gonzalo-core/src/conformance.rs`

**Interfaces:**
- Consumes: `Record.deleted_kind` and retained manifest bodies (Task 1).

**Why conformance:** the existing `tombstone_pins_the_deleted_records_blob` lives here because "a substrate that drops the field on the way to storage frees bytes a peer can still resurrect the record from". A retained body and `deleted_kind` carry exactly the same hazard, and this suite is the only test that reaches fs, S3 and the daemon alike.

- [ ] **Step 1: Write the case and register it**

Add to `crates/gonzalo-core/src/conformance.rs`, and register it inside `run_tombstone_conformance` beside `tombstone_pins_the_deleted_records_blob`, in that function's existing call style.

```rust
/// Deleting a manifest kind retains its body and records the kind, and the
/// substrate round-trips both. A manifest's body is the only record of which
/// blob was which shard, so a substrate that drops either on the way to storage
/// unpins every shard and loses the mapping that would let an operator put the
/// index back (ADR 0029).
async fn tombstone_retains_a_manifest_body<S: Store>(store: &S) {
    let key = tomb_key("manifest");
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

    let t = store.get_raw(&key).await.unwrap().expect("tombstone");
    assert!(t.is_tombstone());
    assert_eq!(t.body, body, "the manifest body survives the round trip");
    assert_eq!(
        t.deleted_kind,
        Some(RecordKind::VectorManifest),
        "a substrate that drops deleted_kind unpins every shard"
    );
}
```

The case keys off `tomb_key`, not `VectorManifest::key`, so it stays inside the suite's own prefix; `tombstone_of` branches on `kind`, not on the key, so that costs nothing.

- [ ] **Step 2: Run it against one substrate**

Run: `cargo test -p gonzalo-store-fs conformance`
Expected: PASS once Task 1 is in. This is a guard case over Task 1's work, so **if it passes on the first run, say so plainly rather than inventing a failure** — its value is that fs, S3 and the daemon all run it. Before Task 1 it would not compile, which is the red state.

- [ ] **Step 3: Qualify the stale doc comment**

`tombstone_pins_the_deleted_records_blob`'s doc says "The tombstone's body is empty, so this pin is the only thing standing between the content and the next blob sweep". That is now true only of non-manifest kinds. Reword it to say so; the case itself still uses a blob-bodied record and needs no change.

- [ ] **Step 4: Run the suites and commit**

Run: `cargo test -p gonzalo-store-fs`, `cargo test -p gonzalo-core --all-features`, then the full gate. The S3 conformance path needs a live endpoint and is skipped locally — say so in your report rather than claiming it ran.

```bash
git add crates/gonzalo-core/src/conformance.rs
git commit -m "$(printf 'test(core): conformance for a retained manifest tombstone body (#327)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 4: `undelete`

**Files:**
- Create: `crates/gonzalo-core/src/undelete.rs`, `crates/gonzalo-store-fs/tests/undelete.rs`
- Modify: `crates/gonzalo-core/src/lib.rs`, `crates/gonzalo-vector/tests/durability.rs`

**Interfaces:**
- Consumes: `Record.deleted_kind` (Task 1); `BlobStore::has_blob`; `Store::get_raw`, `Store::put_raw`.
- Produces:
  ```rust
  pub async fn undelete<S>(store: &S, key: &RecordKey, now_ms: i64, author: Option<&Identity>)
      -> Result<Revision>
  where S: Store + BlobStore + ?Sized;
  ```

**Where the tests live, and why:** `undelete` is bounded by `Store + BlobStore`, and nothing inside `gonzalo-core` satisfies both — `MemStore` implements `Store` only, and core's dev-dependencies are tokio alone, so `FsStore` is unavailable there (it would also be a dependency cycle). The tests therefore go in `crates/gonzalo-store-fs/tests/undelete.rs`, which already depends on core and has `tempfile`. That is the better home anyway: `FsStore` routes writes through the real `plan_put_raw`, so the no-re-stamp and ancestor-folding assertions are meaningful rather than assertions about a hand-written double.

**The mechanism needs no new write path.** `plan_put_raw`'s `(Some(c), Some(e)) if e == c.revision` arm writes a non-tombstone record over a tombstone, folds the tombstone into `ancestors`, and re-stamps nothing — which is what preserves `meta.created`. The existing conformance cases `replication_overwrite_of_tombstone`, `put_raw_never_restamps` and `put_raw_keeps_the_source_times` already prove that write on every substrate, so these tests cover `undelete`'s own decisions, not `put_raw`'s behaviour.

- [ ] **Step 1: Write the failing tests**

Create `crates/gonzalo-store-fs/tests/undelete.rs`. Model the imports on `tests/blob_gc.rs`; this file needs its own `fresh_store`/`meta`, since separate test binaries share nothing.

```rust
//! `undelete` over a real store with real blobs (ADR 0029).

use gonzalo_core::{
    BlobStore, Body, ContentHash, CoreError, DeleteResult, Identity, Meta, PutResult, Record,
    RecordKey, RecordKind, Store, VectorManifest, undelete,
};
use gonzalo_store_fs::FsStore;

fn fresh_store() -> FsStore {
    let dir = tempfile::tempdir().expect("tempdir");
    FsStore::new(dir.keep())
}

fn meta() -> Meta {
    Meta::new(Identity::new("tester"), "test")
}

fn manifest_key() -> RecordKey {
    VectorManifest::key("ns", "memories")
}

/// A store holding one vector index with a single shard blob.
async fn store_with_index() -> (FsStore, ContentHash) {
    let store = fresh_store();
    let shard = store.put_blob(b"shard bytes").await.unwrap();
    let mut m = VectorManifest::new("space-a", 3, 256);
    m.entries.insert(0, shard.clone());
    let record = Record::create(
        manifest_key(),
        RecordKind::VectorManifest,
        m.to_body(),
        meta(),
    );
    assert!(matches!(
        store.put(record, None).await.unwrap(),
        PutResult::Committed(_)
    ));
    (store, shard)
}

// The capability: delete an index, put it back, and it is the same record.
#[tokio::test]
async fn undelete_restores_a_deleted_manifest() {
    let (store, _shard) = store_with_index().await;
    let before = store.get(&manifest_key()).await.unwrap().unwrap();
    assert_eq!(
        store.delete(&manifest_key(), None).await.unwrap(),
        DeleteResult::Deleted
    );
    assert!(
        store.get(&manifest_key()).await.unwrap().is_none(),
        "hidden while deleted"
    );

    undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap();

    let after = store.get(&manifest_key()).await.unwrap().expect("restored");
    assert_eq!(after.kind, RecordKind::VectorManifest);
    assert_eq!(after.body, before.body, "the manifest comes back intact");
    assert_eq!(after.deleted_at, None);
    assert_eq!(after.deleted_kind, None);
}

// Provenance and lineage: `created` survives, and the restored record succeeds
// the tombstone rather than racing it.
#[tokio::test]
async fn undelete_preserves_created_and_continues_the_chain() {
    let (store, _shard) = store_with_index().await;
    let before = store.get(&manifest_key()).await.unwrap().unwrap();
    store.delete(&manifest_key(), None).await.unwrap();
    let t = store.get_raw(&manifest_key()).await.unwrap().unwrap();

    undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap();

    let after = store.get(&manifest_key()).await.unwrap().unwrap();
    assert_eq!(after.meta.created, before.meta.created, "created survives");
    assert_eq!(after.meta.updated, 9_000);
    assert_eq!(after.revision.counter, t.revision.counter + 1);
    assert_eq!(after.parent, Some(t.revision.clone()));
    assert_eq!(
        after.ancestors.first(),
        Some(&t.revision),
        "put_raw folds the tombstone into the chain"
    );
}

// An explicit author overrides the tombstone's, for a daemon-stamped restore.
#[tokio::test]
async fn undelete_stamps_an_explicit_author() {
    let (store, _shard) = store_with_index().await;
    store.delete(&manifest_key(), None).await.unwrap();

    let operator = Identity::new("operator");
    undelete(&store, &manifest_key(), 9_000, Some(&operator))
        .await
        .unwrap();

    let after = store.get(&manifest_key()).await.unwrap().unwrap();
    assert_eq!(after.meta.author, operator);
}

#[tokio::test]
async fn undelete_refuses_when_there_is_no_tombstone() {
    let store = fresh_store();
    let err = undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn undelete_refuses_when_the_record_is_live() {
    let (store, _shard) = store_with_index().await;
    let err = undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Invalid(_)), "got {err:?}");
}

// A kind whose body was never retained cannot be restored. This is the stated
// asymmetry, and the message has to explain it rather than just failing.
#[tokio::test]
async fn undelete_refuses_a_tombstone_whose_body_was_not_retained() {
    let store = fresh_store();
    let key = RecordKey::new("ns", "coll", "topic");
    let record = Record::create(
        key.clone(),
        RecordKind::Topic,
        Body::Inline(b"{}".to_vec()),
        meta(),
    );
    store.put(record, None).await.unwrap();
    store.delete(&key, None).await.unwrap();

    let err = undelete(&store, &key, 9_000, None).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not retained"),
        "the message must explain why, got {msg}"
    );
}

// Creating a record for an index that cannot be opened is worse than refusing,
// so the blobs are checked before anything is written.
#[tokio::test]
async fn undelete_refuses_when_a_named_blob_is_missing() {
    let (store, shard) = store_with_index().await;
    store.delete(&manifest_key(), None).await.unwrap();
    store.delete_blob(&shard).await.unwrap();

    let err = undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains(&shard.0),
        "the message names the missing hash: {err}"
    );
    assert!(
        store.get(&manifest_key()).await.unwrap().is_none(),
        "nothing was written"
    );
}

// Someone recreated the key while the operator was deciding. The live-record
// guard is what fires here: `undelete` reads the key and finds a non-tombstone.
// (`put_raw` returning Conflict is the narrower race where a recreate lands
// between that read and the write; a single-threaded test cannot reach it, so
// nothing below asserts on it.)
#[tokio::test]
async fn undelete_refuses_when_the_key_was_recreated() {
    let (store, _shard) = store_with_index().await;
    store.delete(&manifest_key(), None).await.unwrap();

    let replacement = Record::create(
        manifest_key(),
        RecordKind::Topic,
        Body::Inline(b"{}".to_vec()),
        meta(),
    );
    store.put(replacement, None).await.unwrap();

    let err = undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Invalid(_)), "got {err:?}");
    let live = store.get(&manifest_key()).await.unwrap().unwrap();
    assert_eq!(
        live.kind,
        RecordKind::Topic,
        "the live record was not clobbered"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p gonzalo-store-fs --test undelete`
Expected: FAIL to compile — `undelete` does not exist in `gonzalo_core`.

- [ ] **Step 3: Implement it**

Create `crates/gonzalo-core/src/undelete.rs` with no test module (see "Where the tests live"):

```rust
//! Restore a record from its tombstone, while the tombstone still exists
//! (ADR 0029).
//!
//! Only the kinds whose body a tombstone retains can be restored — the manifest
//! kinds. Every other tombstone has an empty body, so there is nothing to
//! restore from.
//!
//! This rides [`Store::put_raw`] rather than `put`. A consumer `put` treats a
//! tombstone as absent and re-stamps `meta.created`, which is right for `put` —
//! it cannot tell restoring a deleted record from reusing a recycled key, and
//! must not change meaning depending on whether `collect` has run. `put_raw`
//! never re-stamps, so an explicit restore keeps the record's age. "Raw" means a
//! record that already happened, and a restoration is exactly that.

use crate::{
    BlobStore, ContentHash, CoreError, Identity, Manifest, PutResult, Record, RecordKey,
    RecordKind, Result, Revision, Store, VectorManifest,
};

/// Restore the record at `key` from its tombstone, returning the revision
/// written. `author`, when given, is stamped as the restorer; `None` keeps the
/// tombstone's author.
///
/// Refuses, without writing anything, when there is no tombstone, the record is
/// live, the tombstone's body was not retained, a blob the body names is
/// missing, or the key changed while this ran.
pub async fn undelete<S>(
    store: &S,
    key: &RecordKey,
    now_ms: i64,
    author: Option<&Identity>,
) -> Result<Revision>
where
    S: Store + BlobStore + ?Sized,
{
    let Some(tomb) = store.get_raw(key).await? else {
        return Err(CoreError::NotFound(key.clone()));
    };
    if !tomb.is_tombstone() {
        return Err(CoreError::Invalid(format!(
            "{key} is live at revision {}; there is nothing to restore",
            tomb.revision.counter
        )));
    }
    let Some(kind) = tomb.deleted_kind else {
        return Err(CoreError::Invalid(format!(
            "{key}: the deleted body was not retained, so it cannot be restored. \
             Only manifest kinds retain a body (ADR 0029)."
        )));
    };

    // Check the blobs before writing: a record for an index that cannot be
    // opened is worse than a refusal.
    let mut missing = Vec::new();
    for hash in referenced_blobs(kind, &tomb)? {
        if !store.has_blob(&hash).await? {
            missing.push(hash.0);
        }
    }
    if !missing.is_empty() {
        return Err(CoreError::Invalid(format!(
            "{key}: cannot restore, {} blob(s) the retained body names are gone: {}",
            missing.len(),
            missing.join(", ")
        )));
    }

    let mut meta = tomb.meta.clone();
    if let Some(author) = author {
        meta.author = author.clone();
    }
    meta.updated = now_ms;

    let restored = Record {
        key: key.clone(),
        kind,
        revision: Revision {
            counter: tomb.revision.counter + 1,
            hash: ContentHash::of(tomb.body.bytes()),
        },
        parent: Some(tomb.revision.clone()),
        body: tomb.body.clone(),
        meta,
        links: Vec::new(),
        // `plan_put_raw` folds the tombstone in.
        ancestors: Vec::new(),
        deleted_at: None,
        deleted_blob: None,
        deleted_kind: None,
    };

    match store.put_raw(restored, Some(tomb.revision.clone())).await? {
        PutResult::Committed(rev) => Ok(rev),
        PutResult::Conflict(c) => Err(CoreError::Invalid(format!(
            "{key}: the key changed while restoring (now revision {}); nothing was written",
            c.current.revision.counter
        ))),
    }
}

/// The blob hashes a retained body names.
fn referenced_blobs(kind: RecordKind, tomb: &Record) -> Result<Vec<ContentHash>> {
    Ok(match kind {
        RecordKind::VectorManifest => VectorManifest::from_body(&tomb.body)?
            .entries
            .into_values()
            .collect(),
        RecordKind::GraphManifest => Manifest::from_body(&tomb.body)?
            .entries
            .into_values()
            .collect(),
        // `deleted_kind` is only ever set for retaining kinds, so this is
        // unreachable through `tombstone_of`; a hand-written record could still
        // get here, and it names no blobs.
        _ => Vec::new(),
    })
}
```

Check `Revision`'s real field names and `Conflict`'s shape before relying on `c.current.revision.counter` — `Conflict` is boxed inside `PutResult::Conflict`, so follow what the other call sites do.

In `crates/gonzalo-core/src/lib.rs`: `pub mod undelete;` and add `undelete` to the re-exports, following how `collect` and `reset` are exported there.

- [ ] **Step 4: Run the tests and the gate**

Run: `cargo test -p gonzalo-store-fs --test undelete`, then the full gate.
Expected: PASS, 8 tests.

- [ ] **Step 5: Write the end-to-end acceptance test**

The tests above prove the record comes back. This one proves the *index* comes back — the claim the whole ticket rests on — and it runs a sweep **between** the delete and the restore, so it also proves the shards survived. Add it to `crates/gonzalo-vector/tests/durability.rs`, which already opens a `RecordVectorIndex` over an `FsStore` and reuses that file's `DIM`. Extend its imports with `DeleteResult`, `Store`, `BlobStore`, `SweepPolicy`, `gc_blobs_with`, `now_ms` and `undelete` from `gonzalo_core`, plus `std::time::{Duration, SystemTime}`.

```rust
/// A deleted vector index is recoverable for the tombstone's lifetime: the
/// shards survive a sweep taken while it is deleted, and after `undelete` the
/// index opens and answers queries (ADR 0029).
#[tokio::test]
async fn a_deleted_index_can_be_undeleted_and_queried() {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    let key = VectorManifest::key("ns", "restored");

    let mut probe = vec![0.0; DIM];
    probe[3] = 1.0;

    let idx = RecordVectorIndex::open(
        FsStore::new(dir.path()),
        key.clone(),
        "restored-space",
        DIM,
    )
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

    // A sweep with no grace period, while the index is deleted. The tombstone's
    // retained body is the only thing keeping these shards alive.
    let swept = gc_blobs_with(
        &store,
        SweepPolicy {
            min_age: Duration::ZERO,
            now: SystemTime::now(),
        },
    )
    .await
    .unwrap();
    assert!(swept.freed.is_empty(), "the shards were pinned");

    gonzalo_core::undelete(&store, &key, now_ms(), None)
        .await
        .unwrap();

    let reopened =
        RecordVectorIndex::open(FsStore::new(dir.path()), key, "restored-space", DIM)
            .await
            .unwrap();
    assert_eq!(
        reopened.keys(&KeyPrefix::default()).await.unwrap().len(),
        1
    );
    let hits = reopened
        .query(&probe, 1, &KeyPrefix::default())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "the restored index answers queries");
}
```

If `RecordVectorIndex::open` creates a fresh manifest when the key holds a tombstone rather than failing, note that in your report — it would mean a reopen before the restore silently starts an empty index, which is worth knowing even though this test does not depend on it.

Run: `cargo test -p gonzalo-vector --test durability`
Expected: PASS. This test fails without Tasks 2 and 4 both in place, so run it last.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "$(printf 'feat(core): undelete restores a manifest from its tombstone (#327)\n\nRides put_raw, which never re-stamps, so meta.created survives and the record\nsucceeds the tombstone. Refuses rather than writing when the key is live, was\nrecreated, has no retained body, or names a blob that is gone.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 5: `gonzalo undelete`

**Files:**
- Modify: `crates/gonzalo-cli/src/lib.rs`, `crates/gonzalo-cli/src/main.rs`, `crates/gonzalo-cli/tests/cli.rs`, `crates/gonzalo/src/lib.rs`

**Interfaces:**
- Consumes: `gonzalo_core::undelete` (Task 4).
- Produces: `gonzalo_cli::undelete(root, namespace, collection, id) -> Result<Revision>`; the `gonzalo undelete` subcommand; `gonzalo::undelete` re-exported from the facade.

**Exit codes:** every refusal returns `Err`, which `main` surfaces as exit 1 with the message on stderr. Do **not** reuse `EXIT_CONFLICT` (3): that code exists for `delete`'s `--expected` mismatch, where the caller is meant to re-read and retry with a new revision. `undelete` takes no `--expected`, and "the key is live again" is terminal, not retryable.

- [ ] **Step 1: Write the failing test**

Add to `crates/gonzalo-cli/tests/cli.rs`. Its `run(root, args)` helper appends `--root <root>` itself, so the args array must not contain `--root`. The test turns a seeded record into a `GraphManifest` with `edit_record_file`, because `migrate` only writes ordinary kinds — and an **empty** manifest names no blobs, so the pre-write `has_blob` check passes with nothing to seed.

```rust
#[test]
fn undelete_restores_a_deleted_manifest_and_refuses_a_live_one() {
    let root = TempDir::new().unwrap();
    seed(root.path(), "ns", "col", &["note.md"]);

    // Make it a retaining kind: only manifest kinds keep their body (#327).
    let body = serde_json::to_value(gonzalo_core::Manifest::new().to_body()).unwrap();
    edit_record_file(
        &record_file(root.path(), "ns", "col", "note.md"),
        |record| {
            record.insert("kind".into(), serde_json::json!("GraphManifest"));
            record.insert("body".into(), body);
        },
    );

    let args = [
        "undelete",
        "--namespace",
        "ns",
        "--collection",
        "col",
        "--id",
        "note.md",
    ];

    // A live record cannot be restored.
    let live = run(root.path(), &args);
    assert_eq!(live.status.code(), Some(1), "{live:?}");
    assert!(
        stderr(&live).contains("ns/col/note.md"),
        "the refusal names the key, got {:?}",
        stderr(&live)
    );

    let del = run(
        root.path(),
        &[
            "delete",
            "--namespace",
            "ns",
            "--collection",
            "col",
            "--id",
            "note.md",
        ],
    );
    assert_eq!(del.status.code(), Some(0), "{del:?}");
    assert!(
        !run(root.path(), &["get", "ns", "col", "note.md"])
            .status
            .success()
    );

    let out = run(root.path(), &args);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        stdout(&out).starts_with("restored: ns/col/note.md"),
        "got {:?}",
        stdout(&out)
    );
    assert!(
        run(root.path(), &["get", "ns", "col", "note.md"])
            .status
            .success(),
        "the record reads as present again"
    );

    // Restoring twice refuses, because the record is live again.
    let again = run(root.path(), &args);
    assert_eq!(again.status.code(), Some(1), "{again:?}");
}

#[test]
fn undelete_of_a_key_that_was_never_written_exits_nonzero() {
    let root = TempDir::new().unwrap();
    let out = run(
        root.path(),
        &[
            "undelete",
            "--namespace",
            "ns",
            "--collection",
            "col",
            "--id",
            "nope",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "nothing on stdout when nothing was restored");
    assert!(
        stderr(&out).contains("not found"),
        "got {:?}",
        stderr(&out)
    );
}
```

The remaining refusals — a missing blob, and a tombstone with no retained body — are pinned at the core level in Task 4 and are not worth re-seeding through the CLI; the two above are the ones an operator actually types by mistake.

`edit_record_file` rewrites the stored JSON, so the record's `revision.hash` no longer matches its body. Nothing on this path verifies that — `delete` without `--expected` does not, and neither does `undelete`. If some check elsewhere rejects the edited record, patch `revision` in the same closure rather than abandoning the happy path.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p gonzalo-cli undelete`
Expected: FAIL — the `undelete` subcommand does not exist, so clap exits with a usage error (code 2).

- [ ] **Step 3: Add the wrapper**

In `crates/gonzalo-cli/src/lib.rs`, beside the existing `delete` wrapper — read it and match its shape, error type and how it builds the store:

```rust
/// Restore the record at `namespace/collection/id` from its tombstone.
pub async fn undelete(
    root: &Path,
    namespace: &str,
    collection: &str,
    id: &str,
) -> Result<Revision> {
    let store = FsStore::new(root);
    let key = RecordKey::new(namespace, collection, id);
    Ok(gonzalo_core::undelete(&store, &key, gonzalo_core::now_ms(), None).await?)
}
```

- [ ] **Step 4: Add the subcommand**

In `crates/gonzalo-cli/src/main.rs`, beside `Commands::Delete`:

```rust
    /// Restore a record from its tombstone, while the tombstone still exists.
    /// Only manifest kinds retain a body, so only they can be restored. Fails
    /// if the record is live, the key was recreated, the tombstone has already
    /// been collected, or a blob the retained body names is gone.
    Undelete {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
        /// Namespace of the record.
        #[arg(long)]
        namespace: String,
        /// Collection of the record.
        #[arg(long)]
        collection: String,
        /// Id of the record.
        #[arg(long)]
        id: String,
    },
```

and its handler beside the `Delete` one, matching how that arm prints and returns:

```rust
        Commands::Undelete {
            root,
            namespace,
            collection,
            id,
        } => {
            let key = RecordKey::new(&namespace, &collection, &id);
            let revision = undelete(&root, &namespace, &collection, &id).await?;
            println!("restored: {key}");
            println!("revision: {}", serde_json::to_string(&revision)?);
        }
```

Add `undelete` to the `use gonzalo_cli::{…}` list at the top of `main.rs`. Copy the `root` argument's attributes from `Commands::Delete` verbatim rather than trusting the snippet above — `store_root` and the default must match that arm exactly.

- [ ] **Step 5: Re-export from the facade**

In `crates/gonzalo/src/lib.rs`, add `undelete` to the root `pub use gonzalo_core::{…}` block — the same one exporting `collect`, `reset` and `gc_blobs`. If a test there pins that root surface, extend it so the new name is pinned like its neighbours.

- [ ] **Step 6: Run the tests and the gate**

Run: `cargo test -p gonzalo-cli`, `cargo test -p gonzalo --all-features`, then the full gate.
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates
git commit -m "$(printf 'feat(cli): gonzalo undelete (#327)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 6: ADR 0029 and the docs

**Files:**
- Create: `docs/adr/0029-manifest-tombstone-pin.md`
- Modify: `docs/adr/README.md`, `docs/adr/0021-replicated-deletion-with-tombstones.md`, `docs/adr/0024-blob-garbage-collection.md`, `docs/adr/0027-durable-vector-index.md`
- Modify: `docs/guide/src/deletion.md`, `docs/guide/src/storage.md`, `CHANGELOG.md`, `docs/api/openapi.json`

- [ ] **Step 1: Write ADR 0029**

Read `docs/adr/0028-blob-gc-grace-period.md` first and match its header lines and section order, and `docs/adr/template.md` for the Consequences bullet style. Status `accepted`, date `2026-10-03`. The body must stand alone — no "see the spec". Cover:

- **Context:** ADR 0024's three-step promise; `tombstone_of` discarded the body, so a manifest's shard hashes left the mark set *and* the shard-to-blob mapping was destroyed; recoverable for a graph slice, permanent for a vector shard (#317's caller-supplied vectors); reachable through `gonzalo delete` on a manifest key and through `reset`, which tombstones every key in a namespace; `AncestryStore` retains bodies but is never constructed outside its own tests, so no production path recovers a discarded body.
- **Why pinning hashes alone was rejected:** the mapping is gone, so preserved bytes are unnameable, and a genuinely shared shard is already marked by the other index's manifest.
- **Decision:** manifest kinds retain their body and record `deleted_kind`; GC marks through it; `undelete` rides `put_raw`; manifest kinds only; `collect` unchanged.
- **Why `put_raw` and not a new planner arm:** `plan_put_raw` already writes a non-tombstone over a tombstone on a matching revision and never re-stamps, which is what preserves `meta.created`; a third arm in the planner every substrate routes through would add a decision path for no behaviour the existing one lacks. Say plainly that undelete rides the replication planner, so nobody is startled to find it there.
- **Why `put` keeps resetting `created`:** it cannot distinguish restoring a deleted record from reusing a recycled key, and preserving it would make an identical call mean different things depending on whether `collect` had run.
- **Consequences, positive:** ADR 0024's promise now holds for manifests; a deleted index is recoverable for the horizon; no new wire surface, trait method or planner arm.
- **Consequences, negative:** a manifest tombstone is no longer small — it carries the manifest's JSON until collected; only manifest kinds can be undeleted; past `collect` a deleted index is gone; #325's residual sweep windows still apply, so a sweep that decided before the tombstone existed will still delete the shards (#328); `undelete`'s `PutResult::Conflict` arm is a race guard no single-threaded test reaches; and #198's drift check compares paths and methods only, so the `deleted_kind` addition to the `Record` schema in `docs/api/openapi.json` is unguarded by a test.
- **Revisit if:** a non-manifest kind needs to be undeletable; manifest tombstones grow costly; `put` ever needs to express succession explicitly.

- [ ] **Step 2: Annotate the amendments on both sides**

ADR 0029 **amends** 0021 (tombstone shape) and 0024 (what the pin covers). Neither is superseded; both stay `accepted`.

- `docs/adr/README.md`: add the 0029 row, and extend 0021's and 0024's status cells with back-references in the style already used — ADR 0012's row (`accepted (blob GC marking amended by [0024](…))`) is the model.
- `docs/adr/0021-replicated-deletion-with-tombstones.md` and `docs/adr/0024-blob-garbage-collection.md`: add a one-line "Amended by" note each. Change nothing else.
- `docs/adr/0027-durable-vector-index.md`: its consequence that deleting a vector manifest does not pin its shards is now **resolved**. Rewrite it to point at 0029 and drop the `#327` open-work reference.

- [ ] **Step 3: Update the guide and changelog**

- `docs/guide/src/deletion.md`: the manifest case in the delete → collect → gc story, and `gonzalo undelete` with its refusals.
- `docs/guide/src/storage.md`: deleting a vector index is recoverable within the horizon.
- `CHANGELOG.md`, under the unreleased heading and in the existing style: the additive `deleted_kind` field; manifest tombstones retain their body; `gonzalo gc` marks through it; `undelete` in core, the CLI and the facade.
- `docs/api/openapi.json`: a `deleted_kind` property on the `Record` schema, additive, documented as tombstones-only and saying what it is for. Match how `deleted_blob` is described there.

- [ ] **Step 4: Validate the ADR set**

Check and report each as pass or fail: body/index status parity; both-sided annotation for 0021↔0029 and 0024↔0029; 0029's body self-sustaining; every path and inter-ADR link 0029 cites resolving (test each with a command); no gaps or duplicates across 0001–0029; 0029's header lines, sections and filename conforming to the template.

- [ ] **Step 5: Run the full gate and commit**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features
git add docs CHANGELOG.md
git commit -m "$(printf 'docs(core): ADR 0029 for the manifest tombstone pin (#327)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```
