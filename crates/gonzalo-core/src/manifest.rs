//! Per-view code-graph manifests: the identity layer over content-addressed
//! slices (ADR 0012).
//!
//! A manifest maps every path in a view to the [`ContentHash`] of the slice
//! that currently populates it: `(repo, view_id) -> { path -> content_hash }`.
//! Slices are content-addressed and shared across worktrees; the manifest is
//! what gives a *view* its identity and lets assembly resolve `path -> slice`.
//! It is regenerable from source, so a divergence is reconciled last-writer-wins
//! ([`MergeClass::Derived`]) rather than surfaced as a conflict.
//!
//! [`MergeClass::Derived`]: crate::MergeClass::Derived

use crate::{Body, ContentHash, CoreError, RecordKey, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The collection segment under which every view's manifest is addressed.
const MANIFEST_COLLECTION: &str = "graph-manifest";

/// A per-view manifest body: path -> the content hash of the populating slice.
///
/// Backed by a [`BTreeMap`] so serialization has deterministic key order — a
/// manifest with the same entries always hashes identically, which keeps its
/// record revision stable under content-addressed dedup.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: BTreeMap<String, ContentHash>,
}

impl Manifest {
    /// An empty manifest.
    pub fn new() -> Self {
        Self::default()
    }

    /// The stable [`RecordKey`] addressing the manifest for `(repo, view_id)`.
    /// A view has exactly one manifest, so this is a pure function of the pair.
    pub fn key(repo: impl Into<String>, view_id: impl Into<String>) -> RecordKey {
        RecordKey::new(repo, MANIFEST_COLLECTION, view_id)
    }

    /// Record that `path` is populated by the slice with content hash `hash`.
    pub fn insert(&mut self, path: impl Into<String>, hash: ContentHash) {
        self.entries.insert(path.into(), hash);
    }

    /// The content hash of the slice populating `path`, if the view has one.
    pub fn get(&self, path: &str) -> Option<&ContentHash> {
        self.entries.get(path)
    }

    /// Serialize into an inline record [`Body`] (deterministic key order).
    pub fn to_body(&self) -> Body {
        Body::Inline(
            serde_json::to_vec(&self.entries).expect("BTreeMap<String, ContentHash> serializes"),
        )
    }

    /// Reconstruct a manifest from a record [`Body`]. Errors if the body bytes
    /// are not a valid serialized manifest.
    pub fn from_body(body: &Body) -> Result<Self> {
        let entries =
            serde_json::from_slice(body.bytes()).map_err(|e| CoreError::Serde(e.to_string()))?;
        Ok(Self { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RecordKind;

    fn hash(s: &str) -> ContentHash {
        ContentHash::of(s.as_bytes())
    }

    #[test]
    fn key_addresses_repo_and_view_as_namespace_and_id() {
        let k = Manifest::key("acme/widgets", "main");
        assert_eq!(k.namespace, "acme/widgets");
        assert_eq!(k.collection, MANIFEST_COLLECTION);
        assert_eq!(k.id, "main");
    }

    #[test]
    fn key_is_stable_for_the_same_repo_and_view() {
        assert_eq!(Manifest::key("r", "v"), Manifest::key("r", "v"));
        assert_ne!(Manifest::key("r", "v"), Manifest::key("r", "w"));
        assert_ne!(Manifest::key("r", "v"), Manifest::key("s", "v"));
    }

    #[test]
    fn insert_and_get_resolve_path_to_slice_hash() {
        let mut m = Manifest::new();
        m.insert("src/lib.rs", hash("slice-a"));
        assert_eq!(m.get("src/lib.rs"), Some(&hash("slice-a")));
        assert_eq!(m.get("src/absent.rs"), None);
    }

    #[test]
    fn body_round_trips() {
        let mut m = Manifest::new();
        m.insert("src/main.rs", hash("s1"));
        m.insert("src/lib.rs", hash("s2"));

        let restored = Manifest::from_body(&m.to_body()).unwrap();
        assert_eq!(restored, m);
    }

    #[test]
    fn body_bytes_are_deterministic_regardless_of_insert_order() {
        // BTreeMap key order -> the same entries always serialize identically,
        // so two independently-built manifests with equal contents share one
        // revision under content-addressed dedup.
        let mut a = Manifest::new();
        a.insert("b.rs", hash("x"));
        a.insert("a.rs", hash("y"));

        let mut b = Manifest::new();
        b.insert("a.rs", hash("y"));
        b.insert("b.rs", hash("x"));

        assert_eq!(a.to_body().bytes(), b.to_body().bytes());
    }

    #[test]
    fn from_body_rejects_non_manifest_bytes() {
        let garbage = Body::Inline(b"not json at all".to_vec());
        assert!(matches!(
            Manifest::from_body(&garbage),
            Err(CoreError::Serde(_))
        ));
    }

    #[test]
    fn manifest_kind_is_derived() {
        assert_eq!(
            RecordKind::GraphManifest.merge_class(),
            crate::MergeClass::Derived
        );
    }
}
