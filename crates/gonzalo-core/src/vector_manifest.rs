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
        assert_eq!(RecordKind::VectorManifest.merge_class(), MergeClass::Opaque);
    }
}
