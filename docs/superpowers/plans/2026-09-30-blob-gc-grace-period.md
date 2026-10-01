# Blob GC Grace Period Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop a GC sweep from deleting a blob that a writer is about to reference.

**Architecture:** `list_blobs` reports each blob's modified time, and the sweep deletes only unreferenced blobs at least `min_age` old. Because content-addressed dedup means a write can newly reference an *old* blob, writers also re-check the blobs they referenced after committing and re-upload anything missing.

**Tech Stack:** Rust (edition 2024), `async-trait`, `tokio`, `aws-sdk-s3`, `tonic`/`prost`, `axum`.

**Spec:** `docs/superpowers/specs/2026-09-30-blob-gc-grace-period-design.md`

## Global Constraints

- `min_age` defaults to **1 hour** (`DEFAULT_MIN_AGE`).
- A blob is deleted only when it is **both** unreferenced **and** at least `min_age` old.
- A blob dated in the **future** (GC host's clock behind the store's) counts as **too young** and is kept. Every ambiguity errs toward keeping data.
- A hash repeated in one listing resolves to its **newest** `modified`.
- `freed + retained + deferred == distinct listed blobs`.
- `gc_blobs(store)` keeps its signature and uses the safe default; `gc_blobs_with` / `sweep_blobs_with` take an explicit policy.
- gRPC `ListBlobsResponse` **reserves field 1** rather than reusing it.
- `commit()` in `crates/gonzalo-vector/src/record_index.rs` is the most heavily reviewed function in the workspace — it has had two independent reviews after losing data twice. Task 5 adds **one call** inside it and nothing else.
- The gate is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace --all-targets --all-features`, `cargo test --workspace --all-features`. CI runs `--all-features`; the `hnsw` backend compiles there.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.

## Review Focus

Five conditions the spec implies that no task's own happy-path tests would exercise. Each one's test is added to the task that owns the code.

1. **FS `list_blobs` racing a concurrent sweeper** — a blob deleted between `read_dir` and reading its metadata must be *skipped*, not fail the whole listing. Two sweepers, or `--watch --gc` beside a manual `gc`, make this ordinary. A listing that errors aborts GC entirely. → Task 1.
2. **The `min_age` boundary** — a blob *exactly* `min_age` old must be swept (`>=`), or blobs pile up one tick short of collectable forever. → Task 2.
3. **`min_age: Duration::ZERO` sweeps immediately** — every migrated test depends on this, and so does any operator who wants the old behaviour back. → Task 2.
4. **A hash repeated in one listing** resolves to the newest `modified`, so a duplicate can never make a young blob look old. → Task 2.
5. **S3 `has_blob` on a non-404 error** (403, 500, throttling) must return `Err`, not `false`. Reporting `false` makes a writer re-upload on every transient fault and silently hides a broken store. → Task 3.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/gonzalo-core/src/store.rs` | **Modify.** `BlobEntry`; `list_blobs` returns entries; defaulted `has_blob`. |
| `crates/gonzalo-core/src/gc.rs` | **Modify.** `SweepPolicy`, `DEFAULT_MIN_AGE`, age-aware sweep, `GcReport.deferred`. |
| `crates/gonzalo-core/src/lib.rs` | **Modify.** Re-export the new names. |
| `crates/gonzalo-core/src/conformance.rs` | **Modify.** Blob assertions for ages and `has_blob` (runs for every substrate). |
| `crates/gonzalo-core/src/ancestry.rs` | **Modify.** Test fake `Mem`. |
| `crates/gonzalo-store-fs/src/lib.rs` | **Modify.** mtime per blob; `has_blob` via `try_exists`. |
| `crates/gonzalo-store-s3/src/lib.rs` | **Modify.** `LastModified` per blob; `has_blob` via `HeadObject`. |
| `crates/gonzalo-store-server/src/lib.rs` | **Modify.** Client side of the new wire shape. |
| `crates/gonzalo-server/src/{service,http,grpc}.rs` | **Modify.** Daemon side of the new wire shape. |
| `crates/gonzalo-proto/proto/*.proto` | **Modify.** `BlobEntry`; reserve field 1. |
| `docs/api/openapi.json` | **Modify.** `GET /v1/blobs` response schema. |
| `crates/gonzalo-vector/src/record_index.rs` | **Modify.** Post-commit re-check; two test fakes. |
| `crates/gonzalo-cli/src/lib.rs` | **Modify.** `GcSummary.deferred`; graph-indexer re-check helper. |
| `crates/gonzalo-cli/src/main.rs` | **Modify.** `gonzalo gc --min-age`. |
| `docs/adr/0028-blob-gc-grace-period.md` | **Create.** The decision record. |

---

### Task 1: `BlobEntry`, the trait change, and every substrate

The trait signature change fans out to all three substrates and the daemon wire in one go. **The workspace will not compile partway through this task** — that is expected for a trait change; the task's deliverable is a green workspace at the end.

**Files:**
- Modify: `crates/gonzalo-core/src/store.rs`, `crates/gonzalo-core/src/lib.rs`, `crates/gonzalo-core/src/gc.rs`, `crates/gonzalo-core/src/ancestry.rs`, `crates/gonzalo-core/src/conformance.rs`
- Modify: `crates/gonzalo-store-fs/src/lib.rs`, `crates/gonzalo-store-s3/src/lib.rs`, `crates/gonzalo-store-server/src/lib.rs`
- Modify: `crates/gonzalo-server/src/{service,http,grpc}.rs`, `crates/gonzalo-proto/proto/gonzalo.proto`, `docs/api/openapi.json`
- Modify: `crates/gonzalo-vector/src/record_index.rs` (two test fakes only)

**Interfaces:**
- Produces: `BlobEntry { hash: ContentHash, modified_unix_ms: i64 }` with `BlobEntry::from_system_time(hash, SystemTime) -> Self`, `age(&self, now: SystemTime) -> Option<Duration>`; `BlobStore::list_blobs(&self) -> Result<Vec<BlobEntry>>`; defaulted `BlobStore::has_blob(&self, &ContentHash) -> Result<bool>`.

**Note on one deviation from the spec.** The spec writes the field as `modified: SystemTime`. Store it as `modified_unix_ms: i64` instead, with `from_system_time` and `age` accessors. Reason: this type crosses the HTTP wire, `SystemTime` has no useful serde representation, and `i64` milliseconds is already the gRPC field type — so one type serves core, the client and the daemon instead of three near-identical DTOs. Age arithmetic also stays in integers, where "dated in the future" is just a negative difference rather than a `SystemTimeError`.

- [ ] **Step 1: Write the failing test for `BlobEntry`**

Add to `mod tests` in `crates/gonzalo-core/src/store.rs` (create the module at the end of the file if it has none — check first):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    #[test]
    fn age_is_the_gap_between_modified_and_now() {
        let e = BlobEntry::from_system_time(ContentHash("h".into()), at(1_000));
        assert_eq!(e.age(at(4_000)), Some(Duration::from_millis(3_000)));
    }

    #[test]
    fn age_of_a_blob_dated_in_the_future_is_none() {
        // The GC host's clock behind the store's. `None` means "too young", so
        // the sweep keeps the blob — ambiguity errs toward keeping data.
        let e = BlobEntry::from_system_time(ContentHash("h".into()), at(9_000));
        assert_eq!(e.age(at(1_000)), None);
    }

    #[test]
    fn age_at_exactly_now_is_zero_not_none() {
        let e = BlobEntry::from_system_time(ContentHash("h".into()), at(5_000));
        assert_eq!(e.age(at(5_000)), Some(Duration::ZERO));
    }

    #[test]
    fn a_pre_epoch_timestamp_round_trips_as_a_negative_and_still_ages() {
        let before_epoch = UNIX_EPOCH - Duration::from_millis(500);
        let e = BlobEntry::from_system_time(ContentHash("h".into()), before_epoch);
        assert!(e.modified_unix_ms < 0);
        assert_eq!(e.age(at(500)), Some(Duration::from_millis(1_000)));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core store::tests`
Expected: FAIL to compile — `BlobEntry` does not exist.

- [ ] **Step 3: Add `BlobEntry` and change the trait**

In `crates/gonzalo-core/src/store.rs`, add above the `BlobStore` trait:

```rust
/// One stored blob and when it was last written.
///
/// The timestamp is Unix milliseconds rather than a [`SystemTime`] because this
/// type crosses the daemon's HTTP and gRPC surfaces, where milliseconds are the
/// wire form; keeping one type avoids a near-identical DTO in the client and the
/// server. Use [`from_system_time`](Self::from_system_time) and
/// [`age`](Self::age) rather than reading the field arithmetically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobEntry {
    pub hash: ContentHash,
    pub modified_unix_ms: i64,
}

impl BlobEntry {
    pub fn from_system_time(hash: ContentHash, modified: SystemTime) -> Self {
        let modified_unix_ms = match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            // Before the epoch: a bogus or wildly-wrong mtime. Represent it
            // faithfully as a negative rather than clamping, so it reads as very
            // old (collectable) instead of accidentally very new.
            Err(e) => -(e.duration().as_millis() as i64),
        };
        Self {
            hash,
            modified_unix_ms,
        }
    }

    /// How old this blob is at `now`, or `None` when it is dated in the future.
    ///
    /// Callers treat `None` as "too young to sweep": a blob dated ahead of the
    /// GC host's clock must not be deleted on the strength of a clock
    /// disagreement.
    pub fn age(&self, now: SystemTime) -> Option<Duration> {
        let now_ms = match now.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            Err(e) => -(e.duration().as_millis() as i64),
        };
        let age_ms = now_ms.checked_sub(self.modified_unix_ms)?;
        if age_ms < 0 {
            return None;
        }
        Some(Duration::from_millis(age_ms as u64))
    }
}
```

Add the imports `std::time::{Duration, SystemTime}` and `serde::{Deserialize, Serialize}` at the top of the file if absent.

Then change the trait's `list_blobs` and add `has_blob`:

```rust
    /// Every stored blob with the time it was last written. Order is
    /// unspecified, and a hash may repeat — a caller that cares resolves a
    /// duplicate to its newest timestamp. Used by GC to enumerate candidates
    /// and decide which are old enough to sweep (ADR 0024, 0028).
    async fn list_blobs(&self) -> Result<Vec<BlobEntry>>;

    /// Whether `hash` is stored.
    ///
    /// Defaulted so this is not a breaking addition. The default fetches the
    /// blob and throws the bytes away; substrates override it with a cheap
    /// existence check. A writer uses this after committing to confirm the
    /// blobs it referenced are still present (ADR 0028).
    async fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        Ok(self.get_blob(hash).await?.is_some())
    }
```

In `crates/gonzalo-core/src/lib.rs`, add `BlobEntry` to the `pub use store::{…}` list.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core store::tests`
Expected: PASS, 4 tests. The rest of the workspace does not compile yet.

- [ ] **Step 5: Adapt `gc.rs` minimally so core compiles**

In `crates/gonzalo-core/src/gc.rs`, `sweep_blobs` now gets entries. Change only what compiles — age filtering is Task 2:

```rust
    let entries = blobs.list_blobs().await?;
    let all: Vec<ContentHash> = entries.into_iter().map(|e| e.hash).collect();
```

In that file's `mod tests`, update the `FakeBlobs` fake:

```rust
        async fn list_blobs(&self) -> Result<Vec<BlobEntry>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .map(|h| BlobEntry::from_system_time(h, SystemTime::now()))
                .collect())
        }
```

Apply the same shape to `Mem` in `crates/gonzalo-core/src/ancestry.rs`.

Run: `cargo test -p gonzalo-core` — expected PASS.

- [ ] **Step 6: FS substrate — report mtime, and survive a vanishing blob**

This is Review Focus item 1. Add to `crates/gonzalo-store-fs/tests/blob_gc.rs` (or the crate's own test module — check which already covers `list_blobs`):

```rust
#[tokio::test]
async fn list_blobs_reports_a_modified_time_for_each_blob() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let before = std::time::SystemTime::now();
    let h = store.put_blob(b"one").await.unwrap();
    let after = std::time::SystemTime::now();

    let listed = store.list_blobs().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].hash, h);
    let m = std::time::UNIX_EPOCH
        + std::time::Duration::from_millis(listed[0].modified_unix_ms as u64);
    // Filesystem mtime granularity can be coarse, so allow a second of slack
    // on each side rather than asserting a strict interval.
    assert!(m + std::time::Duration::from_secs(1) >= before);
    assert!(m <= after + std::time::Duration::from_secs(1));
}

// A concurrent sweeper (or `--watch --gc` beside a manual `gc`) can delete a
// blob between `read_dir` and the metadata call. Skipping it keeps the listing
// usable; erroring would abort the whole GC run.
#[tokio::test]
async fn list_blobs_skips_a_blob_that_vanishes_mid_listing() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let keep = store.put_blob(b"keep").await.unwrap();
    let gone = store.put_blob(b"gone").await.unwrap();

    // Simulate the race deterministically: the entry is in the directory when
    // `read_dir` runs, and absent when metadata is read.
    store.delete_blob(&gone).await.unwrap();

    let listed = store.list_blobs().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].hash, keep);
}
```

Then in `crates/gonzalo-store-fs/src/lib.rs`, inside `list_blobs`'s loop, replace `out.push(ContentHash(name));` with:

```rust
            if is_blob_hash(&name) {
                // A concurrent sweeper can unlink the blob between `read_dir`
                // and this metadata call. Skip a vanished entry rather than
                // failing the listing, which would abort the GC run that is
                // probably what deleted it.
                let modified = match entry.metadata().await {
                    Ok(md) => md
                        .modified()
                        .map_err(|e| CoreError::Backend(e.to_string()))?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(CoreError::Backend(e.to_string())),
                };
                out.push(BlobEntry::from_system_time(ContentHash(name), modified));
            }
```

Change the signature to `Result<Vec<BlobEntry>>` and import `BlobEntry`.

Run: `cargo test -p gonzalo-store-fs` — expected PASS.

- [ ] **Step 7: S3 substrate — report `LastModified`**

In `crates/gonzalo-store-s3/src/lib.rs`, in `list_blobs`'s `for obj in resp.contents()` loop:

```rust
                if let Some(k) = obj.key()
                    && let Some(hash) = blob_hash_from_key(k)
                {
                    // ListObjectsV2 always carries LastModified; a response
                    // without one is a malformed listing, not a blob we can
                    // reason about the age of.
                    let dt = obj.last_modified().ok_or_else(|| {
                        CoreError::Backend(format!("blob {k} listed without a LastModified"))
                    })?;
                    let modified = SystemTime::try_from(*dt).map_err(|e| {
                        CoreError::Backend(format!("blob {k} has an unrepresentable LastModified: {e}"))
                    })?;
                    out.push(BlobEntry::from_system_time(hash, modified));
                }
```

Change the signature to `Result<Vec<BlobEntry>>`; import `BlobEntry` and `std::time::SystemTime`.

- [ ] **Step 8: Daemon wire — proto, server, client**

`crates/gonzalo-proto/proto/gonzalo.proto`:

```proto
message BlobEntry {
  // Hex ContentHash of the blob.
  string hash = 1;
  // When the blob was last written, Unix milliseconds.
  int64 modified_unix_ms = 2;
}
message ListBlobsResponse {
  // Field 1 was `repeated string hashes`. Reserved, never reused: giving field
  // 1 a new type would make an old client misparse silently, where an empty
  // `entries` makes an old GC client delete nothing — it fails safe.
  reserved 1;
  repeated BlobEntry entries = 2;
}
```

`crates/gonzalo-server/src/service.rs` — change the return type to `Result<Vec<BlobEntry>>` (it just forwards).

`crates/gonzalo-server/src/grpc.rs` in `list_blobs`:

```rust
        let entries = self.service.list_blobs().await.map_err(internal)?;
        Ok(Response::new(ListBlobsResponse {
            entries: entries
                .into_iter()
                .map(|e| crate::pb::BlobEntry {
                    hash: e.hash.0,
                    modified_unix_ms: e.modified_unix_ms,
                })
                .collect(),
        }))
```

Use whatever path the generated types already use in that file for `ListBlobsResponse` — match it rather than inventing `crate::pb`.

`crates/gonzalo-server/src/http.rs` in `list_blobs`: the handler already does `Json(hashes)`; it now serialises `Vec<BlobEntry>`, which derives `Serialize`, so only the binding name needs updating. Verify the emitted JSON is `[{"hash":…,"modified_unix_ms":…}]`.

`crates/gonzalo-store-server/src/lib.rs` in `list_blobs`:

```rust
                Ok(resp.json::<Vec<BlobEntry>>().await.map_err(be)?)
```

and for gRPC:

```rust
                Ok(resp
                    .entries
                    .into_iter()
                    .map(|e| BlobEntry {
                        hash: ContentHash(e.hash),
                        modified_unix_ms: e.modified_unix_ms,
                    })
                    .collect())
```

- [ ] **Step 9: OpenAPI — the response schema**

In `docs/api/openapi.json`, add a `BlobEntry` schema under `components.schemas`:

```json
"BlobEntry": {
  "type": "object",
  "required": ["hash", "modified_unix_ms"],
  "properties": {
    "hash": { "$ref": "#/components/schemas/ContentHash" },
    "modified_unix_ms": {
      "type": "integer",
      "format": "int64",
      "description": "When the blob was last written, Unix milliseconds."
    }
  }
}
```

and point `GET /v1/blobs`'s 200 response at it, replacing the `ContentHash` items ref:

```json
"schema": { "type": "array", "items": { "$ref": "#/components/schemas/BlobEntry" } }
```

Also update that operation's `summary`/`description` wording from "hashes" to "entries".

**Note:** #198's drift test compares paths and methods only, so nothing will fail if this edit is wrong. Check the emitted JSON against the schema by hand — run the daemon's HTTP test for `list_blobs` and read the body.

- [ ] **Step 10: Pin the wire shape with tests**

Two existing daemon tests assert the old shape and must now assert the new one, including that the timestamp is real rather than defaulted:

- `crates/gonzalo-server/src/service.rs` has a test asserting `svc.list_blobs().await.unwrap() == vec![hash.clone()]`. Change it to compare `.hash` and add `assert_ne!(listed[0].modified_unix_ms, 0, "a zero timestamp means the substrate never reported one");`
- `crates/gonzalo-server/src/grpc.rs` has a test calling `list_blobs(Request::new(ListBlobsRequest {}))`. Change it to read `resp.entries`, assert the hash matches, and assert `modified_unix_ms != 0`.

`crates/gonzalo-server/src/http.rs` already has a route test that asserts the old shape:

```rust
        let hashes: Vec<gonzalo_core::ContentHash> = serde_json::from_slice(&body).unwrap();
        assert_eq!(hashes, vec![gonzalo_core::ContentHash::of(&content)]);
```

Replace those two lines with an assertion on the new shape, read as untyped JSON so the test pins the wire form rather than whatever `BlobEntry`'s derive happens to emit:

```rust
        let entries: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let first = &entries.as_array().expect("an array of blob entries")[0];
        assert_eq!(
            first.get("hash").and_then(|h| h.as_str()),
            Some(gonzalo_core::ContentHash::of(&content).0.as_str())
        );
        let ms = first
            .get("modified_unix_ms")
            .and_then(|m| m.as_i64())
            .expect("modified_unix_ms is present and an integer");
        assert_ne!(ms, 0, "a zero timestamp means the substrate never reported one");
```

Reading it as `serde_json::Value` is the point: this is what actually guards the shape recorded in `docs/api/openapi.json`, because #198's drift check compares paths and methods only. Deserialising into `Vec<BlobEntry>` would pass regardless of the field names the schema promises.

- [ ] **Step 11: Vector test fakes**

In `crates/gonzalo-vector/src/record_index.rs`, `CountingStore` and `FailingBlobStore` both forward `list_blobs` to an inner store. Change their signatures to `Result<Vec<BlobEntry>>`; the bodies keep forwarding unchanged.

- [ ] **Step 12: Update the conformance suite to the new shape**

In `crates/gonzalo-core/src/conformance.rs`, `blob_list_reports_stored_hashes` compares `Vec<ContentHash>`. Keep what it asserts; adapt the shape:

```rust
    let mut listed: Vec<ContentHash> = store
        .list_blobs()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.hash)
        .collect();
```

Age and `has_blob` assertions come in Task 3.

- [ ] **Step 13: Run the full gate**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features
```

Expected: all green. Fix and re-run the whole gate on any failure.

- [ ] **Step 14: Commit**

```bash
git add crates docs/api/openapi.json
git commit -m "$(printf 'feat(core)!: list_blobs reports each blob modified time (#325)\n\nBREAKING: BlobStore::list_blobs returns Vec<BlobEntry>, and the daemon\n GET /v1/blobs response and gRPC ListBlobsResponse change shape. gRPC field 1\nis reserved rather than reused so an old client fails safe.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 2: GC sweeps by age

**Files:**
- Modify: `crates/gonzalo-core/src/gc.rs`, `crates/gonzalo-core/src/lib.rs`

**Interfaces:**
- Consumes: `BlobEntry::age`, `BlobStore::list_blobs` (Task 1).
- Produces: `SweepPolicy { min_age: Duration, now: SystemTime }`, `DEFAULT_MIN_AGE: Duration`, `sweep_blobs_with(blobs, live, policy)`, `gc_blobs_with(store, policy)`, `GcReport { freed, retained, deferred }`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/gonzalo-core/src/gc.rs`:

```rust
    use std::time::{Duration, UNIX_EPOCH};

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A store whose blobs carry the timestamps a test chooses.
    struct AgedBlobs(std::sync::Mutex<Vec<BlobEntry>>);

    #[async_trait]
    impl BlobStore for AgedBlobs {
        async fn put_blob(&self, _content: &[u8]) -> Result<ContentHash> {
            unreachable!("tests seed blobs directly")
        }
        async fn get_blob(&self, _hash: &ContentHash) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn list_blobs(&self) -> Result<Vec<BlobEntry>> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
            self.0.lock().unwrap().retain(|e| &e.hash != hash);
            Ok(())
        }
    }

    fn aged(entries: Vec<(ContentHash, SystemTime)>) -> AgedBlobs {
        AgedBlobs(std::sync::Mutex::new(
            entries
                .into_iter()
                .map(|(h, t)| BlobEntry::from_system_time(h, t))
                .collect(),
        ))
    }

    fn policy(now: u64, min_age_secs: u64) -> SweepPolicy {
        SweepPolicy {
            min_age: Duration::from_secs(min_age_secs),
            now: at(now),
        }
    }

    // The bug this ticket exists for: a blob uploaded moments ago, whose
    // manifest has not committed yet, must survive the sweep.
    #[tokio::test]
    async fn a_young_unreferenced_blob_is_deferred_not_freed() {
        let blobs = aged(vec![(h("fresh"), at(3_600))]);
        let report = sweep_blobs_with(&blobs, &BTreeSet::new(), policy(3_630, 3_600))
            .await
            .unwrap();
        assert!(report.freed.is_empty());
        assert_eq!(report.deferred, 1);
        assert_eq!(report.retained, 0);
    }

    #[tokio::test]
    async fn an_old_unreferenced_blob_is_freed() {
        let blobs = aged(vec![(h("stale"), at(0))]);
        let report = sweep_blobs_with(&blobs, &BTreeSet::new(), policy(7_200, 3_600))
            .await
            .unwrap();
        assert_eq!(report.freed, vec![h("stale")]);
        assert_eq!(report.deferred, 0);
    }

    // Review Focus 2. `>=`, not `>` — otherwise a blob sits one tick short of
    // collectable forever.
    #[tokio::test]
    async fn a_blob_exactly_min_age_old_is_freed() {
        let blobs = aged(vec![(h("edge"), at(0))]);
        let report = sweep_blobs_with(&blobs, &BTreeSet::new(), policy(3_600, 3_600))
            .await
            .unwrap();
        assert_eq!(report.freed, vec![h("edge")]);
    }

    // Review Focus 3. The migration path for every pre-existing test, and the
    // way an operator asks for the old behaviour.
    #[tokio::test]
    async fn a_zero_min_age_sweeps_immediately() {
        let blobs = aged(vec![(h("now"), at(1_000))]);
        let report = sweep_blobs_with(
            &blobs,
            &BTreeSet::new(),
            SweepPolicy {
                min_age: Duration::ZERO,
                now: at(1_000),
            },
        )
        .await
        .unwrap();
        assert_eq!(report.freed, vec![h("now")]);
    }

    #[tokio::test]
    async fn a_referenced_blob_is_retained_however_old() {
        let blobs = aged(vec![(h("live"), at(0))]);
        let live = BTreeSet::from([h("live")]);
        let report = sweep_blobs_with(&blobs, &live, policy(100_000, 3_600))
            .await
            .unwrap();
        assert!(report.freed.is_empty());
        assert_eq!(report.retained, 1);
        assert_eq!(report.deferred, 0);
    }

    // The GC host's clock behind the store's. Deleting on the strength of a
    // clock disagreement is the one outcome that loses data.
    #[tokio::test]
    async fn a_future_dated_blob_is_deferred_not_freed() {
        let blobs = aged(vec![(h("ahead"), at(9_000))]);
        let report = sweep_blobs_with(&blobs, &BTreeSet::new(), policy(1_000, 0))
            .await
            .unwrap();
        assert!(report.freed.is_empty());
        assert_eq!(report.deferred, 1);
    }

    // Review Focus 4. Listings promise neither order nor uniqueness; a
    // duplicate must not let a young blob read as old.
    #[tokio::test]
    async fn a_repeated_hash_resolves_to_its_newest_timestamp() {
        let blobs = aged(vec![(h("dup"), at(0)), (h("dup"), at(3_600))]);
        let report = sweep_blobs_with(&blobs, &BTreeSet::new(), policy(3_630, 3_600))
            .await
            .unwrap();
        assert!(report.freed.is_empty(), "the newest timestamp is young");
        assert_eq!(report.deferred, 1);
    }

    #[tokio::test]
    async fn the_three_outcomes_partition_the_distinct_blobs() {
        let blobs = aged(vec![
            (h("live"), at(0)),
            (h("old"), at(0)),
            (h("young"), at(3_600)),
        ]);
        let live = BTreeSet::from([h("live")]);
        let report = sweep_blobs_with(&blobs, &live, policy(3_630, 3_600))
            .await
            .unwrap();
        assert_eq!(report.freed.len() + report.retained + report.deferred, 3);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core gc::tests`
Expected: FAIL to compile — `SweepPolicy` and `sweep_blobs_with` do not exist.

- [ ] **Step 3: Implement the policy and the age-aware sweep**

In `crates/gonzalo-core/src/gc.rs`:

```rust
/// How long a blob must have gone untouched before a sweep may delete it.
///
/// A writer uploads its blobs *before* committing the manifest that names them,
/// so a sweep that runs in between sees them as unreferenced. One hour exceeds
/// the longest upload phase in the workspace (a full graph walk, or a bulk
/// vector load of 256 shards over S3 — both minutes) and absorbs clock skew
/// between the GC host and the store. See ADR 0028.
pub const DEFAULT_MIN_AGE: Duration = Duration::from_secs(3600);

/// When a sweep runs and how old a blob must be to qualify.
///
/// `now` is a parameter rather than read from the clock so the grace period is
/// testable without backdating files across three substrates.
#[derive(Clone, Copy, Debug)]
pub struct SweepPolicy {
    pub min_age: Duration,
    pub now: SystemTime,
}

impl Default for SweepPolicy {
    fn default() -> Self {
        Self {
            min_age: DEFAULT_MIN_AGE,
            now: SystemTime::now(),
        }
    }
}

/// As [`sweep_blobs`], with an explicit policy.
pub async fn sweep_blobs_with<B>(
    blobs: &B,
    live: &BTreeSet<ContentHash>,
    policy: SweepPolicy,
) -> Result<GcReport>
where
    B: BlobStore + ?Sized,
{
    // Listings promise neither order nor uniqueness, so collapse duplicates to
    // the NEWEST timestamp: a stale duplicate must never make a young blob look
    // collectable.
    let mut newest: BTreeMap<ContentHash, i64> = BTreeMap::new();
    for entry in blobs.list_blobs().await? {
        newest
            .entry(entry.hash)
            .and_modify(|ms| *ms = (*ms).max(entry.modified_unix_ms))
            .or_insert(entry.modified_unix_ms);
    }

    let mut freed = Vec::new();
    let mut deferred = 0usize;
    let mut retained = 0usize;
    for (hash, modified_unix_ms) in &newest {
        if live.contains(hash) {
            retained += 1;
            continue;
        }
        let entry = BlobEntry {
            hash: hash.clone(),
            modified_unix_ms: *modified_unix_ms,
        };
        // `None` means dated in the future — treated as too young, because
        // deleting on the strength of a clock disagreement is the one outcome
        // that loses data.
        match entry.age(policy.now) {
            Some(age) if age >= policy.min_age => freed.push(hash.clone()),
            _ => deferred += 1,
        }
    }

    for hash in &freed {
        blobs.delete_blob(hash).await?;
    }
    Ok(GcReport {
        freed,
        retained,
        deferred,
    })
}
```

Rewrite `sweep_blobs` as the defaulted wrapper, and do the same for `gc_blobs`:

```rust
pub async fn sweep_blobs<B>(blobs: &B, live: &BTreeSet<ContentHash>) -> Result<GcReport>
where
    B: BlobStore + ?Sized,
{
    sweep_blobs_with(blobs, live, SweepPolicy::default()).await
}
```

Add `deferred: usize` to `GcReport` with the doc comment:

```rust
    /// Count of blobs no record references that were kept anyway, because they
    /// are younger than the policy's `min_age`. Without this an operator cannot
    /// tell "nothing to reclaim" from "reclaiming held back".
    pub deferred: usize,
```

Factor `gc_blobs`'s record-marking half into `gc_blobs_with(store, policy)` and leave `gc_blobs(store)` calling it with `SweepPolicy::default()`. Import `BTreeMap`, `Duration`, `SystemTime`, `BlobEntry`. Export `SweepPolicy`, `DEFAULT_MIN_AGE`, `sweep_blobs_with`, `gc_blobs_with` from `crates/gonzalo-core/src/lib.rs`.

- [ ] **Step 4: Migrate the callers that expect an immediate sweep**

These call sites assert a blob is freed right after being written, which the default policy now defers. Point each at `sweep_blobs_with` / `gc_blobs_with` with `SweepPolicy { min_age: Duration::ZERO, now: SystemTime::now() }`:

- `crates/gonzalo-store-fs/tests/blob_gc.rs` (4 `gc_blobs` calls)
- `crates/gonzalo-vector/src/record_index.rs` (the orphan-reclaim test's `gc_blobs`)
- any `sweep_blobs` call in `crates/gonzalo-core/src/gc.rs`'s own older tests

Run `rg -n 'gc_blobs\(|sweep_blobs\(' crates/` and migrate every test hit. Leave production calls on the defaulted form.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core gc` then `cargo test --workspace --all-features`
Expected: PASS. 8 new tests in `gc::tests`.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "$(printf 'feat(gc): sweep only blobs older than min_age (#325)\n\nA writer uploads blobs before committing the manifest that names them, so a\nsweep in between saw them as unreferenced and deleted them. Unreferenced\nblobs younger than min_age are now deferred and counted.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 3: `has_blob` overrides and conformance

**Files:**
- Modify: `crates/gonzalo-store-fs/src/lib.rs`, `crates/gonzalo-store-s3/src/lib.rs`, `crates/gonzalo-core/src/conformance.rs`

**Interfaces:**
- Consumes: `BlobStore::has_blob` default (Task 1).
- Produces: cheap `has_blob` on FS and S3; conformance coverage for `has_blob` and for `modified`, which every substrate runs.

- [ ] **Step 1: Write the failing conformance assertions**

In `crates/gonzalo-core/src/conformance.rs`, add two functions and register them wherever the existing blob cases are registered (find how `blob_list_reports_stored_hashes` is invoked and follow it exactly):

```rust
async fn blob_list_reports_a_plausible_modified_time<B: BlobStore>(store: &B) {
    let before = SystemTime::now();
    let hash = store.put_blob(b"aged content").await.unwrap();
    let listed = store.list_blobs().await.unwrap();
    let entry = listed
        .iter()
        .find(|e| e.hash == hash)
        .expect("the blob just written is listed");

    // Deliberately generous: the store's clock is not this process's clock (S3
    // stamps LastModified server-side). The point is to catch a substrate that
    // returns the epoch, zero, or a client-side `now` it made up.
    let age = entry.age(before + Duration::from_secs(3600));
    assert!(
        age.is_some(),
        "modified should not be dated an hour into the future"
    );
    assert!(
        age.unwrap() <= Duration::from_secs(7200),
        "modified looks nothing like now: {:?}",
        entry.modified_unix_ms
    );
    assert_ne!(entry.modified_unix_ms, 0, "epoch means unimplemented");
}

async fn has_blob_tracks_presence<B: BlobStore>(store: &B) {
    let hash = store.put_blob(b"present").await.unwrap();
    assert!(store.has_blob(&hash).await.unwrap());

    let absent = ContentHash::of(b"never stored by this test");
    assert!(!store.has_blob(&absent).await.unwrap());
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p gonzalo-store-fs conformance`
Expected: FAIL to compile — the functions are not registered yet; once registered they should pass for FS via the default `has_blob`, and the `modified` assertion passes because Task 1 implemented it. **If both pass immediately, say so in the report rather than inventing a failure** — these are guard tests over Task 1's work, and their value is that every substrate now runs them.

- [ ] **Step 3: Override `has_blob` on FS**

In `crates/gonzalo-store-fs/src/lib.rs`'s `impl BlobStore for FsStore`:

```rust
    async fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        let path = layout::blob_path(&self.root, hash);
        tokio::fs::try_exists(&path)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))
    }
```

- [ ] **Step 4: Override `has_blob` on S3, and write Review Focus 5's test first**

Add to `crates/gonzalo-store-s3`'s test module (the unit tests that do not need a live S3 — check how the crate separates unit from `--test integration`):

```rust
// Review Focus 5. A 403, a 500 or throttling must surface as Err. Reporting
// `false` would make a writer re-upload on every transient fault and hide a
// broken store behind what looks like a missing blob.
#[test]
fn a_head_error_other_than_not_found_is_not_absence() {
    assert!(!head_means_absent("AccessDenied"));
    assert!(!head_means_absent("InternalError"));
    assert!(head_means_absent("NotFound"));
}
```

Then in `crates/gonzalo-store-s3/src/lib.rs`:

```rust
/// Whether a `HeadObject` error code means "no such blob" as opposed to a fault
/// worth surfacing. S3 answers a missing key with 404 `NotFound`; anything else
/// is a real error and must not be reported as absence.
fn head_means_absent(code: &str) -> bool {
    matches!(code, "NotFound" | "NoSuchKey")
}
```

and in `impl BlobStore for S3Store`:

```rust
    async fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        let key = format!("{BLOB_PREFIX}{}", hash.0);
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                let svc = e.into_service_error();
                if svc.is_not_found() || head_means_absent(svc.code().unwrap_or_default()) {
                    Ok(false)
                } else {
                    Err(CoreError::Backend(svc.to_string()))
                }
            }
        }
    }
```

Check the generated error type: if `HeadObjectError` has `is_not_found()`, prefer it and keep `head_means_absent` for the code-string fallback.

- [ ] **Step 5: Run the gate**

Run: `cargo test --workspace --all-features`, then clippy/build/fmt as in Task 1 Step 13.
Expected: PASS. The S3 conformance path runs under the integration suite; note in the report whether it was exercised locally or only in CI's soak job.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "$(printf 'feat(store): cheap has_blob on fs and s3, with conformance (#325)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 4: The operator surface

**Files:**
- Modify: `crates/gonzalo-cli/src/lib.rs`, `crates/gonzalo-cli/src/main.rs`

**Interfaces:**
- Consumes: `SweepPolicy`, `DEFAULT_MIN_AGE`, `gc_blobs_with`, `GcReport.deferred` (Task 2).
- Produces: `GcSummary { scanned, freed, retained, deferred }`; `gc(root, min_age: Duration)`; `gonzalo gc --min-age <duration>`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/gonzalo-cli/src/lib.rs`:

```rust
    // The flag's default is a string clap parses at startup. A typo here is a
    // runtime failure on every `gonzalo gc`, which no other test would catch.
    #[test]
    fn the_default_min_age_spelling_parses_to_an_hour() {
        assert_eq!(
            parse_duration("1h").unwrap(),
            std::time::Duration::from_secs(3600)
        );
        assert_eq!(parse_duration("1h").unwrap(), gonzalo_core::DEFAULT_MIN_AGE);
    }

    #[tokio::test]
    async fn gc_defers_a_blob_younger_than_min_age_and_counts_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());
        store.put_blob(b"brand new").await.unwrap();

        // The default horizon: nothing written moments ago is collectable.
        let held = gc(dir.path(), gonzalo_core::DEFAULT_MIN_AGE).await.unwrap();
        assert_eq!(held.freed, 0);
        assert_eq!(held.deferred, 1);

        // An operator asking for the old behaviour gets it.
        let swept = gc(dir.path(), std::time::Duration::ZERO).await.unwrap();
        assert_eq!(swept.freed, 1);
        assert_eq!(swept.deferred, 0);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p gonzalo-cli gc_defers`
Expected: FAIL to compile — `gc` takes one argument and `GcSummary` has no `deferred`.

- [ ] **Step 3: Thread `min_age` through**

In `crates/gonzalo-cli/src/lib.rs`: add `pub deferred: usize` to `GcSummary`; change `pub async fn gc(root: &Path, min_age: Duration) -> Result<GcSummary>` to build a policy and call `sweep_blobs_with`:

```rust
    let report = gonzalo_core::sweep_blobs_with(
        &store,
        &live,
        gonzalo_core::SweepPolicy {
            min_age,
            now: SystemTime::now(),
        },
    )
    .await?;
    Ok(GcSummary {
        scanned: records.len(),
        freed: report.freed.len(),
        retained: report.retained,
        deferred: report.deferred,
    })
```

Thread `min_age` through `index_with_gc`, `index_with_gc_filtered`, `index_with_gc_filtered_worker` and `crates/gonzalo-cli/src/watch.rs`. Those pass `DEFAULT_MIN_AGE` from their callers rather than inventing a value, so `--watch --gc` is covered with no new flag.

- [ ] **Step 4: Add the flag and print the count**

In `crates/gonzalo-cli/src/main.rs`, add to `Commands::Gc`:

```rust
        /// Keep unreferenced blobs younger than this, so a sweep cannot delete
        /// a blob a writer has uploaded but not yet referenced. Accepts the same
        /// spellings as `collect --horizon`, e.g. `30m`, `2h`.
        #[arg(long, default_value = "1h", value_parser = parse_duration)]
        min_age: std::time::Duration,
```

and in its handler:

```rust
        Commands::Gc { root, min_age } => {
            let summary = gc(&root, min_age).await?;
            println!("scanned:  {}", summary.scanned);
            println!("freed:    {}", summary.freed);
            println!("retained: {}", summary.retained);
            println!("deferred: {}", summary.deferred);
        }
```

Add `println!("gc.deferred: {}", swept.deferred);` beside the existing `gc.retained` line in the index handler.

- [ ] **Step 5: Run the tests and the gate**

Run: `cargo test -p gonzalo-cli` then the full gate.
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "$(printf 'feat(cli): gonzalo gc --min-age, and report deferred blobs (#325)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 5: The vector index re-checks its blobs after committing

The age filter protects *freshly uploaded* blobs. It cannot protect an **old** blob that a write newly references, which is what content-addressed dedup produces: `upsert x` then `remove x` returns a shard to its earlier content, re-referencing the blob the first write orphaned.

**Files:**
- Modify: `crates/gonzalo-vector/src/record_index.rs`

**Interfaces:**
- Consumes: `BlobStore::has_blob` (Task 1).
- Produces: `RecordVectorIndex::verify_shard_blobs(&self, written: &BTreeMap<u16, ContentHash>) -> Result<()>`.

**Constraint:** `commit()` gets **one added call** and nothing else. It has had two independent reviews on the most capable model after losing data twice. If a fix seems to need restructuring it, stop and report `NEEDS_CONTEXT`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/gonzalo-vector/src/record_index.rs`:

```rust
    /// Deletes a chosen blob the moment a record `put` lands — a deterministic
    /// stand-in for a GC sweep hitting the window between a writer's blob
    /// upload and its manifest commit.
    struct SweepsOnPut {
        inner: FsStore,
        victim: std::sync::Mutex<Option<ContentHash>>,
    }

    #[async_trait]
    impl Store for SweepsOnPut {
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            if let Some(hash) = self.victim.lock().unwrap().take() {
                self.inner.delete_blob(&hash).await?;
            }
            self.inner.put(record, expected).await
        }
        // Forward every other Store method to `self.inner`.
    }

    #[async_trait]
    impl BlobStore for SweepsOnPut {
        // Forward every BlobStore method to `self.inner`.
    }

    #[tokio::test]
    async fn a_commit_restores_a_shard_blob_swept_behind_its_back() {
        let dir = tmp();
        let key = index_key();
        let k = RecordKey::new("ns", "coll", "a");

        // First commit: one shard, one blob.
        let idx = RecordVectorIndex::open(fs(&dir), key.clone(), "space-a", 3)
            .await
            .unwrap();
        idx.upsert(k.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        let manifest = fs(&dir).get(&key).await.unwrap().unwrap();
        let shard_blob = VectorManifest::from_body(&manifest.body)
            .unwrap()
            .entries
            .values()
            .next()
            .unwrap()
            .clone();
        drop(idx);

        // Second commit, with that blob deleted as the manifest lands.
        let store = SweepsOnPut {
            inner: fs(&dir),
            victim: std::sync::Mutex::new(Some(shard_blob.clone())),
        };
        let idx = RecordVectorIndex::open(store, key.clone(), "space-a", 3)
            .await
            .unwrap();
        idx.upsert(RecordKey::new("ns", "coll", "b"), vec![0.0, 1.0, 0.0])
            .await
            .unwrap();
        drop(idx);

        // The re-check must have put the bytes back, so the index still opens.
        let reopened = RecordVectorIndex::open(fs(&dir), key, "space-a", 3)
            .await
            .unwrap();
        let mut keys = reopened.keys(&KeyPrefix::default()).await.unwrap();
        keys.sort();
        assert_eq!(keys, vec![k, RecordKey::new("ns", "coll", "b")]);
    }
```

Write out the forwarding impls in full — the file's existing `CountingStore` shows the pattern for `Store` and `BlobStore` forwarding; copy its shape rather than abbreviating.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p gonzalo-vector a_commit_restores_a_shard_blob`
Expected: FAIL — reopening errors with "shard N names blob … but it is absent", because nothing restores the swept blob.

- [ ] **Step 3: Implement the re-check**

Add to `impl<S: Store + BlobStore + Send + Sync + 'static> RecordVectorIndex<S>`:

```rust
    /// Confirm the shard blobs this commit referenced still exist, re-uploading
    /// any that do not.
    ///
    /// A sweep can delete a blob between the upload and the manifest commit that
    /// names it. GC's age filter covers a freshly uploaded blob, but not an
    /// **old** blob a commit newly references — which content-addressed dedup
    /// produces whenever a shard returns to earlier content (upsert `x`, then
    /// remove `x`). See ADR 0028.
    ///
    /// Called after the commit lands, so the in-memory shard is the committed
    /// content and the bytes can be re-encoded on demand. Re-staging is not used
    /// and no staged bytes are held: retaining them for a 256-shard batch would
    /// add roughly 150 MB of peak memory for a case that almost never fires.
    async fn verify_shard_blobs(&self, written: &BTreeMap<u16, ContentHash>) -> Result<()> {
        for (id, hash) in written {
            if self.store.has_blob(hash).await? {
                continue;
            }
            let entries = self
                .inner
                .collect_where(|k| shard_of(k, self.shards) == *id);
            let bytes = encode_shard(self.dim, &entries);
            let restored = self.store.put_blob(&bytes).await?;
            if &restored != hash {
                return Err(CoreError::Backend(format!(
                    "vector index {}: shard {id} was swept and the re-encoded bytes \
                     hash to {} rather than the committed {}",
                    self.key, restored.0, hash.0
                )));
            }
        }
        Ok(())
    }
```

In `commit()`'s `PutResult::Committed(rev)` arm, after `last_seen` is set and immediately before `return Ok(())`, add exactly one line:

```rust
                    self.verify_shard_blobs(&blobs).await?;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p gonzalo-vector --all-features`
Expected: PASS, including every pre-existing `record_index` test.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-vector/src/record_index.rs
git commit -m "$(printf 'fix(vector): re-check shard blobs after a commit lands (#325)\n\nContent-addressed dedup lets a commit newly reference an OLD blob, which\nGC age filtering cannot protect. The writer now confirms the blobs it\nreferenced still exist and re-uploads any a sweep took.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 6: The graph indexer re-checks its slices

**Files:**
- Modify: `crates/gonzalo-cli/src/lib.rs`

**Interfaces:**
- Consumes: `BlobStore::has_blob` (Task 1).
- Produces: `ensure_slices_present(store: &FsStore, wanted: &[(String, ContentHash)], staging: &GraphStaging) -> anyhow::Result<usize>`, returning how many blobs it restored.

**Why the helper is tested directly:** `index` builds its own `FsStore` from a path, so no wrapper can be injected without a larger refactor. The helper takes a store, so a test can delete a blob behind its back deterministically.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/gonzalo-cli/src/lib.rs`:

```rust
    #[tokio::test]
    async fn ensure_slices_present_restores_a_blob_that_was_swept() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());

        // Parse one real file so staging holds a Slice whose bytes we can restore.
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn alpha() {}\n").unwrap();

        let mut staging = GraphStaging::default();
        let desired = build_desired_full(
            &store,
            &mut staging,
            None,
            &src,
            &IndexFilter::default(),
        )
        .await
        .unwrap();

        let wanted: Vec<(String, ContentHash)> = desired
            .entries
            .iter()
            .map(|(p, h)| (p.clone(), h.clone()))
            .collect();
        assert!(!wanted.is_empty(), "the parsed file produced a slice");

        // A sweep takes it between the upload and the manifest commit.
        store.delete_blob(&wanted[0].1).await.unwrap();
        assert!(!store.has_blob(&wanted[0].1).await.unwrap());

        let restored = ensure_slices_present(&store, &wanted, &staging).await.unwrap();
        assert_eq!(restored, 1);
        assert!(store.has_blob(&wanted[0].1).await.unwrap());
    }
```

Check `build_desired_full`'s exact signature before writing this — the `pool` argument's type and whether it is `Option`. Match it.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p gonzalo-cli ensure_slices_present`
Expected: FAIL to compile — `ensure_slices_present` does not exist.

- [ ] **Step 3: Implement the helper and wire it in**

In `crates/gonzalo-cli/src/lib.rs`:

```rust
/// Confirm the slice blobs a just-committed manifest references still exist,
/// re-uploading any a sweep took.
///
/// A sweep can delete a blob between the upload and the manifest commit naming
/// it. GC's age filter covers a freshly uploaded slice, but not an **old** blob
/// this run newly references — content-addressed dedup makes that ordinary when
/// a file reverts to earlier content. Returns how many blobs it restored. See
/// ADR 0028.
async fn ensure_slices_present(
    store: &FsStore,
    wanted: &[(String, ContentHash)],
    staging: &GraphStaging,
) -> anyhow::Result<usize> {
    let mut restored = 0;
    for (path, hash) in wanted {
        if store.has_blob(hash).await? {
            continue;
        }
        let Some((_, slice)) = staging.inserts.iter().find(|(p, _)| p == path) else {
            anyhow::bail!(
                "slice blob {} for {path} is missing and this run did not parse it, \
                 so it cannot be restored; re-index the view",
                hash.0
            );
        };
        let put = store.put_blob(&slice.to_slice_bytes()).await?;
        anyhow::ensure!(
            &put == hash,
            "restored slice for {path} hashes to {} rather than the committed {}",
            put.0,
            hash.0
        );
        restored += 1;
    }
    Ok(restored)
}
```

In `index`, immediately after the manifest `put` commits (the `PutResult::Committed(_) => {}` arm) and **before** `staging.apply(&mut graph)`, check only the newly referenced paths:

```rust
    // Only added/modified paths need checking: an unchanged path was referenced
    // by both the old manifest and the new one, so no sweep ever saw it as
    // garbage.
    let newly_referenced: Vec<(String, ContentHash)> = recon
        .added
        .iter()
        .chain(recon.modified.iter())
        .filter_map(|p| recon.manifest.get(p).map(|h| (p.clone(), h.clone())))
        .collect();
    let restored = ensure_slices_present(&store, &newly_referenced, &staging).await?;
    if restored > 0 {
        eprintln!("restored {restored} slice blob(s) swept during this index run");
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-cli` then the full gate.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-cli/src/lib.rs
git commit -m "$(printf 'fix(cli): re-check slice blobs after the manifest commit (#325)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 7: ADR 0028 and the docs

**Files:**
- Create: `docs/adr/0028-blob-gc-grace-period.md`
- Modify: `docs/adr/README.md`, `docs/adr/0024-blob-garbage-collection.md`, `docs/adr/0027-durable-vector-index.md`
- Modify: `docs/guide/src/deletion.md`, `docs/guide/src/storage.md`, `CHANGELOG.md`

- [ ] **Step 1: Write ADR 0028**

Read `docs/adr/0027-durable-vector-index.md` first and match its header lines and section order exactly. Status `accepted`, date `2026-09-30`. The body must stand alone — no "see the spec". Cover:

- **Context:** writers upload blobs before committing the manifest naming them, so a sweep in between deletes them. Recoverable for a graph slice, unrecoverable for a vector shard. `gonzalo index --gc` under `--watch` makes it reachable with no operator involved, so ADR 0024's "explicit and operator-run" describes the one-shot CLI, not the watch trigger. Content-addressed dedup adds a second path: `put_blob` is a no-op on existing content and refreshes no timestamp, so a write that re-references an old blob gets no protection from age.
- **Decision:** sweep by blob age with `min_age` defaulting to one hour; `list_blobs` reports each blob's modified time (breaking, with the daemon wire); writers re-check newly referenced blobs after committing; future-dated blobs count as too young; a duplicate hash resolves to its newest timestamp.
- **Why one hour:** it must exceed the longest upload phase (minutes) and absorb clock skew, and `min_age` *is* the skew tolerance — a GC host running ahead of the store is the direction that loses data. Git uses two weeks for the same mechanism but has no writer re-check.
- **Rejected:** sleeping through the interval (every `gc` blocks for minutes, `--watch` stalls); two strikes across runs (a first `gc` frees nothing, and GC gains persisted state); durable leases (the only option that survives a writer crash, but a new record kind and its own ADR); freshening on re-put (still races — GC can list a blob as old, the writer freshens, GC deletes on a decision already made, and closing that needs a conditional delete FS cannot do atomically).
- **Consequences, positive:** the race is closed for live writers on every substrate; `--watch --gc` is safe with no flag; `deferred` tells an operator why space was not reclaimed.
- **Consequences, negative:** a writer that **crashes** between its commit and its re-check can still lose a newly referenced old blob; clock skew beyond `min_age` in the losing direction defeats the age filter; `ServerStore::has_blob` downloads the blob; unreferenced blobs now linger for at least `min_age`; and #198's drift check compares paths and methods, so the changed `GET /v1/blobs` response schema in `docs/api/openapi.json` is unguarded by a test.
- **Revisit if:** an upload phase can exceed an hour; the crash window is observed in practice, which would justify leases; GC becomes something a daemon runs continuously.

- [ ] **Step 2: Annotate both amendments on both sides**

ADR 0028 **amends** 0024 (blob GC gains an age rule) and 0027 (whose "do not run `gonzalo gc` while vector writes are in flight" consequence is now addressed for live writers). Neither is superseded; both stay `accepted`.

- In `docs/adr/README.md`, add the 0028 row, and extend 0024's and 0027's status cells with back-references in the style already used for partial amendments — 0012's row (`accepted (blob GC marking amended by [0024](…))`) is the model.
- In `docs/adr/0024-blob-garbage-collection.md`, add a one-line note that 0028 adds the age rule. Change nothing else.
- In `docs/adr/0027-durable-vector-index.md`, rewrite the GC-window negative consequence: the window is closed for live writers by 0028, and what remains is the crash case. It must stop telling operators to avoid GC.

- [ ] **Step 3: Update the guide**

- `docs/guide/src/deletion.md` — in the GC section, the age rule, `--min-age`, and the `deferred` count.
- `docs/guide/src/storage.md` — replace the "do not run `gonzalo gc` while vector writes are in flight" warning with the residual crash window.

Read the neighbouring prose in each file and match it.

- [ ] **Step 4: Update the changelog**

Under the unreleased heading, in the existing style:

- **BREAKING:** `BlobStore::list_blobs` returns `Vec<BlobEntry>`; `GET /v1/blobs` returns objects; gRPC `ListBlobsResponse` reserves field 1 for `entries`.
- Added: `BlobStore::has_blob` (defaulted); `SweepPolicy`, `DEFAULT_MIN_AGE`, `sweep_blobs_with`, `gc_blobs_with`; `GcReport.deferred`; `gonzalo gc --min-age`.
- Fixed: a sweep could delete a blob between a writer's upload and the manifest commit naming it.

- [ ] **Step 5: Validate the ADR set**

Check and report each: body/index status parity; both-sided amendment annotation for 0024↔0028 and 0027↔0028; 0028's body self-sustaining; every path and inter-ADR link 0028 cites resolves (test each with a command); no gaps or duplicates across 0001–0028; 0028's header lines, sections and filename conform to `docs/adr/template.md`.

- [ ] **Step 6: Run the full gate and commit**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features
git add docs CHANGELOG.md
git commit -m "$(printf 'docs(gc): ADR 0028 for the blob-GC grace period (#325)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```
