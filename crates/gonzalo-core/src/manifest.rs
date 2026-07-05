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

    /// The collection segment every manifest is addressed under. A
    /// [`KeyPrefix`](crate::KeyPrefix) with this collection and no namespace
    /// lists every view's manifest across all repos — the set GC must union to
    /// mark live slices.
    pub fn collection() -> &'static str {
        MANIFEST_COLLECTION
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

    /// Reconcile this manifest against the `desired` `path -> content_hash` set
    /// of a working tree, returning the change sets and the reconciled manifest.
    ///
    /// The A/M/D classification is a pure **set difference**, so it is robust to
    /// missed events — a full reconcile always converges the manifest onto the
    /// tree regardless of how the desired set was sourced (a `git diff` stream is
    /// only an optimization for building it). Unchanged paths (present in both
    /// with an equal hash) appear in none of the change sets. Each set is sorted
    /// for deterministic output, and the reconciled manifest equals `desired`.
    pub fn reconcile(&self, desired: &BTreeMap<String, ContentHash>) -> Reconciliation {
        let mut added = Vec::new();
        let mut modified = Vec::new();
        for (path, hash) in desired {
            match self.entries.get(path) {
                None => added.push(path.clone()),
                Some(current) if current != hash => modified.push(path.clone()),
                Some(_) => {} // unchanged
            }
        }
        let deleted = self
            .entries
            .keys()
            .filter(|path| !desired.contains_key(*path))
            .cloned()
            .collect();
        // BTreeMap iteration is already key-sorted, so `added`/`modified`/
        // `deleted` come out sorted without an explicit sort.
        Reconciliation {
            added,
            modified,
            deleted,
            manifest: Manifest {
                entries: desired.clone(),
            },
        }
    }
}

/// Build the desired `path -> content_hash` set for a working tree by hashing
/// each file's content. The output feeds [`Manifest::reconcile`].
pub fn desired_set<P, C>(entries: impl IntoIterator<Item = (P, C)>) -> BTreeMap<String, ContentHash>
where
    P: Into<String>,
    C: AsRef<[u8]>,
{
    entries
        .into_iter()
        .map(|(path, content)| (path.into(), ContentHash::of(content.as_ref())))
        .collect()
}

/// The result of reconciling a [`Manifest`] against a working tree: the change
/// sets (each sorted) plus the reconciled manifest, which equals the tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reconciliation {
    /// Paths present in the tree but not the old manifest.
    pub added: Vec<String>,
    /// Paths in both whose content hash changed.
    pub modified: Vec<String>,
    /// Paths in the old manifest but no longer in the tree.
    pub deleted: Vec<String>,
    /// The manifest after reconciliation (equal to the desired tree set).
    pub manifest: Manifest,
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

    #[test]
    fn desired_set_hashes_each_tree_entry() {
        let desired = desired_set([("a.rs", "one"), ("b.rs", "two")]);
        assert_eq!(desired.get("a.rs"), Some(&hash("one")));
        assert_eq!(desired.get("b.rs"), Some(&hash("two")));
        assert_eq!(desired.len(), 2);
    }

    #[test]
    fn reconcile_classifies_added_modified_deleted() {
        let mut current = Manifest::new();
        current.insert("keep.rs", hash("same")); // unchanged
        current.insert("edit.rs", hash("old")); // modified
        current.insert("gone.rs", hash("bye")); // deleted

        let desired = desired_set([("keep.rs", "same"), ("edit.rs", "new"), ("add.rs", "fresh")]);

        let r = current.reconcile(&desired);
        assert_eq!(r.added, vec!["add.rs".to_string()]);
        assert_eq!(r.modified, vec!["edit.rs".to_string()]);
        assert_eq!(r.deleted, vec!["gone.rs".to_string()]);
    }

    #[test]
    fn reconciled_manifest_equals_the_desired_tree() {
        let mut current = Manifest::new();
        current.insert("gone.rs", hash("bye"));
        let desired = desired_set([("add.rs", "fresh")]);

        let r = current.reconcile(&desired);
        assert_eq!(r.manifest.entries, desired);
    }

    #[test]
    fn reconcile_reports_nothing_when_tree_matches_manifest() {
        let mut current = Manifest::new();
        current.insert("a.rs", hash("x"));
        current.insert("b.rs", hash("y"));
        let desired = desired_set([("a.rs", "x"), ("b.rs", "y")]);

        let r = current.reconcile(&desired);
        assert!(r.added.is_empty());
        assert!(r.modified.is_empty());
        assert!(r.deleted.is_empty());
        assert_eq!(r.manifest.entries, current.entries);
    }

    #[test]
    fn reconcile_from_empty_marks_all_added() {
        let desired = desired_set([("b.rs", "2"), ("a.rs", "1")]);
        let r = Manifest::new().reconcile(&desired);
        // Sorted, deterministic.
        assert_eq!(r.added, vec!["a.rs".to_string(), "b.rs".to_string()]);
        assert!(r.modified.is_empty());
        assert!(r.deleted.is_empty());
    }

    #[test]
    fn reconcile_to_empty_marks_all_deleted() {
        let mut current = Manifest::new();
        current.insert("b.rs", hash("2"));
        current.insert("a.rs", hash("1"));

        let r = current.reconcile(&BTreeMap::new());
        assert_eq!(r.deleted, vec!["a.rs".to_string(), "b.rs".to_string()]);
        assert!(r.added.is_empty());
        assert!(r.modified.is_empty());
        assert!(r.manifest.entries.is_empty());
    }
}
