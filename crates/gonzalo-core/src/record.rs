//! The universal persisted unit and its classification.

use crate::{ContentHash, Identity, RecordKey, Revision};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a record represents. Drives the merge strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordKind {
    MemoryTier,
    Topic,
    Session,
    Checkpoint,
    /// A tracked work item imported from an external ticket platform.
    Ticket,
    /// An append-only comment/event on a ticket.
    TicketEvent,
    /// A per-view code-graph manifest: `(repo, view_id) -> { path -> content_hash }`.
    /// Regenerable from source; reconciled last-writer-wins. See ADR 0012.
    GraphManifest,
    /// A deletion marker. Hidden from consumer reads (`get`/`list`); replicated
    /// by sync and pull through raw reads; physically removed only by
    /// `Store::purge`. See ADR 0021.
    Tombstone,
}

/// How concurrent edits to a record of a given kind are reconciled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeClass {
    /// Edits union/concatenate (auto-memory topics, session transcripts).
    AppendOnly,
    /// Field-level 3-way merge against the common base.
    Structured,
    /// No safe automatic merge; surface to the caller.
    Opaque,
    /// Regenerable / don't-merge (e.g. per-view code-graph manifests, ADR 0012).
    /// The body can be re-derived from source, and views are single-writer, so a
    /// divergence is rare and reconciled deterministically in favor of side A
    /// (the `ours` argument to `merge`, which has no `Meta` to compare) rather
    /// than a content merge — never a surfaced conflict.
    Derived,
}

impl RecordKind {
    pub fn merge_class(self) -> MergeClass {
        match self {
            RecordKind::Topic | RecordKind::Session | RecordKind::TicketEvent => {
                MergeClass::AppendOnly
            }
            RecordKind::MemoryTier | RecordKind::Ticket => MergeClass::Structured,
            RecordKind::Checkpoint => MergeClass::Opaque,
            RecordKind::GraphManifest => MergeClass::Derived,
            // Sync and pull reconcile tombstones before any body merge runs;
            // the most conservative class guards a path that forgets to.
            RecordKind::Tombstone => MergeClass::Opaque,
        }
    }
}

/// A record body. `Inline` stores bytes directly in the record; `Blob`
/// references content held out-of-line in a content-addressed [`BlobStore`],
/// so byte-identical bodies (e.g. code-graph slices shared across worktrees)
/// are stored once. See ADR 0012.
///
/// [`BlobStore`]: crate::store::BlobStore
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Body {
    Inline(Vec<u8>),
    /// Content stored out-of-line under `hash` in a [`BlobStore`]; `len` is the
    /// referenced content's byte length. The record itself carries only the
    /// reference — the bytes are fetched via `BlobStore::get_blob`.
    ///
    /// [`BlobStore`]: crate::store::BlobStore
    Blob {
        hash: ContentHash,
        len: u64,
    },
}

impl Body {
    /// Build a blob body referencing `content` by its content hash. The content
    /// itself is written separately via `BlobStore::put_blob`.
    pub fn blob(content: &[u8]) -> Self {
        Body::Blob {
            hash: ContentHash::of(content),
            len: content.len() as u64,
        }
    }

    /// The bytes used for content hashing and merging. For a `Blob` these are
    /// the reference's hash bytes, not the referenced content — identical
    /// content yields an identical reference, so the record's revision is
    /// stable under content-addressed dedup.
    pub fn bytes(&self) -> &[u8] {
        match self {
            Body::Inline(b) => b,
            Body::Blob { hash, .. } => hash.0.as_bytes(),
        }
    }
}

/// Provenance and labels for a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub author: Identity,
    pub origin_system: String,
    pub created: i64,
    pub updated: i64,
    pub labels: BTreeMap<String, String>,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_class_is_assigned_per_kind() {
        assert_eq!(RecordKind::Topic.merge_class(), MergeClass::AppendOnly);
        assert_eq!(RecordKind::Session.merge_class(), MergeClass::AppendOnly);
        assert_eq!(RecordKind::MemoryTier.merge_class(), MergeClass::Structured);
        assert_eq!(RecordKind::Checkpoint.merge_class(), MergeClass::Opaque);
        assert_eq!(RecordKind::Ticket.merge_class(), MergeClass::Structured);
        assert_eq!(
            RecordKind::TicketEvent.merge_class(),
            MergeClass::AppendOnly
        );
        assert_eq!(RecordKind::GraphManifest.merge_class(), MergeClass::Derived);
    }

    #[test]
    fn body_exposes_bytes() {
        assert_eq!(Body::Inline(b"hi".to_vec()).bytes(), b"hi");
    }

    #[test]
    fn blob_body_references_content_by_hash() {
        let body = Body::blob(b"fn main() {}");
        match &body {
            Body::Blob { hash, len } => {
                assert_eq!(*hash, crate::ContentHash::of(b"fn main() {}"));
                assert_eq!(*len, 12);
            }
            _ => panic!("expected Body::Blob"),
        }
    }

    #[test]
    fn blob_body_bytes_are_stable_per_content() {
        // Identical content -> identical body bytes -> identical revision (the
        // record-level face of content-addressed dedup).
        assert_eq!(Body::blob(b"same").bytes(), Body::blob(b"same").bytes());
        assert_ne!(Body::blob(b"same").bytes(), Body::blob(b"diff").bytes());
    }

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
}
