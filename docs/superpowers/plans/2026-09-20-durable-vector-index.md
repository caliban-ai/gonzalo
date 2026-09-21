# Durable Vector Index Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a gonzalo vector index survive the process that built it, so recall works across sessions without re-embedding.

**Architecture:** Vectors are bucketed by a hash of their key into a fixed number of shards; each shard is one content-addressed blob; one `VectorManifest` record names the shards and pins the embedding space. `RecordVectorIndex` implements the existing `VectorIndex` trait, composing a `MemoryVectorIndex` for queries and writing through to the store under OCC.

**Tech Stack:** Rust (edition 2024), `async-trait`, `serde_json`, blake3 via `gonzalo_core::ContentHash`, `tokio` for tests, `gonzalo-store-fs` + `tempfile` as dev-dependencies.

**Spec:** `docs/superpowers/specs/2026-09-20-durable-vector-index-design.md`

## Global Constraints

- Shard count defaults to **256**, is fixed when the index is created, and is read from the manifest thereafter.
- Shard blob format: magic `"GZVS"`, version `1`, little-endian, entries sorted by `(namespace, collection, id)`.
- `RecordKind::VectorManifest` is **`MergeClass::Opaque`** — never `Derived`, never `Structured`.
- OCC commits retry at most **5** attempts before erroring.
- Shard reads use a concurrency of **16**, matching `LIST_READ_CONCURRENCY` in `crates/gonzalo-store-s3/src/lib.rs:678`.
- `space` and `dim` are checked at `open` and the error names **both** the stored and the declared value.
- One writer per index. OCC detects a second writer and retries; it does not merge.
- The full gate is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace --all-targets --all-features`, `cargo test --workspace --all-features`. Note `--all-features`: the `hnsw` backend is compiled and tested in CI, so every trait change must be implemented there too.
- Commit messages end with `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/gonzalo-core/src/vector_manifest.rs` | **Create.** The `VectorManifest` body type: space, dim, shard count, shard-id → blob hash. Lives in core because `gc.rs` parses it and core cannot depend on the capability layer (ADR 0008). |
| `crates/gonzalo-core/src/record.rs` | **Modify.** Add the `VectorManifest` kind and its merge class. |
| `crates/gonzalo-core/src/lib.rs` | **Modify.** Export the new module. |
| `crates/gonzalo-core/src/gc.rs` | **Modify.** Add the vector-manifest arm to the blob mark set. |
| `crates/gonzalo-vector/src/shard.rs` | **Create.** Shard assignment and the shard blob codec. Pure functions, no I/O. |
| `crates/gonzalo-vector/src/record_index.rs` | **Create.** `RecordVectorIndex`: open/hydrate, write-through, OCC retry. |
| `crates/gonzalo-vector/src/index.rs` | **Modify.** Add `collect_where` and the `keys` implementation. |
| `crates/gonzalo-vector/src/hnsw.rs` | **Modify.** Add the `keys` implementation. |
| `crates/gonzalo-vector/src/lib.rs` | **Modify.** Trait additions (`upsert_many`, `keys`) and module wiring. |
| `crates/gonzalo-vector/Cargo.toml` | **Modify.** Add `gonzalo-store-fs` and `tempfile` as dev-dependencies. |
| `crates/gonzalo-knowledge/src/lib.rs` | **Modify.** `KnowledgeStore::open`, rebuilding chunk counts from a durable index. |
| `docs/adr/0027-durable-vector-index.md` | **Create.** The decision record. |

---

### Task 1: The `VectorManifest` body type

**Files:**
- Create: `crates/gonzalo-core/src/vector_manifest.rs`
- Modify: `crates/gonzalo-core/src/record.rs` (the `RecordKind` enum and the `merge_class` match)
- Modify: `crates/gonzalo-core/src/lib.rs` (module + re-export)

**Interfaces:**
- Consumes: `Body`, `ContentHash`, `CoreError`, `RecordKey`, `Result` from `crate`.
- Produces: `VectorManifest { space: String, dim: usize, shards: u16, entries: BTreeMap<u16, ContentHash> }` with `new(space, dim, shards)`, `key(namespace, index_id)`, `collection()`, `to_body()`, `from_body(&Body)`; and `RecordKind::VectorManifest`.

- [ ] **Step 1: Write the failing tests**

Create `crates/gonzalo-core/src/vector_manifest.rs` with only the test module plus the imports it needs:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MergeClass, RecordKind};

    fn hash(s: &str) -> ContentHash {
        ContentHash::of(s.as_bytes())
    }

    #[test]
    fn body_round_trips() {
        let mut m = VectorManifest::new("bge-small-en-v1.5", 384, 256);
        m.entries.insert(0, hash("shard-0"));
        m.entries.insert(17, hash("shard-17"));

        assert_eq!(VectorManifest::from_body(&m.to_body()).unwrap(), m);
    }

    #[test]
    fn body_bytes_are_deterministic_regardless_of_insert_order() {
        let mut a = VectorManifest::new("space", 8, 4);
        a.entries.insert(3, hash("x"));
        a.entries.insert(1, hash("y"));

        let mut b = VectorManifest::new("space", 8, 4);
        b.entries.insert(1, hash("y"));
        b.entries.insert(3, hash("x"));

        assert_eq!(a.to_body().bytes(), b.to_body().bytes());
    }

    #[test]
    fn from_body_rejects_non_manifest_bytes() {
        let garbage = Body::Inline(b"not json at all".to_vec());
        assert!(matches!(
            VectorManifest::from_body(&garbage),
            Err(CoreError::Serde(_))
        ));
    }

    #[test]
    fn key_addresses_namespace_and_index_id() {
        let k = VectorManifest::key("acme", "memories");
        assert_eq!(k.namespace, "acme");
        assert_eq!(k.collection, VectorManifest::collection());
        assert_eq!(k.id, "memories");
    }

    // Caller-supplied vectors cannot be re-derived from source, so `Derived`
    // (which resolves a divergence in favour of one side without a merge) would
    // silently discard the other side's vectors for good. `Structured` fails the
    // same way: two writers touching one shard produce different blob hashes and
    // a field merge keeps only one. Opaque surfaces it instead.
    #[test]
    fn vector_manifest_kind_is_opaque_not_derived() {
        assert_eq!(
            RecordKind::VectorManifest.merge_class(),
            MergeClass::Opaque
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core vector_manifest`
Expected: FAIL — the module is not declared in `lib.rs`, so it does not compile.

- [ ] **Step 3: Write the implementation**

Put this **above** the test module in `crates/gonzalo-core/src/vector_manifest.rs`:

```rust
//! The identity layer over a durable vector index: which shards exist, which
//! blob holds each, and which embedding space they are all in (ADR 0027).
//!
//! Structurally this mirrors [`Manifest`](crate::Manifest) — a record body
//! mapping identifiers to [`ContentHash`]es of out-of-line content — but it is
//! *not* regenerable. A code-graph manifest can be rebuilt by re-reading source;
//! a vector manifest cannot, because with caller-supplied embeddings gonzalo
//! never sees the model that produced the vectors. That difference is why this
//! kind is [`MergeClass::Opaque`](crate::MergeClass::Opaque) while a graph
//! manifest is `Derived`.

use crate::{Body, ContentHash, CoreError, RecordKey, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The collection segment under which every vector index's manifest is addressed.
const MANIFEST_COLLECTION: &str = "vector-manifest";

/// A durable vector index's manifest body.
///
/// `entries` is a [`BTreeMap`] so serialization has deterministic key order: a
/// manifest with the same contents always hashes identically, keeping its record
/// revision stable under content-addressed dedup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorManifest {
    /// Caller-declared embedding space, e.g. `"bge-small-en-v1.5"`.
    ///
    /// Gonzalo cannot verify a vector came from this model — it never sees the
    /// model. This records what the writer *declared*, which is enough to catch
    /// configuration drift when a reader declares something else.
    pub space: String,
    /// Vector dimension. Every entry in every shard carries exactly this many floats.
    pub dim: usize,
    /// Number of shards, fixed when the index is created.
    pub shards: u16,
    /// Shard id -> the blob holding that shard's vectors.
    pub entries: BTreeMap<u16, ContentHash>,
}

impl VectorManifest {
    /// An empty manifest for a new index.
    pub fn new(space: impl Into<String>, dim: usize, shards: u16) -> Self {
        Self {
            space: space.into(),
            dim,
            shards,
            entries: BTreeMap::new(),
        }
    }

    /// The stable [`RecordKey`] addressing the index `index_id` in `namespace`.
    pub fn key(namespace: impl Into<String>, index_id: impl Into<String>) -> RecordKey {
        RecordKey::new(namespace, MANIFEST_COLLECTION, index_id)
    }

    /// The collection segment every vector manifest is addressed under. A
    /// [`KeyPrefix`](crate::KeyPrefix) with this collection and no namespace
    /// lists every index's manifest — the set GC must union to mark live shards.
    pub fn collection() -> &'static str {
        MANIFEST_COLLECTION
    }

    /// Serialize into an inline record [`Body`] (deterministic key order).
    pub fn to_body(&self) -> Body {
        Body::Inline(serde_json::to_vec(self).expect("VectorManifest serializes"))
    }

    /// Reconstruct from a record [`Body`]. Errors if the bytes are not a valid
    /// serialized manifest.
    pub fn from_body(body: &Body) -> Result<Self> {
        serde_json::from_slice(body.bytes()).map_err(|e| CoreError::Serde(e.to_string()))
    }
}
```

In `crates/gonzalo-core/src/record.rs`, add the variant to `RecordKind` immediately after `GraphManifest`:

```rust
    /// The manifest of a durable vector index: which shards exist, which blob
    /// holds each, and the embedding space they are in. Unlike
    /// [`GraphManifest`](Self::GraphManifest) it is **not** regenerable — with
    /// caller-supplied embeddings gonzalo never sees the model — so it merges
    /// [`Opaque`](MergeClass::Opaque). See ADR 0027.
    VectorManifest,
```

In the same file, add to the `merge_class` match (the match has no wildcard arm, so it will not compile until you do):

```rust
            // Not regenerable: a lost side is lost vectors, so surface the
            // divergence rather than resolving it silently.
            RecordKind::VectorManifest => MergeClass::Opaque,
```

In `crates/gonzalo-core/src/lib.rs`, after the `manifest` lines:

```rust
pub mod vector_manifest;
pub use vector_manifest::VectorManifest;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core vector_manifest`
Expected: PASS, 5 tests.

- [ ] **Step 5: Check nothing else broke on the exhaustive match**

Run: `cargo build --workspace --all-targets --all-features`
Expected: **fails** in `gonzalo-knowledge`. `chunk()` at
`crates/gonzalo-knowledge/src/lib.rs:244` matches `RecordKind` exhaustively with
no wildcard, so the new variant must be classified there. A vector manifest is a
shard-id → blob-hash map, not natural-language text, so it joins the
not-knowledge-bearing arm beside `GraphManifest`:

```rust
        RecordKind::Checkpoint
        | RecordKind::GraphManifest
        | RecordKind::VectorManifest
        | RecordKind::Tombstone
```

Re-run the build afterwards. Any other exhaustive `RecordKind` match that fails
gets an arm mirroring the `GraphManifest` one at that site.

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-core/src/vector_manifest.rs crates/gonzalo-core/src/record.rs crates/gonzalo-core/src/lib.rs crates/gonzalo-knowledge/src/lib.rs
git commit -m "$(printf 'feat(core): add the VectorManifest record kind and body (#323)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 2: GC must mark vector shard blobs

This task fixes a silent data-loss bug that Task 1 just created. `live_blob_hashes` gates its manifest arm on `record.kind == RecordKind::GraphManifest` exactly, so a vector manifest's shard blobs are currently unreachable from the mark set and the next `gonzalo gc` deletes every one of them while the manifest still points at them.

**Files:**
- Modify: `crates/gonzalo-core/src/gc.rs`

**Interfaces:**
- Consumes: `VectorManifest::{new, to_body}` and `RecordKind::VectorManifest` from Task 1.
- Produces: no new public API — `live_blob_hashes` gains behaviour.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/gonzalo-core/src/gc.rs`:

```rust
    // Red-first guard on real data loss: before the VectorManifest arm exists,
    // the mark set misses every shard blob and a sweep deletes live vectors.
    #[test]
    fn live_set_includes_vector_manifest_shards() {
        use crate::VectorManifest;

        let mut vm = VectorManifest::new("bge-small-en-v1.5", 384, 256);
        vm.entries.insert(0, h("shard-0"));
        vm.entries.insert(9, h("shard-9"));

        let rec = record("memories", RecordKind::VectorManifest, vm.to_body());

        let live = live_blob_hashes([&rec]).unwrap();
        assert!(live.contains(&h("shard-0")));
        assert!(live.contains(&h("shard-9")));
    }

    #[test]
    fn a_graph_manifest_and_a_vector_manifest_both_mark() {
        use crate::VectorManifest;

        let mut gm = Manifest::new();
        gm.insert("src/lib.rs", h("slice"));
        let graph = record("view", RecordKind::GraphManifest, gm.to_body());

        let mut vm = VectorManifest::new("space", 8, 4);
        vm.entries.insert(1, h("shard"));
        let vector = record("memories", RecordKind::VectorManifest, vm.to_body());

        let live = live_blob_hashes([&graph, &vector]).unwrap();
        assert_eq!(live, BTreeSet::from([h("slice"), h("shard")]));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-core gc::tests::live_set_includes_vector_manifest_shards`
Expected: FAIL — `assert!(live.contains(&h("shard-0")))` is false, because nothing reads a vector manifest's entries.

- [ ] **Step 3: Write the implementation**

In `crates/gonzalo-core/src/gc.rs`, inside `live_blob_hashes`, add after the `GraphManifest` arm:

```rust
        // A vector manifest's shards are live blobs. Without this the next sweep
        // deletes every vector in the index while the manifest still names them,
        // and — unlike a graph slice — they cannot be regenerated from source.
        if record.kind == RecordKind::VectorManifest {
            live.extend(
                crate::VectorManifest::from_body(&record.body)?
                    .entries
                    .into_values(),
            );
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-core gc`
Expected: PASS, including both new tests and every pre-existing gc test.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-core/src/gc.rs
git commit -m "$(printf 'fix(core): mark vector manifest shards as live in gc (#323)\n\nWithout this arm the next sweep deletes every vector blob while the manifest\nstill points at them, and caller-supplied vectors cannot be regenerated.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 3: Shard assignment and the shard blob codec

Pure functions, no I/O, no store — everything here is unit-testable in isolation.

**Files:**
- Create: `crates/gonzalo-vector/src/shard.rs`
- Modify: `crates/gonzalo-vector/src/lib.rs` (declare the module)

**Interfaces:**
- Consumes: `gonzalo_core::{ContentHash, CoreError, RecordKey, Result}`.
- Produces:
  - `pub fn shard_of(key: &RecordKey, shards: NonZeroU16) -> u16`
  - `pub fn encode_shard(dim: usize, entries: &[(RecordKey, Vec<f32>)]) -> Vec<u8>`
  - `pub fn decode_shard(bytes: &[u8]) -> Result<(usize, Vec<(RecordKey, Vec<f32>)>)>` returning `(dim, entries)`
  - `pub const DEFAULT_SHARDS: NonZeroU16` (256)

- [ ] **Step 1: Write the failing tests**

Create `crates/gonzalo-vector/src/shard.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> RecordKey {
        RecordKey::new("ns", "coll", id)
    }

    #[test]
    fn shard_assignment_is_stable_across_calls() {
        let k = key("a");
        assert_eq!(shard_of(&k, 256), shard_of(&k, 256));
    }

    // The assignment must not drift between releases, or every existing index
    // silently loses track of where its vectors live. Pinning one literal makes
    // an accidental change to the hashing fail loudly here.
    #[test]
    fn shard_assignment_is_pinned_to_a_known_value() {
        assert_eq!(shard_of(&key("a"), 256), 121);
    }

    #[test]
    fn shard_assignment_respects_the_shard_count() {
        for i in 0..100 {
            assert!(shard_of(&key(&i.to_string()), 4) < 4);
        }
    }

    #[test]
    fn round_trips_entries_and_dim() {
        let entries = vec![
            (key("a"), vec![1.0, 2.0, 3.0]),
            (key("b"), vec![-1.5, 0.0, 2.5]),
        ];
        let bytes = encode_shard(3, &entries);
        let (dim, decoded) = decode_shard(&bytes).unwrap();
        assert_eq!(dim, 3);
        assert_eq!(decoded, entries);
    }

    #[test]
    fn round_trips_an_empty_shard() {
        let bytes = encode_shard(4, &[]);
        let (dim, decoded) = decode_shard(&bytes).unwrap();
        assert_eq!(dim, 4);
        assert!(decoded.is_empty());
    }

    // Identical contents must produce identical bytes, so an untouched shard
    // hashes to the blob already stored and costs nothing to "rewrite".
    #[test]
    fn encoding_is_deterministic_regardless_of_input_order() {
        let a = vec![(key("b"), vec![1.0]), (key("a"), vec![2.0])];
        let b = vec![(key("a"), vec![2.0]), (key("b"), vec![1.0])];
        assert_eq!(encode_shard(1, &a), encode_shard(1, &b));
    }

    #[test]
    fn decode_rejects_a_bad_magic() {
        let mut bytes = encode_shard(1, &[(key("a"), vec![1.0])]);
        bytes[0] = b'X';
        assert!(matches!(decode_shard(&bytes), Err(CoreError::Backend(_))));
    }

    #[test]
    fn decode_rejects_an_unknown_version() {
        let mut bytes = encode_shard(1, &[(key("a"), vec![1.0])]);
        bytes[4] = 99;
        assert!(matches!(decode_shard(&bytes), Err(CoreError::Backend(_))));
    }

    #[test]
    fn decode_rejects_a_truncated_shard() {
        let bytes = encode_shard(2, &[(key("a"), vec![1.0, 2.0])]);
        let truncated = &bytes[..bytes.len() - 3];
        assert!(matches!(decode_shard(truncated), Err(CoreError::Backend(_))));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-vector shard`
Expected: FAIL to compile — `shard_of`, `encode_shard`, `decode_shard` and `EXPECTED_SHARD_FOR_NS_COLL_A` do not exist.

- [ ] **Step 3: Write the implementation**

Put this above the test module in `crates/gonzalo-vector/src/shard.rs`:

```rust
//! Shard assignment and the on-blob format for a durable vector index.
//!
//! Vectors are bucketed by a hash of their key so a write touches one shard
//! rather than the whole index, and a shard's bytes are deterministic so an
//! unchanged shard content-addresses to the blob already stored.

use gonzalo_core::{ContentHash, CoreError, RecordKey, Result};

/// Default number of shards for a new index. At ~100k chunks of 384 f32s this
/// is ~600 KB per shard: 256 reads to open, one ~600 KB rewrite per upsert.
pub const DEFAULT_SHARDS: u16 = 256;

const MAGIC: &[u8; 4] = b"GZVS";
const VERSION: u8 = 1;

/// Which shard `key` belongs to.
///
/// Uses blake3 via [`ContentHash`] rather than [`std::hash::DefaultHasher`],
/// whose output is explicitly not stable across releases — a drift there would
/// silently strand every vector in every existing index.
pub fn shard_of(key: &RecordKey, shards: u16) -> u16 {
    let s = format!("{}/{}/{}", key.namespace, key.collection, key.id);
    let hex = ContentHash::of(s.as_bytes()).0;
    let bits = u16::from_str_radix(&hex[..4], 16).expect("blake3 hex is 64 hex digits");
    bits % shards
}

/// Encode one shard. Entries are sorted by key, so identical contents always
/// produce identical bytes.
pub fn encode_shard(dim: usize, entries: &[(RecordKey, Vec<f32>)]) -> Vec<u8> {
    let mut sorted: Vec<&(RecordKey, Vec<f32>)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&(dim as u32).to_le_bytes());
    out.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    for (key, vector) in sorted {
        put_str16(&mut out, &key.namespace);
        put_str16(&mut out, &key.collection);
        out.extend_from_slice(&(key.id.len() as u32).to_le_bytes());
        out.extend_from_slice(key.id.as_bytes());
        for f in vector {
            out.extend_from_slice(&f.to_le_bytes());
        }
    }
    out
}

/// Decode one shard, returning its dimension and entries.
pub fn decode_shard(bytes: &[u8]) -> Result<(usize, Vec<(RecordKey, Vec<f32>)>)> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(4)? != MAGIC {
        return Err(CoreError::Backend("vector shard: bad magic".into()));
    }
    let version = r.take(1)?[0];
    if version != VERSION {
        return Err(CoreError::Backend(format!(
            "vector shard: unsupported version {version}, expected {VERSION}"
        )));
    }
    let dim = r.u32()? as usize;
    let count = r.u32()? as usize;

    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let namespace = r.str16()?;
        let collection = r.str16()?;
        let id_len = r.u32()? as usize;
        let id = String::from_utf8(r.take(id_len)?.to_vec())
            .map_err(|e| CoreError::Backend(format!("vector shard: bad utf8 in id: {e}")))?;
        let mut vector = Vec::with_capacity(dim);
        for _ in 0..dim {
            let b: [u8; 4] = r.take(4)?.try_into().expect("took exactly 4 bytes");
            vector.push(f32::from_le_bytes(b));
        }
        entries.push((RecordKey::new(namespace, collection, id), vector));
    }
    Ok((dim, entries))
}

fn put_str16(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(overflow)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| CoreError::Backend("vector shard: truncated".into()))?;
        self.at = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32> {
        let b: [u8; 4] = self.take(4)?.try_into().expect("took exactly 4 bytes");
        Ok(u32::from_le_bytes(b))
    }

    fn str16(&mut self) -> Result<String> {
        let b: [u8; 2] = self.take(2)?.try_into().expect("took exactly 2 bytes");
        let len = u16::from_le_bytes(b) as usize;
        String::from_utf8(self.take(len)?.to_vec())
            .map_err(|e| CoreError::Backend(format!("vector shard: bad utf8: {e}")))
    }
}

fn overflow() -> CoreError {
    CoreError::Backend("vector shard: length overflow".into())
}
```

In `crates/gonzalo-vector/src/lib.rs`, add near the other module declarations:

```rust
pub mod shard;
pub use shard::{DEFAULT_SHARDS, shard_of};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-vector shard`
Expected: PASS, 9 tests.

The literal `121` in `shard_assignment_is_pinned_to_a_known_value` is the real
value of `blake3("ns/coll/a")[..4] as u16 % 256` — verified against
`ContentHash::of` before this plan was written, not guessed. If it fails, the
hashing changed and every existing index has been stranded; do not "fix" the
test by updating the number without understanding why it moved.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-vector/src/shard.rs crates/gonzalo-vector/src/lib.rs
git commit -m "$(printf 'feat(vector): add shard assignment and the shard blob codec (#323)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 4: `VectorIndex` trait additions

**Files:**
- Modify: `crates/gonzalo-vector/src/lib.rs` (the `VectorIndex` trait)
- Modify: `crates/gonzalo-vector/src/index.rs` (`collect_where`, `keys`)
- Modify: `crates/gonzalo-vector/src/hnsw.rs` (`keys`)

**Interfaces:**
- Produces:
  - `async fn upsert_many(&self, items: Vec<(RecordKey, Vec<f32>)>) -> Result<()>` on `VectorIndex`, **with** a default that loops over `upsert`.
  - `async fn keys(&self, filter: &KeyPrefix) -> Result<Vec<RecordKey>>` on `VectorIndex`, **required** — there is no way to enumerate an arbitrary index, and a default returning an error would turn a compile-time gap into a runtime one.
  - `pub fn collect_where(&self, pred: impl Fn(&RecordKey) -> bool) -> Vec<(RecordKey, Vec<f32>)>` on `MemoryVectorIndex`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/gonzalo-vector/src/index.rs`:

```rust
    #[tokio::test]
    async fn keys_lists_everything_under_the_filter() {
        let idx = MemoryVectorIndex::new();
        idx.upsert(RecordKey::new("ns", "a", "1"), vec![1.0]).await.unwrap();
        idx.upsert(RecordKey::new("ns", "b", "2"), vec![2.0]).await.unwrap();

        let all = idx.keys(&KeyPrefix::default()).await.unwrap();
        assert_eq!(all.len(), 2);

        let only_a = idx
            .keys(&KeyPrefix { namespace: None, collection: Some("a".into()) })
            .await
            .unwrap();
        assert_eq!(only_a, vec![RecordKey::new("ns", "a", "1")]);
    }

    #[tokio::test]
    async fn an_arc_delegates_every_method_including_upsert_many() {
        use std::sync::Arc;
        let idx: Arc<MemoryVectorIndex> = Arc::new(MemoryVectorIndex::new());
        idx.upsert_many(vec![(RecordKey::new("ns", "c", "1"), vec![1.0])])
            .await
            .unwrap();
        assert_eq!(idx.keys(&KeyPrefix::default()).await.unwrap().len(), 1);
        idx.remove(&RecordKey::new("ns", "c", "1")).await.unwrap();
        assert!(idx.keys(&KeyPrefix::default()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_many_inserts_every_item() {
        let idx = MemoryVectorIndex::new();
        idx.upsert_many(vec![
            (RecordKey::new("ns", "c", "1"), vec![1.0]),
            (RecordKey::new("ns", "c", "2"), vec![2.0]),
        ])
        .await
        .unwrap();

        assert_eq!(idx.keys(&KeyPrefix::default()).await.unwrap().len(), 2);
    }

    #[test]
    fn collect_where_returns_only_matching_entries() {
        let idx = MemoryVectorIndex::new();
        futures_lite_block(idx.upsert(RecordKey::new("ns", "c", "keep"), vec![1.0]));
        futures_lite_block(idx.upsert(RecordKey::new("ns", "c", "drop"), vec![2.0]));

        let got = idx.collect_where(|k| k.id == "keep");
        assert_eq!(got, vec![(RecordKey::new("ns", "c", "keep"), vec![1.0])]);
    }
```

Replace `futures_lite_block(...)` with whatever this test module already uses to drive an async call from a sync test; if there is none, make the test `#[tokio::test] async fn` and `.await` the upserts instead. Check the top of the existing `mod tests` before writing it.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-vector --all-features`
Expected: FAIL to compile — `keys`, `upsert_many` and `collect_where` do not exist.

- [ ] **Step 3: Write the implementation**

In `crates/gonzalo-vector/src/lib.rs`, add to the `VectorIndex` trait:

```rust
    /// Insert or replace many vectors at once.
    ///
    /// The default loops over [`upsert`](Self::upsert). A durable backend
    /// overrides this: committing once per vector would mean one manifest
    /// commit per vector on a bulk load.
    async fn upsert_many(&self, items: Vec<(RecordKey, Vec<f32>)>) -> Result<()> {
        for (key, vector) in items {
            self.upsert(key, vector).await?;
        }
        Ok(())
    }

    /// Every key in the index matching `filter`. Order is unspecified.
    ///
    /// Used to rebuild derived state that would otherwise be lost across a
    /// restart, such as a `KnowledgeStore`'s per-record chunk counts.
    async fn keys(&self, filter: &KeyPrefix) -> Result<Vec<RecordKey>>;
```

In `crates/gonzalo-vector/src/index.rs`, add to `impl MemoryVectorIndex`:

```rust
    /// Entries whose key satisfies `pred`.
    ///
    /// Predicate-shaped rather than shard-shaped so sharding does not leak into
    /// the in-memory index; a durable backend passes its own shard test here and
    /// clones one shard rather than the whole index.
    pub fn collect_where(&self, pred: impl Fn(&RecordKey) -> bool) -> Vec<(RecordKey, Vec<f32>)> {
        let map = self.store.lock().expect("mutex poisoned");
        map.iter()
            .filter(|(key, _)| pred(key))
            .map(|(key, vector)| (key.clone(), vector.clone()))
            .collect()
    }
```

And to `impl VectorIndex for MemoryVectorIndex`:

```rust
    async fn keys(&self, filter: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let map = self.store.lock().expect("mutex poisoned");
        Ok(map.keys().filter(|k| filter.matches(k)).cloned().collect())
    }
```

In `crates/gonzalo-vector/src/hnsw.rs`, add to `impl VectorIndex for HnswVectorIndex`:

```rust
    async fn keys(&self, filter: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let inner = self.inner.lock().expect("mutex poisoned");
        Ok(inner
            .key_to_id
            .keys()
            .filter(|k| filter.matches(k))
            .cloned()
            .collect())
    }
```

Finally, in `crates/gonzalo-vector/src/lib.rs`, add a delegating impl for `Arc<T>`
so one index can back two owners — a `KnowledgeStore` and a test, or two stores
sharing an index. Without it, every sharing caller invents its own newtype:

```rust
#[async_trait]
impl<T: VectorIndex + ?Sized> VectorIndex for std::sync::Arc<T> {
    async fn upsert(&self, key: RecordKey, vector: Vec<f32>) -> Result<()> {
        (**self).upsert(key, vector).await
    }
    async fn remove(&self, key: &RecordKey) -> Result<()> {
        (**self).remove(key).await
    }
    async fn query(&self, query: &[f32], k: usize, filter: &KeyPrefix) -> Result<Vec<Match>> {
        (**self).query(query, k, filter).await
    }
    async fn keys(&self, filter: &KeyPrefix) -> Result<Vec<RecordKey>> {
        (**self).keys(filter).await
    }
    async fn upsert_many(&self, items: Vec<(RecordKey, Vec<f32>)>) -> Result<()> {
        (**self).upsert_many(items).await
    }
}
```

Note `upsert_many` is delegated explicitly: without it `Arc<RecordVectorIndex>`
would silently fall back to the trait's looping default and commit once per
vector, which is exactly what Task 7 exists to prevent.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-vector --all-features`
Expected: PASS. The `--all-features` matters — it is what compiles the `hnsw` backend, and CI runs it.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-vector/src/lib.rs crates/gonzalo-vector/src/index.rs crates/gonzalo-vector/src/hnsw.rs
git commit -m "$(printf 'feat(vector): add upsert_many and keys to VectorIndex (#323)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 5: `RecordVectorIndex::open` and the load path

**Files:**
- Create: `crates/gonzalo-vector/src/record_index.rs`
- Modify: `crates/gonzalo-vector/src/lib.rs` (declare + re-export)
- Modify: `crates/gonzalo-vector/Cargo.toml` (dev-dependencies)

**Interfaces:**
- Consumes: `shard_of`, `decode_shard`, `DEFAULT_SHARDS` (Task 3); `VectorManifest` and `RecordKind::VectorManifest` (Task 1); `MemoryVectorIndex::collect_where` and the trait methods (Task 4).
- Produces: `RecordVectorIndex<S>` with `open(store: S, key: RecordKey, space: &str, dim: usize) -> Result<Self>`, `open_with_shards(store, key, space, dim, shards: NonZeroU16)`, and `store(&self) -> &S`.

- [ ] **Step 1: Add the dev-dependencies**

In `crates/gonzalo-vector/Cargo.toml`, under `[dev-dependencies]`:

```toml
gonzalo-store-fs = { workspace = true }
tempfile         = { workspace = true }
```

`MemStore` in `gonzalo-core` does **not** implement `BlobStore` — only `FsStore` does — so tests need a real `FsStore` over a `tempfile::TempDir`.

- [ ] **Step 2: Write the failing tests**

Create `crates/gonzalo-vector/src/record_index.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_core::{BlobStore, Identity, Meta, Record, RecordKind, Store};
    use gonzalo_store_fs::FsStore;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        TempDir::new().unwrap()
    }

    /// A fresh handle onto the same directory. `FsStore` is not `Clone`, and a
    /// second handle is what a restart actually looks like anyway.
    fn fs(dir: &TempDir) -> FsStore {
        FsStore::new(dir.path())
    }

    fn index_key() -> RecordKey {
        VectorManifest::key("ns", "memories")
    }

    #[tokio::test]
    async fn opening_a_missing_index_starts_empty() {
        let dir = tmp();
        let s = fs(&dir);
        let idx = RecordVectorIndex::open(s, index_key(), "space-a", 3).await.unwrap();
        assert!(idx.keys(&KeyPrefix::default()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn opening_hydrates_vectors_from_shard_blobs() {
        let dir = tmp();
        let s = fs(&dir);
        let k = RecordKey::new("ns", "coll", "a");

        // Hand-build a one-shard index so the load path is tested without
        // depending on the write path, which does not exist yet.
        let entries = vec![(k.clone(), vec![1.0, 0.0, 0.0])];
        let hash = s.put_blob(&crate::shard::encode_shard(3, &entries)).await.unwrap();
        let mut vm = VectorManifest::new("space-a", 3, DEFAULT_SHARDS);
        vm.entries.insert(shard_of(&k, DEFAULT_SHARDS), hash);
        let rec = Record::create(
            index_key(),
            RecordKind::VectorManifest,
            vm.to_body(),
            Meta::new(Identity::new("test"), "test"),
        );
        s.put(rec, None).await.unwrap();

        let idx = RecordVectorIndex::open(s, index_key(), "space-a", 3).await.unwrap();
        let hits = idx.query(&[1.0, 0.0, 0.0], 5, &KeyPrefix::default()).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, k);
    }

    // The realistic failure this guards: a swapped embedder. Two models can share
    // a dimension (all-MiniLM-L6-v2 and bge-small are both 384), so dimension
    // alone would let mismatched vectors score against each other forever.
    #[tokio::test]
    async fn opening_with_a_different_space_errors_naming_both() {
        let dir = tmp();
        let s = fs(&dir);
        let vm = VectorManifest::new("space-a", 3, DEFAULT_SHARDS);
        let rec = Record::create(
            index_key(),
            RecordKind::VectorManifest,
            vm.to_body(),
            Meta::new(Identity::new("test"), "test"),
        );
        s.put(rec, None).await.unwrap();

        let err = RecordVectorIndex::open(s, index_key(), "space-b", 3)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("space-a"), "stored space missing from: {err}");
        assert!(err.contains("space-b"), "declared space missing from: {err}");
    }

    #[tokio::test]
    async fn opening_with_a_different_dim_errors_naming_both() {
        let dir = tmp();
        let s = fs(&dir);
        let vm = VectorManifest::new("space-a", 3, DEFAULT_SHARDS);
        let rec = Record::create(
            index_key(),
            RecordKind::VectorManifest,
            vm.to_body(),
            Meta::new(Identity::new("test"), "test"),
        );
        s.put(rec, None).await.unwrap();

        let err = RecordVectorIndex::open(s, index_key(), "space-a", 8)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains('3'), "stored dim missing from: {err}");
        assert!(err.contains('8'), "declared dim missing from: {err}");
    }

    // A manifest naming a blob that is gone means gc swept a live blob or a write
    // was lost. Opening short and quiet would turn that into missing search hits.
    #[tokio::test]
    async fn a_missing_shard_blob_is_a_loud_error() {
        let dir = tmp();
        let s = fs(&dir);
        let mut vm = VectorManifest::new("space-a", 3, DEFAULT_SHARDS);
        vm.entries.insert(7, gonzalo_core::ContentHash::of(b"never stored"));
        let rec = Record::create(
            index_key(),
            RecordKind::VectorManifest,
            vm.to_body(),
            Meta::new(Identity::new("test"), "test"),
        );
        s.put(rec, None).await.unwrap();

        let err = RecordVectorIndex::open(s, index_key(), "space-a", 3)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains('7'), "shard id missing from: {err}");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-vector record_index`
Expected: FAIL to compile — `RecordVectorIndex` does not exist.

- [ ] **Step 4: Write the implementation**

Put above the test module in `crates/gonzalo-vector/src/record_index.rs`:

```rust
//! A [`VectorIndex`] whose contents survive the process that built it (ADR 0027).
//!
//! Vectors live in content-addressed shard blobs named by one
//! [`VectorManifest`] record. Queries are served from an in-memory
//! [`MemoryVectorIndex`] hydrated at [`open`](RecordVectorIndex::open); writes
//! go through to the store under OCC.
//!
//! **One writer per index.** A concurrent writer is detected and retried, not
//! merged, and a second writer's in-memory view can be stale until it reopens.

use crate::shard::{DEFAULT_SHARDS, decode_shard, encode_shard, shard_of};
use crate::{Match, MemoryVectorIndex, VectorIndex};
use async_trait::async_trait;
use gonzalo_core::{
    BlobStore, CoreError, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
    Result, Store, VectorManifest,
};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;

/// How many shard blobs to read at once when opening. Matches
/// `LIST_READ_CONCURRENCY` in `gonzalo-store-s3`, which settled on 16 for the
/// same reason: enough parallelism to hide latency, not enough to thrash.
const SHARD_READ_CONCURRENCY: usize = 16;

/// How many times a commit re-reads and retries before giving up.
const MAX_COMMIT_ATTEMPTS: usize = 5;

/// One pending change to apply and persist.
#[derive(Clone, Debug)]
enum Delta {
    Upsert(RecordKey, Vec<f32>),
    Remove(RecordKey),
}

pub struct RecordVectorIndex<S> {
    store: S,
    key: RecordKey,
    space: String,
    dim: usize,
    shards: NonZeroU16,
    inner: MemoryVectorIndex,
    meta: Meta,
}

impl<S: Store + BlobStore> RecordVectorIndex<S> {
    /// Open the index at `key`, hydrating it from `store`.
    ///
    /// Errors if a manifest exists whose `space` or `dim` differs from the
    /// declared one, naming both values, so a swapped embedder surfaces once at
    /// startup rather than as quietly wrong rankings on every later query. A
    /// missing manifest starts an empty index created on the first commit.
    pub async fn open(store: S, key: RecordKey, space: &str, dim: usize) -> Result<Self> {
        Self::open_with_shards(store, key, space, dim, DEFAULT_SHARDS).await
    }

    /// As [`open`](Self::open), but choosing the shard count for a **new**
    /// index. An existing manifest's shard count always wins.
    pub async fn open_with_shards(
        store: S,
        key: RecordKey,
        space: &str,
        dim: usize,
        shards: NonZeroU16,
    ) -> Result<Self> {
        let existing = store.get(&key).await?;
        let (shards, manifest) = match &existing {
            None => (shards, None),
            Some(record) => {
                let m = VectorManifest::from_body(&record.body)?;
                if m.space != space {
                    return Err(CoreError::Invalid(format!(
                        "vector index {key}: stored embedding space is {:?}, opened as {:?}",
                        m.space, space
                    )));
                }
                if m.dim != dim {
                    return Err(CoreError::Invalid(format!(
                        "vector index {key}: stored dimension is {}, opened as {}",
                        m.dim, dim
                    )));
                }
                // A stored shard count of zero means the manifest is corrupt.
                // Surface it here rather than letting it reach `shard_of`.
                let stored = NonZeroU16::new(m.shards).ok_or_else(|| {
                    CoreError::Backend(format!(
                        "vector index {key}: manifest declares 0 shards"
                    ))
                })?;
                (stored, Some(m))
            }
        };

        let index = Self {
            store,
            key,
            space: space.to_string(),
            dim,
            shards,
            inner: MemoryVectorIndex::new(),
            meta: Meta::new(Identity::new("gonzalo-vector"), "gonzalo-vector"),
        };

        if let Some(m) = manifest {
            let ids: Vec<u16> = m.entries.keys().copied().collect();
            for batch in ids.chunks(SHARD_READ_CONCURRENCY) {
                for &id in batch {
                    index.hydrate_shard(&m, id).await?;
                }
            }
        }
        Ok(index)
    }

    /// Borrow the underlying store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Load one shard from `manifest` into the in-memory index, replacing
    /// whatever that shard currently holds.
    async fn hydrate_shard(&self, manifest: &VectorManifest, id: u16) -> Result<()> {
        let Some(hash) = manifest.entries.get(&id) else {
            for (key, _) in self.inner.collect_where(|k| shard_of(k, self.shards) == id) {
                self.inner.remove(&key).await?;
            }
            return Ok(());
        };
        let bytes = self.store.get_blob(hash).await?.ok_or_else(|| {
            CoreError::Backend(format!(
                "vector index {}: shard {id} names blob {} but it is absent",
                self.key, hash.0
            ))
        })?;
        let (dim, entries) = decode_shard(&bytes)?;
        if dim != self.dim {
            return Err(CoreError::Backend(format!(
                "vector index {}: shard {id} has dimension {dim}, manifest says {}",
                self.key, self.dim
            )));
        }
        for (key, _) in self.inner.collect_where(|k| shard_of(k, self.shards) == id) {
            self.inner.remove(&key).await?;
        }
        self.inner.upsert_many(entries).await
    }
}
```

Add to `crates/gonzalo-vector/src/lib.rs`:

```rust
pub mod record_index;
pub use record_index::RecordVectorIndex;
```

The `VectorIndex` impl arrives in Task 6. To make Task 5's tests compile now, add this minimal impl at the bottom of `record_index.rs` — Task 6 replaces `upsert` and `remove`:

```rust
#[async_trait]
impl<S: Store + BlobStore> VectorIndex for RecordVectorIndex<S> {
    async fn upsert(&self, _key: RecordKey, _vector: Vec<f32>) -> Result<()> {
        Err(CoreError::Backend("not yet implemented".into()))
    }

    async fn remove(&self, _key: &RecordKey) -> Result<()> {
        Err(CoreError::Backend("not yet implemented".into()))
    }

    async fn query(&self, query: &[f32], k: usize, filter: &KeyPrefix) -> Result<Vec<Match>> {
        self.inner.query(query, k, filter).await
    }

    async fn keys(&self, filter: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.inner.keys(filter).await
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-vector record_index`
Expected: PASS, 5 tests.

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-vector/src/record_index.rs crates/gonzalo-vector/src/lib.rs crates/gonzalo-vector/Cargo.toml
git commit -m "$(printf 'feat(vector): add RecordVectorIndex open and load path (#323)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 6: The write path, with OCC retry

**Files:**
- Modify: `crates/gonzalo-vector/src/record_index.rs`

**Interfaces:**
- Consumes: everything from Task 5, plus `encode_shard` from Task 3.
- Produces: working `upsert` / `remove` on `RecordVectorIndex`, and a private `commit(&self, deltas: Vec<Delta>) -> Result<()>`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/gonzalo-vector/src/record_index.rs`:

```rust
    // The headline claim of the whole ticket.
    #[tokio::test]
    async fn vectors_survive_reopening_the_index() {
        let dir = tmp();
        let s = fs(&dir);
        let k = RecordKey::new("ns", "coll", "a");

        let idx = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3).await.unwrap();
        idx.upsert(k.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        drop(idx);

        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3).await.unwrap();
        let hits = reopened.query(&[1.0, 0.0, 0.0], 5, &KeyPrefix::default()).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, k);
    }

    #[tokio::test]
    async fn a_removed_vector_stays_removed_after_reopening() {
        let dir = tmp();
        let s = fs(&dir);
        let k = RecordKey::new("ns", "coll", "a");

        let idx = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3).await.unwrap();
        idx.upsert(k.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        idx.remove(&k).await.unwrap();
        drop(idx);

        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3).await.unwrap();
        assert!(reopened.keys(&KeyPrefix::default()).await.unwrap().is_empty());
    }

    // Two handles on one index is the case OCC exists for: the loser must not
    // drop the winner's vector, nor its own.
    #[tokio::test]
    async fn a_concurrent_writer_loses_neither_sides_vectors() {
        let dir = tmp();
        let s = fs(&dir);
        let a = RecordKey::new("ns", "coll", "a");
        let b = RecordKey::new("ns", "coll", "b");

        let one = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3).await.unwrap();
        let two = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3).await.unwrap();

        one.upsert(a.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        // `two` opened before that commit, so its manifest read is stale and
        // this commit must conflict, reload, and retry.
        two.upsert(b.clone(), vec![0.0, 1.0, 0.0]).await.unwrap();

        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3).await.unwrap();
        let mut got = reopened.keys(&KeyPrefix::default()).await.unwrap();
        got.sort();
        assert_eq!(got, vec![a, b]);
    }

    // A lost race leaves the loser's shard blob unreferenced. Nothing points at
    // it, so gc must reclaim it — and must not touch the shard that won.
    #[tokio::test]
    async fn a_shard_blob_orphaned_by_a_lost_race_is_reclaimed() {
        let dir = tmp();
        let s = fs(&dir);
        let a = RecordKey::new("ns", "coll", "a");
        let b = RecordKey::new("ns", "coll", "b");

        let one = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3).await.unwrap();
        let two = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3).await.unwrap();
        one.upsert(a, vec![1.0, 0.0, 0.0]).await.unwrap();
        two.upsert(b.clone(), vec![0.0, 1.0, 0.0]).await.unwrap();

        let before = s.list_blobs().await.unwrap().len();
        let report = gonzalo_core::gc::gc_blobs(&s).await.unwrap();
        let after = s.list_blobs().await.unwrap().len();

        // `GcReport.freed` is a Vec<ContentHash> of what was deleted, not a count
        // (crates/gonzalo-core/src/gc.rs:18).
        assert!(!report.freed.is_empty(), "the orphaned shard blob should be swept");
        assert!(after < before);

        // The surviving index must still be intact and queryable afterwards.
        let reopened = RecordVectorIndex::open(s, index_key(), "space-a", 3).await.unwrap();
        let hits = reopened.query(&[0.0, 1.0, 0.0], 1, &KeyPrefix::default()).await.unwrap();
        assert_eq!(hits[0].key, b);
    }

    #[tokio::test]
    async fn rewriting_identical_content_reuses_the_same_blob() {
        let dir = tmp();
        let s = fs(&dir);
        let k = RecordKey::new("ns", "coll", "a");

        let idx = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3).await.unwrap();
        idx.upsert(k.clone(), vec![1.0, 0.0, 0.0]).await.unwrap();
        let first = s.get(&index_key()).await.unwrap().unwrap();

        idx.upsert(k, vec![1.0, 0.0, 0.0]).await.unwrap();
        let second = s.get(&index_key()).await.unwrap().unwrap();

        let a = VectorManifest::from_body(&first.body).unwrap();
        let b = VectorManifest::from_body(&second.body).unwrap();
        assert_eq!(a.entries, b.entries, "identical content should hash the same");
    }
```

`FsStore` is **not** `Clone` — verified, there is no derive on it at `crates/gonzalo-store-fs/src/lib.rs:26` — which is why the helpers above hand out a fresh `fs(&dir)` handle per "process" rather than cloning one. Do not add a `Clone` derive to `FsStore` to make these tests compile.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-vector record_index`
Expected: FAIL — `upsert` returns `Err("not yet implemented")`.

- [ ] **Step 3: Write the implementation**

In `crates/gonzalo-vector/src/record_index.rs`, add to `impl<S: Store + BlobStore> RecordVectorIndex<S>`:

```rust
    /// Apply `deltas` to memory, then persist every shard they touched.
    ///
    /// On a conflict the winner's version of each touched shard is reloaded and
    /// the deltas re-applied on top, so a lost race costs a retry rather than
    /// either side's vectors. Shard blobs orphaned by a lost race are left for
    /// `gc`, exactly as the graph indexer does.
    async fn commit(&self, deltas: Vec<Delta>) -> Result<()> {
        let mut dirty = BTreeSet::new();
        for delta in &deltas {
            let key = match delta {
                Delta::Upsert(key, _) | Delta::Remove(key) => key,
            };
            dirty.insert(shard_of(key, self.shards));
        }
        self.apply(&deltas).await?;

        for attempt in 1..=MAX_COMMIT_ATTEMPTS {
            let mut blobs = BTreeMap::new();
            for &id in &dirty {
                let entries = self.inner.collect_where(|k| shard_of(k, self.shards) == id);
                let hash = self.store.put_blob(&encode_shard(self.dim, &entries)).await?;
                blobs.insert(id, hash);
            }

            let existing = self.store.get(&self.key).await?;
            let mut manifest = match &existing {
                Some(record) => VectorManifest::from_body(&record.body)?,
                None => VectorManifest::new(&self.space, self.dim, self.shards.get()),
            };
            for (id, hash) in &blobs {
                manifest.entries.insert(*id, hash.clone());
            }

            let body = manifest.to_body();
            let (record, expected) = match &existing {
                Some(current) => (
                    current.update(body, self.meta.clone()),
                    Some(current.revision.clone()),
                ),
                None => (
                    Record::create(
                        self.key.clone(),
                        RecordKind::VectorManifest,
                        body,
                        self.meta.clone(),
                    ),
                    None,
                ),
            };

            match self.store.put(record, expected).await? {
                PutResult::Committed(_) => return Ok(()),
                PutResult::Conflict(_) if attempt < MAX_COMMIT_ATTEMPTS => {
                    // Someone else committed between our read and our write.
                    // Take their version of each shard we touched, then put our
                    // own deltas back on top and try again.
                    let winner = self
                        .store
                        .get(&self.key)
                        .await?
                        .ok_or_else(|| {
                            CoreError::Backend(format!(
                                "vector index {}: manifest vanished mid-commit",
                                self.key
                            ))
                        })
                        .and_then(|r| VectorManifest::from_body(&r.body))?;
                    for &id in &dirty {
                        self.hydrate_shard(&winner, id).await?;
                    }
                    self.apply(&deltas).await?;
                }
                PutResult::Conflict(_) => {
                    return Err(CoreError::Backend(format!(
                        "vector index {}: gave up after {MAX_COMMIT_ATTEMPTS} conflicting commits",
                        self.key
                    )));
                }
            }
        }
        unreachable!("the loop returns on the final attempt")
    }

    /// Apply deltas to the in-memory index only.
    async fn apply(&self, deltas: &[Delta]) -> Result<()> {
        for delta in deltas {
            match delta {
                Delta::Upsert(key, vector) => {
                    self.inner.upsert(key.clone(), vector.clone()).await?
                }
                Delta::Remove(key) => self.inner.remove(key).await?,
            }
        }
        Ok(())
    }
```

Replace the placeholder `upsert` and `remove` in the `VectorIndex` impl:

```rust
    async fn upsert(&self, key: RecordKey, vector: Vec<f32>) -> Result<()> {
        if vector.len() != self.dim {
            return Err(CoreError::Invalid(format!(
                "vector index {}: expected dimension {}, got {}",
                self.key,
                self.dim,
                vector.len()
            )));
        }
        self.commit(vec![Delta::Upsert(key, vector)]).await
    }

    async fn remove(&self, key: &RecordKey) -> Result<()> {
        self.commit(vec![Delta::Remove(key.clone())]).await
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-vector record_index`
Expected: PASS, 10 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-vector/src/record_index.rs
git commit -m "$(printf 'feat(vector): persist upserts and removes under OCC (#323)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 7: Batched writes, and the acceptance measurement

**Files:**
- Modify: `crates/gonzalo-vector/src/record_index.rs`
- Create: `crates/gonzalo-vector/tests/durability.rs`

**Interfaces:**
- Consumes: `commit` and `Delta` from Task 6.
- Produces: an `upsert_many` override on `RecordVectorIndex`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `record_index.rs`:

```rust
    // Committing once per vector would mean one manifest commit per vector, so a
    // bulk load of 100k chunks would be 100k commits. One batch, one revision.
    #[tokio::test]
    async fn upsert_many_commits_the_manifest_once() {
        let dir = tmp();
        let s = fs(&dir);
        let idx = RecordVectorIndex::open(fs(&dir), index_key(), "space-a", 3).await.unwrap();

        idx.upsert_many(
            (0..64)
                .map(|i| (RecordKey::new("ns", "coll", i.to_string()), vec![i as f32, 0.0, 0.0]))
                .collect(),
        )
        .await
        .unwrap();

        let record = s.get(&index_key()).await.unwrap().unwrap();
        assert_eq!(record.revision.counter, 1, "one batch must be one revision");
        assert_eq!(idx.keys(&KeyPrefix::default()).await.unwrap().len(), 64);
    }
```

Create `crates/gonzalo-vector/tests/durability.rs`:

```rust
//! The ticket's acceptance criterion: an index of real size is written, dropped,
//! reopened, and queried — with the reopen cost reported rather than assumed.

use gonzalo_core::{KeyPrefix, RecordKey, VectorManifest};
use gonzalo_store_fs::FsStore;
use gonzalo_vector::{RecordVectorIndex, VectorIndex as _};
use tempfile::TempDir;

const N: usize = 10_000;
const DIM: usize = 16;

#[tokio::test]
async fn ten_thousand_vectors_survive_a_reopen() {
    let dir = TempDir::new().unwrap();
    let store = FsStore::new(dir.path());
    let key = VectorManifest::key("ns", "acceptance");

    let items: Vec<(RecordKey, Vec<f32>)> = (0..N)
        .map(|i| {
            let mut v = vec![0.0; DIM];
            v[i % DIM] = 1.0;
            (RecordKey::new("ns", "coll", i.to_string()), v)
        })
        .collect();

    let probe = items[0].1.clone();

    let idx = RecordVectorIndex::open(FsStore::new(dir.path()), key.clone(), "acceptance-space", DIM)
        .await
        .unwrap();
    idx.upsert_many(items).await.unwrap();
    drop(idx);

    let started = std::time::Instant::now();
    let reopened = RecordVectorIndex::open(store, key, "acceptance-space", DIM)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(reopened.keys(&KeyPrefix::default()).await.unwrap().len(), N);
    let hits = reopened.query(&probe, 5, &KeyPrefix::default()).await.unwrap();
    assert_eq!(hits.len(), 5);

    println!("reopened {N} vectors (dim {DIM}) in {elapsed:?}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p gonzalo-vector --all-features`
Expected: `upsert_many_commits_the_manifest_once` FAILS with `revision.counter` equal to 64, because the default `upsert_many` loops over `upsert` and commits each time.

- [ ] **Step 3: Write the implementation**

Add to the `VectorIndex` impl for `RecordVectorIndex`:

```rust
    async fn upsert_many(&self, items: Vec<(RecordKey, Vec<f32>)>) -> Result<()> {
        for (key, vector) in &items {
            if vector.len() != self.dim {
                return Err(CoreError::Invalid(format!(
                    "vector index {}: {key} has dimension {}, index is {}",
                    self.key,
                    vector.len(),
                    self.dim
                )));
            }
        }
        self.commit(
            items
                .into_iter()
                .map(|(key, vector)| Delta::Upsert(key, vector))
                .collect(),
        )
        .await
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-vector --all-features`
Expected: PASS. Note the printed reopen time from the acceptance test — record it in the PR body as a measured number.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-vector/src/record_index.rs crates/gonzalo-vector/tests/durability.rs
git commit -m "$(printf 'feat(vector): batch bulk writes into one manifest commit (#323)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 8: Chunk counts survive a restart

`KnowledgeStore.chunk_counts` exists so a re-ingest that shrinks a record drops its orphaned high-ordinal chunks (#150). It is in-memory, so it resets on restart — and a durable index makes that reachable: re-ingest a shrunk record after a restart and the orphans stay, matching queries and de-duping to a parent that no longer covers them.

**This task is severable.** It fixes a bug reachable only by callers who have an embedder configured, and #317's caller-supplied path never calls `ingest`. Cut it if the PR is getting long; do not cut it silently.

**Files:**
- Modify: `crates/gonzalo-knowledge/src/lib.rs`
- Modify: `crates/gonzalo-knowledge/Cargo.toml` if `gonzalo-store-fs`/`tempfile` are not already dev-dependencies

**Interfaces:**
- Consumes: `VectorIndex::keys` (Task 4).
- Produces: `KnowledgeStore::open(store: S, index: V, embedder: E) -> Result<Self>`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/gonzalo-knowledge/src/lib.rs`:

```rust
    // Re-ingesting a shrunk record must drop the chunks it no longer has — even
    // when the counts were not built in this process. Before `open`, a restart
    // resets them to zero and the orphans survive (#150, via #323).
    #[tokio::test]
    async fn counts_rebuilt_from_a_durable_index_still_drop_orphans() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());
        let index = Arc::new(MemoryVectorIndex::default());
        let key = RecordKey::new("ns", "coll", "doc");

        // `chunk()` maps a Topic to one chunk per bullet, so bullet count is
        // chunk count (crates/gonzalo-knowledge/src/lib.rs:250).
        let three = Topic {
            slug: "doc".into(),
            bullets: vec!["alpha".into(), "beta".into(), "gamma".into()],
        };
        put(&store, record(&key, RecordKind::Topic, three.to_body().unwrap())).await;

        // A first "process" ingests all three chunks.
        let first = KnowledgeStore::new(store.clone(), Arc::clone(&index), Bow);
        assert!(first.ingest(&key).await.unwrap());
        assert_eq!(index.keys(&KeyPrefix::default()).await.unwrap().len(), 3);
        drop(first);

        // The record shrinks to one bullet.
        let one = Topic {
            slug: "doc".into(),
            bullets: vec!["alpha".into()],
        };
        let existing = store.get(&key).await.unwrap().unwrap();
        let shrunk = existing.update(
            one.to_body().unwrap(),
            Meta::new(Identity::new("test"), "test"),
        );
        store.put(shrunk, Some(existing.revision)).await.unwrap();

        // A second "process" opens over the same durable index and re-ingests.
        // Chunks 1 and 2 must go; with `new` instead of `open` they would stay.
        let second = KnowledgeStore::open(store, Arc::clone(&index), Bow)
            .await
            .unwrap();
        assert!(second.ingest(&key).await.unwrap());

        assert_eq!(index.keys(&KeyPrefix::default()).await.unwrap().len(), 1);
    }
```

This relies on `VectorIndex` being implemented for `Arc<T>`, which Task 4 added
alongside the trait. Nothing new is needed here.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p gonzalo-knowledge counts_rebuilt_from_a_durable_index`
Expected: FAIL to compile — `KnowledgeStore::open` does not exist.

- [ ] **Step 3: Write the implementation**

Add to `impl<S: Store, V: VectorIndex, E: Embedder> KnowledgeStore<S, V, E>`:

```rust
    /// Open over an index that may already hold vectors, rebuilding the chunk
    /// counts from it.
    ///
    /// [`new`](Self::new) starts those counts empty, which is right for a fresh
    /// in-memory index and wrong for a durable one: a re-ingest that shrinks a
    /// record would not know how many chunks to remove, leaving orphans that
    /// still match queries (#150). `KeyPrefix` cannot narrow below a collection,
    /// so this scans once here rather than per ingest.
    pub async fn open(store: S, index: V, embedder: E) -> Result<Self> {
        let mut counts: std::collections::HashMap<gonzalo_core::RecordKey, usize> =
            std::collections::HashMap::new();
        for chunk in index.keys(&KeyPrefix::default()).await? {
            let (parent, ordinal) = parent_key(&chunk);
            let entry = counts.entry(parent).or_insert(0);
            *entry = (*entry).max(ordinal + 1);
        }
        Ok(Self {
            store,
            index,
            embedder,
            chunk_counts: std::sync::Mutex::new(counts),
        })
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-knowledge`
Expected: PASS, including every pre-existing test.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-knowledge/src/lib.rs crates/gonzalo-knowledge/Cargo.toml
git commit -m "$(printf 'fix(knowledge): rebuild chunk counts from a durable index (#323)\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```

---

### Task 9: ADR, guide, matrix correction, changelog

**Files:**
- Create: `docs/adr/0027-durable-vector-index.md`
- Modify: `docs/adr/README.md` (index row for 0027, back-reference on 0014)
- Modify: `docs/adr/0014-approximate-vector-index-backend.md` (note that 0027 answers its on-disk "Revisit if")
- Modify: `docs/guide/src/storage.md`
- Modify: `docs/evaluation/competitors/zep/parity-gap-matrix.md:29`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Write ADR 0027**

Follow the house format exactly — read `docs/adr/0026-grouped-facade-surface.md` first and match its header lines and section order. The ADR must stand on its own (no "see the spec for why"), and must cover:

- **Context:** the retrieval layer had no persistence and no callers outside its own unit tests; #317's caller-supplied vectors removed the last route to rebuilding an index by re-embedding.
- **Decision:** sharded blobs under one `VectorManifest` record; `VectorIndex` stays the seam, with no new persistence trait; an embedding-space tag bound at open; `MergeClass::Opaque`; 256 shards by default.
- **Consequences — positive:** recall survives a process; the GC mark set already covers the shards; an external backend (#202) plugs in at `VectorIndex` without disturbing this.
- **Consequences — negative:** one writer per index; the whole index is held in memory once opened; the space tag records what a caller *declared* and cannot verify a vector came from that model.
- **Revisit if:** indexes routinely exceed ~100k chunks; multi-writer indexes become real; a trustworthy way to bind a vector to its producing model appears.

- [ ] **Step 2: Correct the parity matrix row**

`docs/evaluation/competitors/zep/parity-gap-matrix.md:29` currently reads ✅ for "Incremental updates (no full recompute)", justified as *"Writes are per-record; index layers (vector/graph) update incrementally, never rebuild the store (ADR 0008, 0012)"*. That was true of the graph and not of vector, where no durable index existed to update. Rewrite the justification to describe what is true after this change, citing ADR 0027 alongside 0008 and 0012.

- [ ] **Step 3: Document it in the guide**

Add a section to `docs/guide/src/storage.md` covering: what a vector manifest is, that opening declares a space and dimension and why a mismatch is an error, the one-writer-per-index rule, and that `gonzalo gc` treats shards as live. Match the surrounding prose style — read the neighbouring sections first.

- [ ] **Step 4: Update the changelog**

Add entries under the unreleased heading for: the `VectorManifest` record kind, `RecordVectorIndex`, the two `VectorIndex` trait methods (noting `keys` is a required method and therefore a breaking change for any out-of-tree implementation), and `KnowledgeStore::open` if Task 8 shipped.

- [ ] **Step 5: Validate the ADR set**

Run the `adr-validate` skill against `docs/adr`. Apply its mechanical fixes (status parity, supersession links, sequence, broken refs). If it reports a non-mechanical inconsistency, stop and surface it rather than papering over it.

- [ ] **Step 6: Run the full gate**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features
```

Expected: all green. Fix and re-run the whole gate on any failure — not just the step that failed.

- [ ] **Step 7: Commit**

```bash
git add docs CHANGELOG.md
git commit -m "$(printf 'docs(vector): ADR 0027 and guide for the durable vector index (#323)\n\nAlso corrects the zep parity matrix row that claimed incremental index\nupdates for vector, where no durable index existed.\n\nClaude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu')"
```
