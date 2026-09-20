# S3 Tombstone Markers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the S3 consumer `list` from issuing a `GetObject` per key to hide tombstones, by making a tombstone visible in the listing itself.

**Architecture:** A delete writes a zero-byte marker object beside the record at `<object key>.tombstone`; the record object itself does not move, so every compare-and-swap stays a single conditional write. `list` makes one `ListObjectsV2` pass, sorts record keys from marker keys as it goes, and reads only marked keys. A reader may trust the *absence* of a marker only in a collection flagged at `ns/col/_tombstone_markers`, which the first full listing of that collection backfills and sets.

**Tech Stack:** Rust, `aws-sdk-s3`, `tokio`, RustFS for integration tests (`scripts/rustfs-up.sh`).

**Spec:** [`docs/superpowers/specs/2026-09-19-s3-tombstone-marker-layout-design.md`](../specs/2026-09-19-s3-tombstone-marker-layout-design.md), recorded as [ADR 0025](../../adr/0025-s3-tombstone-markers.md).

## Global Constraints

- **The invariant is one-directional: a tombstone implies a marker; a marker implies nothing.** Write the marker *before* the tombstone; remove it *after* the live record or the purge. Never the other way round.
- **A stale marker must never change a result** — only cost one `GetObject`. Any code path that finds one either ignores it or deletes it.
- **`list_raw` is not touched.** It never filtered, so it never read.
- **Never trust a missing marker in an unflagged collection.** Until `ns/col/_tombstone_markers` exists, `list` reads every key exactly as it does today.
- Marker and flag keys must be unproducible by `object_key`: rely on `segment()` escaping `.` (`crates/gonzalo-core/src/paths.rs:21-34`) and on `parse_object_key` requiring exactly three parts ending in `.json`.
- The full local gate must pass before each commit: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`.
- S3 integration tests skip without `GONZALO_S3_TEST_ENDPOINT`/`GONZALO_S3_TEST_BUCKET`. Run them with RustFS up: `eval "$(scripts/rustfs-up.sh)" && cargo test -p gonzalo-store-s3 --test integration`.

---

### Task 1: Marker and flag key derivation

**Files:**
- Modify: `crates/gonzalo-store-s3/src/lib.rs` (beside `parse_object_key`, ~line 758)

**Interfaces:**
- Consumes: `gonzalo_core::{object_key, segment, RecordKey}`, existing `parse_object_key`.
- Produces:
  - `const MARKER_SUFFIX: &str = ".tombstone";`
  - `fn marker_key(key: &RecordKey) -> String`
  - `fn parse_marker_key(s: &str) -> Option<RecordKey>`
  - `fn marked_flag_key(namespace: &str, collection: &str) -> String`

- [ ] **Step 1: Write the failing tests**

In the existing `mod tests` of `crates/gonzalo-store-s3/src/lib.rs`:

```rust
#[test]
fn marker_key_is_the_record_key_plus_a_suffix() {
    let k = RecordKey::new("ns", "col", "id");
    assert_eq!(marker_key(&k), "ns/col/id.json.tombstone");
    assert_eq!(parse_marker_key(&marker_key(&k)), Some(k));
}

#[test]
fn a_marker_key_is_not_a_record_key() {
    // `segment()` escapes `.`, so no id can produce a key ending in
    // `.json.tombstone` — the marker namespace is disjoint by construction.
    let k = RecordKey::new("ns", "col", "id");
    assert_eq!(parse_object_key(&marker_key(&k)), None);
    for id in ["id.json.tombstone", "id.json", "a.b", "..", "50%"] {
        let key = RecordKey::new("ns", "col", id);
        assert_ne!(object_key(&key), marker_key(&RecordKey::new("ns", "col", "id")));
        assert_eq!(parse_marker_key(&object_key(&key)), None);
    }
}

#[test]
fn marked_flag_key_is_not_a_record_or_marker_key() {
    let flag = marked_flag_key("ns", "col");
    assert_eq!(flag, "ns/col/_tombstone_markers");
    assert_eq!(parse_object_key(&flag), None);
    assert_eq!(parse_marker_key(&flag), None);
}

#[test]
fn flag_key_escapes_its_components() {
    // Same encoding as record keys, so a namespace containing `/` cannot
    // reach another collection's flag.
    assert_eq!(marked_flag_key("a/b", "c.d"), "a%2Fb/c%2Ed/_tombstone_markers");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p gonzalo-store-s3 --lib marker`
Expected: FAIL — `cannot find function 'marker_key' in this scope`.

- [ ] **Step 3: Implement**

```rust
/// Suffix appended to a record's object key to form its tombstone marker
/// (ADR 0025). `segment()` escapes `.`, so no encoded id can end in this and
/// the two key spaces are disjoint by construction.
const MARKER_SUFFIX: &str = ".tombstone";

/// The marker object key for `key`: `namespace/collection/id.json.tombstone`.
/// A zero-byte object at this key means "the record here may be a tombstone —
/// read it". Its absence is only meaningful in a flagged collection.
fn marker_key(key: &RecordKey) -> String {
    format!("{}{MARKER_SUFFIX}", object_key(key))
}

/// The record a marker object belongs to, or `None` if `s` is not a marker.
fn parse_marker_key(s: &str) -> Option<RecordKey> {
    parse_object_key(s.strip_suffix(MARKER_SUFFIX)?)
}

/// The per-collection flag object key. Its presence says markers have always
/// been maintained here, so `list` may treat an unmarked key as live. The name
/// has no dot and no `.json`, so `parse_object_key` can never produce it.
fn marked_flag_key(namespace: &str, collection: &str) -> String {
    format!(
        "{}/{}/_tombstone_markers",
        gonzalo_core::segment(namespace),
        gonzalo_core::segment(collection)
    )
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p gonzalo-store-s3 --lib marker`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-store-s3/src/lib.rs
git commit -m "feat(store-s3): derive tombstone marker and marked-flag keys (#294)"
```

---

### Task 2: Maintain markers on the write paths

**Files:**
- Modify: `crates/gonzalo-store-s3/src/lib.rs` — `write_planned` (~line 227) and new helpers beside `delete_record_if_match` (~line 198)
- Test: `crates/gonzalo-store-s3/tests/integration.rs`

**Interfaces:**
- Consumes: `marker_key` (Task 1), `Record::is_tombstone()`, `WriteOutcome`, `Planned`.
- Produces:
  - `async fn put_marker(&self, key: &RecordKey) -> Result<()>`
  - `async fn delete_marker(&self, key: &RecordKey) -> Result<()>`
  - `pub async fn marker_exists(&self, key: &RecordKey) -> Result<bool>` — public so the integration tests can assert layout state without an S3 client of their own.

- [ ] **Step 1: Write the failing tests**

In `crates/gonzalo-store-s3/tests/integration.rs`:

```rust
#[tokio::test]
async fn delete_writes_a_marker_and_recreation_removes_it() {
    let Some((endpoint, _)) = test_target() else { return };
    let store = fresh_bucket_store(&endpoint).await;
    let key = RecordKey::new("ns", "col", "marked");

    let rev = committed(store.put(sample(key.clone(), b"v1", Revision::initial(b"v1"), None), None).await);
    assert!(!store.marker_exists(&key).await.unwrap(), "a live record has no marker");

    assert_eq!(store.delete(&key, Some(rev)).await.unwrap(), DeleteResult::Deleted);
    assert!(store.marker_exists(&key).await.unwrap(), "a tombstone must have a marker");

    // Recreating the key makes it live again, so the marker must go.
    committed(store.put(sample(key.clone(), b"v2", Revision::initial(b"v2"), None), None).await);
    assert!(!store.marker_exists(&key).await.unwrap(), "a recreation clears the marker");
}

#[tokio::test]
async fn purge_removes_the_marker_with_the_tombstone() {
    let Some((endpoint, _)) = test_target() else { return };
    let store = fresh_bucket_store(&endpoint).await;
    let key = RecordKey::new("ns", "col", "purged");

    let rev = committed(store.put(sample(key.clone(), b"v1", Revision::initial(b"v1"), None), None).await);
    assert_eq!(store.delete(&key, Some(rev)).await.unwrap(), DeleteResult::Deleted);
    let tomb = store.get_raw(&key).await.unwrap().expect("tombstone");
    assert!(store.marker_exists(&key).await.unwrap());

    assert_eq!(store.purge(&key, tomb.revision).await.unwrap(), DeleteResult::Deleted);
    assert!(!store.marker_exists(&key).await.unwrap(), "purge leaves no orphan marker");
}

#[tokio::test]
async fn replicating_a_tombstone_writes_a_marker() {
    // put_raw is the replication write: a tombstone arriving from a peer must
    // be marked exactly like a locally-written one, or the receiving store's
    // list would show the deleted record as live.
    let Some((endpoint, _)) = test_target() else { return };
    let store = fresh_bucket_store(&endpoint).await;
    let source = fresh_bucket_store(&endpoint).await;
    let key = RecordKey::new("ns", "col", "replicated");

    let rev = committed(source.put(sample(key.clone(), b"v1", Revision::initial(b"v1"), None), None).await);
    assert_eq!(source.delete(&key, Some(rev)).await.unwrap(), DeleteResult::Deleted);
    let tomb = source.get_raw(&key).await.unwrap().expect("tombstone");

    committed(store.put_raw(tomb, None).await);
    assert!(store.marker_exists(&key).await.unwrap());
}
```

Add the helper used above, next to `sample`:

```rust
/// The revision a committed write landed at, or a panic naming the conflict.
fn committed(result: Result<PutResult, CoreError>) -> Revision {
    match result.expect("put") {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `eval "$(scripts/rustfs-up.sh)" && cargo test -p gonzalo-store-s3 --test integration marker`
Expected: FAIL — `no method named 'marker_exists'`.

- [ ] **Step 3: Implement the helpers**

Beside `delete_record_if_match` in `crates/gonzalo-store-s3/src/lib.rs`:

```rust
/// Write the zero-byte marker for `key` (ADR 0025). Unconditional: the marker
/// carries no version, and writing one that already exists is a no-op.
async fn put_marker(&self, key: &RecordKey) -> Result<()> {
    self.client
        .put_object()
        .bucket(&self.bucket)
        .key(marker_key(key))
        .body(Vec::new().into())
        .send()
        .await
        .map(|_| ())
        .map_err(|e| CoreError::Backend(e.into_service_error().to_string()))
}

/// Remove the marker for `key`. Idempotent: S3 `DeleteObject` succeeds on an
/// absent key, so this is safe to call without checking first.
async fn delete_marker(&self, key: &RecordKey) -> Result<()> {
    self.client
        .delete_object()
        .bucket(&self.bucket)
        .key(marker_key(key))
        .send()
        .await
        .map(|_| ())
        .map_err(|e| CoreError::Backend(e.into_service_error().to_string()))
}

/// Whether `key` currently carries a tombstone marker. Exposed for tests and
/// operational inspection of the layout; `list` uses the listing instead, which
/// is the whole point of the marker.
pub async fn marker_exists(&self, key: &RecordKey) -> Result<bool> {
    match self
        .client
        .head_object()
        .bucket(&self.bucket)
        .key(marker_key(key))
        .send()
        .await
    {
        Ok(_) => Ok(true),
        Err(e) => {
            let svc = e.into_service_error();
            if svc.is_not_found() {
                Ok(false)
            } else {
                Err(CoreError::Backend(svc.to_string()))
            }
        }
    }
}
```

- [ ] **Step 4: Hook them into `write_planned`**

In the `Planned::Put(record, answer)` arm, before the write, and after an applied write:

```rust
Planned::Put(record, answer) => {
    // The marker goes first: a crash after this leaves a stale marker (one
    // wasted read), while a crash after the tombstone would leave one
    // unmarked — the only state a flagged collection cannot survive.
    if record.is_tombstone() {
        self.put_marker(key).await?;
    }
    let replaced_tombstone = current
        .as_ref()
        .is_some_and(|(rec, _)| rec.is_tombstone());
    let outcome = self.put_record_if(&record, precondition(etag)).await?;
    if let WriteOutcome::LostRace(_) = outcome {
        *pending.lock().unwrap() = Some((record, answer));
        return Ok(match outcome {
            WriteOutcome::LostRace(kind) => Step::Retry(kind),
            WriteOutcome::Applied => unreachable!("checked above"),
        });
    }
    // The key is live again, so nothing pins its marker. Only on a
    // recreation: a plain update never had one.
    if replaced_tombstone && !record.is_tombstone() {
        self.delete_marker(key).await?;
    }
    (outcome, answer)
}
```

And in the `Planned::Remove(answer)` arm, after the delete applies:

```rust
Planned::Remove(answer) => {
    let tag = etag.unwrap_or_default().to_string();
    let outcome = self.delete_record_if_match(key, tag).await?;
    if let WriteOutcome::Applied = outcome {
        // Purge removed the record the marker pointed at.
        self.delete_marker(key).await?;
    }
    (outcome, answer)
}
```

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p gonzalo-store-s3 --test integration marker` and then the whole file: `cargo test -p gonzalo-store-s3 --test integration`
Expected: PASS, conformance included.

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-store-s3
git commit -m "feat(store-s3): write a tombstone marker beside the record (#294)"
```

---

### Task 3: The per-collection marked flag

**Files:**
- Modify: `crates/gonzalo-store-s3/src/lib.rs` — `S3Store` struct (~line 18), `new`, `handle` (~line 65)
- Test: `crates/gonzalo-store-s3/tests/integration.rs`

**Interfaces:**
- Consumes: `marked_flag_key` (Task 1).
- Produces:
  - field `marked: Arc<RwLock<BTreeSet<(String, String)>>>` — encoded `(namespace, collection)` pairs known flagged
  - `async fn collection_marked(&self, namespace: &str, collection: &str) -> Result<bool>`
  - `async fn mark_collection(&self, namespace: &str, collection: &str) -> Result<()>`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_marked_flag_is_sticky_and_shared_across_handles() {
    let Some((endpoint, _)) = test_target() else { return };
    let store = fresh_bucket_store(&endpoint).await;
    assert!(!store.collection_marked("ns", "col").await.unwrap());

    store.mark_collection("ns", "col").await.unwrap();
    assert!(store.collection_marked("ns", "col").await.unwrap());
    // A sibling collection is unaffected: the flag is per collection.
    assert!(!store.collection_marked("ns", "other").await.unwrap());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `eval "$(scripts/rustfs-up.sh)" && cargo test -p gonzalo-store-s3 --test integration marked_flag`
Expected: FAIL — `no method named 'collection_marked'`.

- [ ] **Step 3: Implement**

Add to the struct and both constructors (`new` sets `marked: Arc::new(RwLock::new(BTreeSet::new()))`; `handle` **clones the `Arc`**, not the set):

```rust
pub struct S3Store {
    client: Client,
    bucket: String,
    cap: usize,
    /// Encoded `(namespace, collection)` pairs known to carry the marked flag
    /// (ADR 0025). Shared across `handle()` clones, and only ever added to:
    /// the flag is set once and never cleared, so a cached `true` cannot go
    /// stale. An unmarked collection is deliberately not cached — it re-checks,
    /// which costs one `HeadObject` against a path already paying N reads.
    marked: Arc<RwLock<BTreeSet<(String, String)>>>,
}
```

```rust
/// Whether `list` may treat an unmarked key in this collection as live.
async fn collection_marked(&self, namespace: &str, collection: &str) -> Result<bool> {
    let pair = (
        gonzalo_core::segment(namespace),
        gonzalo_core::segment(collection),
    );
    if self.marked.read().unwrap().contains(&pair) {
        return Ok(true);
    }
    let found = match self
        .client
        .head_object()
        .bucket(&self.bucket)
        .key(marked_flag_key(namespace, collection))
        .send()
        .await
    {
        Ok(_) => true,
        Err(e) => {
            let svc = e.into_service_error();
            if svc.is_not_found() {
                false
            } else {
                return Err(CoreError::Backend(svc.to_string()));
            }
        }
    };
    if found {
        self.marked.write().unwrap().insert(pair);
    }
    Ok(found)
}

/// Record that every tombstone in this collection carries a marker, so later
/// listings may trust a missing marker. Set only by a pass that read every key
/// in the collection and backfilled what was missing.
async fn mark_collection(&self, namespace: &str, collection: &str) -> Result<()> {
    self.client
        .put_object()
        .bucket(&self.bucket)
        .key(marked_flag_key(namespace, collection))
        .body(b"1".to_vec().into())
        .send()
        .await
        .map_err(|e| CoreError::Backend(e.into_service_error().to_string()))?;
    self.marked.write().unwrap().insert((
        gonzalo_core::segment(namespace),
        gonzalo_core::segment(collection),
    ));
    Ok(())
}
```

Make both methods `pub` so the integration test can drive them directly.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p gonzalo-store-s3 --test integration marked_flag`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-store-s3
git commit -m "feat(store-s3): per-collection marked flag, cached across handles (#294)"
```

---

### Task 4: One-pass `list` with the fast path, backfill and self-healing

**Files:**
- Modify: `crates/gonzalo-store-s3/src/lib.rs` — `list_keys` (~line 129) and `Store::list` (~line 516)
- Test: `crates/gonzalo-store-s3/tests/integration.rs`

**Interfaces:**
- Consumes: Tasks 1-3.
- Produces:
  - `struct Listing { records: Vec<RecordKey>, markers: BTreeSet<RecordKey> }`
  - `async fn list_objects(&self, prefix: &KeyPrefix) -> Result<Listing>` — replaces the body of `list_keys`, which becomes `Ok(self.list_objects(prefix).await?.records)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn list_hides_a_tombstone_without_reading_live_records() {
    let Some((endpoint, _)) = test_target() else { return };
    let store = fresh_bucket_store(&endpoint).await;
    let live = RecordKey::new("ns", "col", "live");
    let gone = RecordKey::new("ns", "col", "gone");
    committed(store.put(sample(live.clone(), b"a", Revision::initial(b"a"), None), None).await);
    let rev = committed(store.put(sample(gone.clone(), b"b", Revision::initial(b"b"), None), None).await);
    assert_eq!(store.delete(&gone, Some(rev)).await.unwrap(), DeleteResult::Deleted);

    // First listing is the slow path: it reads every key, backfills, and flags.
    assert_eq!(store.list(&prefix("ns", "col")).await.unwrap(), vec![live.clone()]);
    assert!(store.collection_marked("ns", "col").await.unwrap());

    // Second listing takes the fast path and must agree.
    assert_eq!(store.list(&prefix("ns", "col")).await.unwrap(), vec![live.clone()]);
    // list_raw is unchanged: it still shows the tombstoned key.
    let mut raw = store.list_raw(&prefix("ns", "col")).await.unwrap();
    raw.sort();
    assert_eq!(raw, vec![gone, live]);
}

#[tokio::test]
async fn an_unflagged_collection_hides_tombstones_written_before_the_upgrade() {
    // Seed the way 0.7.0 would: a tombstone at the record's key, no marker and
    // no flag. Trusting the missing marker here would resurrect the record.
    let Some((endpoint, _)) = test_target() else { return };
    let store = fresh_bucket_store(&endpoint).await;
    let live = RecordKey::new("ns", "col", "live");
    let gone = RecordKey::new("ns", "col", "gone");
    committed(store.put(sample(live.clone(), b"a", Revision::initial(b"a"), None), None).await);
    let rev = committed(store.put(sample(gone.clone(), b"b", Revision::initial(b"b"), None), None).await);
    assert_eq!(store.delete(&gone, Some(rev)).await.unwrap(), DeleteResult::Deleted);
    // Undo the marker and the flag to reproduce the old layout exactly.
    store.remove_marker_for_test(&gone).await.unwrap();
    assert!(!store.marker_exists(&gone).await.unwrap());

    assert_eq!(store.list(&prefix("ns", "col")).await.unwrap(), vec![live]);
    // The pass that proved it also repaired it.
    assert!(store.marker_exists(&gone).await.unwrap());
    assert!(store.collection_marked("ns", "col").await.unwrap());
}

#[tokio::test]
async fn a_stale_marker_neither_hides_a_live_record_nor_survives() {
    let Some((endpoint, _)) = test_target() else { return };
    let store = fresh_bucket_store(&endpoint).await;
    let live = RecordKey::new("ns", "col", "live");
    committed(store.put(sample(live.clone(), b"a", Revision::initial(b"a"), None), None).await);
    store.mark_collection("ns", "col").await.unwrap();
    // The crash window: a marker written for a record that is (still) live.
    store.write_marker_for_test(&live).await.unwrap();

    assert_eq!(store.list(&prefix("ns", "col")).await.unwrap(), vec![live.clone()]);
    assert!(!store.marker_exists(&live).await.unwrap(), "list heals the stale marker");
}

#[tokio::test]
async fn an_orphan_marker_is_swept() {
    let Some((endpoint, _)) = test_target() else { return };
    let store = fresh_bucket_store(&endpoint).await;
    let ghost = RecordKey::new("ns", "col", "ghost");
    store.mark_collection("ns", "col").await.unwrap();
    store.write_marker_for_test(&ghost).await.unwrap();

    assert!(store.list(&prefix("ns", "col")).await.unwrap().is_empty());
    assert!(!store.marker_exists(&ghost).await.unwrap());
}
```

Add the prefix helper next to `sample`:

```rust
fn prefix(namespace: &str, collection: &str) -> KeyPrefix {
    KeyPrefix {
        namespace: Some(namespace.into()),
        collection: Some(collection.into()),
    }
}
```

and, on `S3Store`, two test seams that write the states the code cannot produce:

```rust
/// Write a marker for `key` without writing a tombstone — the state a crash
/// between the two writes leaves behind. Test seam for ADR 0025's stale-marker
/// cases; production code always pairs the two.
#[doc(hidden)]
pub async fn write_marker_for_test(&self, key: &RecordKey) -> Result<()> {
    self.put_marker(key).await
}

/// Remove a marker while leaving its tombstone — the pre-ADR-0025 layout.
#[doc(hidden)]
pub async fn remove_marker_for_test(&self, key: &RecordKey) -> Result<()> {
    self.delete_marker(key).await
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `eval "$(scripts/rustfs-up.sh)" && cargo test -p gonzalo-store-s3 --test integration list_`
Expected: FAIL — `no method named 'write_marker_for_test'`, and the upgrade test fails on a resurrected record once the fast path exists.

- [ ] **Step 3: Collect markers in the listing pass**

Replace `list_keys` with `list_objects`, keeping the pagination exactly as it is and adding one arm:

```rust
/// What one `ListObjectsV2` traversal saw: the record keys, and the keys that
/// carry a tombstone marker. Markers are siblings of their records, so the same
/// prefix covers both and no second traversal is needed.
struct Listing {
    records: Vec<RecordKey>,
    markers: BTreeSet<RecordKey>,
}

// inside the pagination loop, replacing the single `if let` over contents:
for obj in resp.contents() {
    let Some(k) = obj.key() else { continue };
    if let Some(key) = parse_object_key(k) {
        if prefix.matches(&key) {
            out.records.push(key);
        }
    } else if let Some(key) = parse_marker_key(k)
        && prefix.matches(&key)
    {
        out.markers.insert(key);
    }
}
```

and keep the old entry point:

```rust
/// Every record key under `prefix`, tombstones included (the raw listing).
async fn list_keys(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
    Ok(self.list_objects(prefix).await?.records)
}
```

- [ ] **Step 4: Rewrite `Store::list`**

```rust
async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
    // One traversal classifies every key (ADR 0025): a marker is a sibling
    // object, so it arrives in the same pages as the record it marks.
    let listing = self.list_objects(prefix).await?;

    // Trust is per collection, so decide per collection. A pass that spans
    // several enumerated each of them fully, and may flag each of them.
    let mut by_collection: BTreeMap<(String, String), Vec<RecordKey>> = BTreeMap::new();
    for key in listing.records {
        by_collection
            .entry((key.namespace.clone(), key.collection.clone()))
            .or_default()
            .push(key);
    }

    let mut out = Vec::new();
    for ((namespace, collection), keys) in by_collection {
        let marked = self.collection_marked(&namespace, &collection).await?;
        // Fast path: read only the keys a marker points at. Slow path: read
        // every key, exactly as before the marker layout existed, and repair
        // what the old layout left behind.
        let to_read: Vec<RecordKey> = if marked {
            keys.iter()
                .filter(|k| listing.markers.contains(k))
                .cloned()
                .collect()
        } else {
            keys.clone()
        };
        let live = self.read_liveness(&to_read).await?;

        for key in keys {
            match live.get(&key) {
                // Not read: unmarked in a flagged collection, so live.
                None => out.push(key),
                Some(true) => {
                    if listing.markers.contains(&key) {
                        // Live but marked: the crash window. Heal it.
                        self.delete_marker(&key).await?;
                    }
                    out.push(key);
                }
                Some(false) => {
                    if !marked && !listing.markers.contains(&key) {
                        // A tombstone the old layout left unmarked. The pass
                        // that proved it is the cheapest place to repair it.
                        self.put_marker(&key).await?;
                    }
                }
            }
        }

        // Markers with no record of their own are orphans from a purge that
        // died between its two deletes.
        for orphan in listing
            .markers
            .iter()
            .filter(|m| m.namespace == namespace && m.collection == collection)
            .filter(|m| !live.contains_key(m) || live.get(m) == Some(&false))
            .filter(|m| !live.contains_key(m))
        {
            self.delete_marker(orphan).await?;
        }

        if !marked {
            self.mark_collection(&namespace, &collection).await?;
        }
    }
    out.sort();
    Ok(out)
}
```

with the bounded-concurrency read lifted into a helper, unchanged in behaviour
from the current `list` body:

```rust
/// Read `keys` in bounded-concurrency batches and report which are live.
/// A key absent from the map was not read. The reads run concurrently because
/// one at a time multiplied every key's latency by a round trip (gonzalo#286).
async fn read_liveness(&self, keys: &[RecordKey]) -> Result<BTreeMap<RecordKey, bool>> {
    let mut live = BTreeMap::new();
    for batch in keys.chunks(LIST_READ_CONCURRENCY) {
        let mut reads = tokio::task::JoinSet::new();
        for key in batch {
            let store = self.handle();
            let key = key.clone();
            reads.spawn(async move {
                let visible = listed_as_live(store.read(&key).await)?;
                Ok::<_, CoreError>((key, visible))
            });
        }
        while let Some(joined) = reads.join_next().await {
            let (key, visible) = joined.map_err(|e| CoreError::Backend(e.to_string()))??;
            live.insert(key, visible);
        }
    }
    Ok(live)
}
```

Note the ordering change this makes explicit: the previous implementation sorted
each batch by input position; `out.sort()` now gives the whole listing one
deterministic order regardless of which reads returned first.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p gonzalo-store-s3 --test integration`
Expected: PASS — the four new cases plus both conformance suites.

- [ ] **Step 6: Simplify the orphan filter**

The filter written in Step 4 is redundant (`!live.contains_key(m)` twice, plus a
contradictory clause). Reduce it to the single condition that matters — a marker
whose record key was not in the listing at all:

```rust
let listed: BTreeSet<&RecordKey> = keys_seen.iter().collect();
for orphan in listing
    .markers
    .iter()
    .filter(|m| m.namespace == namespace && m.collection == collection)
    .filter(|m| !listed.contains(m))
{
    self.delete_marker(orphan).await?;
}
```

where `keys_seen` is the `keys` vector captured before the consuming loop.

- [ ] **Step 7: Run the full gate and commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features
git add crates/gonzalo-store-s3
git commit -m "perf(store-s3): list tombstones from the listing, not a read per key (#294)"
```

---

### Task 5: Benchmark, docs and changelog

**Files:**
- Create: `crates/gonzalo-store-s3/examples/list_bench.rs`
- Modify: `docs/guide/src/storage.md`, `docs/guide/src/deletion.md`, `CHANGELOG.md`

**Interfaces:**
- Consumes: the finished layout.
- Produces: measured before/after numbers for the ticket's acceptance criterion.

- [ ] **Step 1: Write the benchmark**

```rust
//! Measure `Store::list` over a namespace of N live records and M tombstones
//! (gonzalo#294). Run against RustFS:
//!
//!     eval "$(scripts/rustfs-up.sh)"
//!     cargo run -p gonzalo-store-s3 --example list_bench -- 500 200
//!
//! Prints the wall-clock of the first listing (which backfills and flags the
//! collection) and of the second (the fast path). The request count is the
//! durable number: the first pass reads N+M objects, the second reads M.
```

The body seeds `N` live records and `M` deleted ones into a fresh bucket, times
`store.list(&prefix)` twice, and prints both durations with the object counts.

- [ ] **Step 2: Run it and record the numbers**

Run: `eval "$(scripts/rustfs-up.sh)" && cargo run -p gonzalo-store-s3 --example list_bench -- 500 200`
Capture both timings for the PR body and the ADR's Consequences.

- [ ] **Step 3: Update the guide**

`docs/guide/src/storage.md` currently says nothing about what a listing costs on
S3. Add, in the S3 section, that a listing costs one traversal plus one read per
tombstone, that the first listing of a collection after upgrading from 0.7.0
pays the old cost once while it backfills, and link ADR 0025.

`docs/guide/src/deletion.md` — in the section on what collection frees — note
that collecting tombstones also removes their markers, so listings get cheaper
as well as the store getting smaller.

- [ ] **Step 4: Changelog**

Under `## [Unreleased]` → `### Changed`:

```markdown
- **S3 listings stop paying a read per record.** Hiding tombstones from `list`
  cost a `GetObject` per key, because a tombstone lives at the deleted record's
  own key and only its body says so; `reset` and `collect` inherited it. A
  delete now also writes a zero-byte marker beside the record, so one
  `ListObjectsV2` pass classifies every key and only marked keys are read —
  one read per *tombstone*, which `collect` bounds, instead of one per
  *record*, which nothing did. The record object does not move, so every
  compare-and-swap is still a single conditional write. A bucket written by
  0.7.0 upgrades itself: until a collection is flagged, `list` reads every key
  as before and backfills the missing markers as it goes. See
  [ADR 0025](docs/adr/0025-s3-tombstone-markers.md). (#294)
```

- [ ] **Step 5: Full gate and commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features
git add -A
git commit -m "docs(store-s3): document the marker layout and its listing cost (#294)"
```

---

## Self-Review

**Spec coverage.** §3.1 marker → Tasks 1-2. §3.2 ordering invariant → Task 2 Step 4. §3.3 reading, self-healing, orphans → Task 4. §3.4 trust and backfill → Tasks 3-4. §3.5 caching and `handle()` sharing → Task 3. §4 costs → documented in Task 5. §5 testing: (1) Task 1, (2) Task 2, (3) Task 4 stale/orphan cases, (4) Task 4 upgrade case, (5) conformance re-run in Tasks 2 and 4, (6) Task 5. §6 rejected alternatives → ADR only, no task needed.

**Placeholders.** None: every step carries the code or the exact command.

**Type consistency.** `marker_key`/`parse_marker_key`/`marked_flag_key` (Task 1) are used under those names in Tasks 2-4. `Listing { records, markers }` (Task 4) is the only new type. `collection_marked`/`mark_collection` (Task 3) match their call sites in Task 4. `read_liveness` returns `BTreeMap<RecordKey, bool>`, which Task 4's match arms read as `None`/`Some(true)`/`Some(false)`.

**Known wrinkle, deliberately left in:** Task 4 Step 4 writes an orphan filter that Step 6 then simplifies. The two-step shape is intentional — the first version is what falls out of the match arms, and seeing it wrong once is cheaper than describing the right one abstractly. An implementer who writes Step 6's version directly should do so and skip Step 6.

---

## Deviation from this plan, found during Task 4

Tasks 2 and 4 as written had a recreation and a purge remove the marker, and had
`list` remove stale markers unconditionally. That is unsafe, and not only in a
crash: a removal is a live race against a concurrent delete, which writes its
marker first and its tombstone second. A removal landing between those two
strands an unmarked tombstone on a healthy system — exactly the state the
invariant forbids — and no test in this plan would have caught it.

The implemented design therefore differs:

- **Writers only ever add markers.** A recreation leaves the old marker stale; a
  purge leaves it orphaned. Neither costs a round trip any more.
- **Only `list` removes a marker**, with `If-Match` on the ETag it saw in its own
  listing.
- **The marker body is the tombstone's revision**, which is what makes that
  conditional meaningful: a tombstone's counter always exceeds the record it
  replaced, so no two markers for a key share an ETag, and a marker rewritten
  since the listing is left alone.

The spec and [ADR 0025](../../adr/0025-s3-tombstone-markers.md) were updated to
match; they are the authority, not this plan.
