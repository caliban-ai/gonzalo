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
    /// A human with fleet roles. Not ADR 0015's `Principal`, which is a gonzalo
    /// bearer token. See ADR 0022.
    Person,
    /// Binds one external account (chat user id, OIDC subject) to a `Person`.
    /// See ADR 0022.
    IdentityBinding,
    /// A person's fleet role, fleet-wide or for one workspace. See ADR 0022,
    /// amended by ADR 0023.
    RoleGrant,
    /// A chat channel's configuration: what it follows, a notify preset, and
    /// a role ceiling. See ADR 0022, amended by ADR 0023.
    ChannelConfig,
    /// A one-time, expiring token that links an account to a person. Stores only
    /// the token's hash, and is marked consumed rather than deleted. See ADR 0022.
    LinkToken,
    /// A write-once record of who did what, to what, and with what result.
    /// See ADR 0022.
    AuditEntry,
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
            RecordKind::MemoryTier
            | RecordKind::Ticket
            | RecordKind::Person
            | RecordKind::IdentityBinding
            | RecordKind::RoleGrant
            | RecordKind::ChannelConfig => MergeClass::Structured,
            RecordKind::Checkpoint => MergeClass::Opaque,
            // Both diverge only through a double redemption or an audit-key
            // collision, which must surface rather than merge (ADR 0022).
            RecordKind::LinkToken | RecordKind::AuditEntry => MergeClass::Opaque,
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

impl Meta {
    /// Provenance for a write by `author` from `origin_system`, with no labels.
    ///
    /// `created` and `updated` are left at `0`: no store populates them yet
    /// (gonzalo#293). Set the fields directly for labels or timestamps.
    pub fn new(author: Identity, origin_system: impl Into<String>) -> Self {
        Self {
            author,
            origin_system: origin_system.into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        }
    }
}

impl Record {
    /// A record that does not exist yet: the body's first revision, no parent
    /// and no history. Write it with `put(record, None)` (gonzalo#305).
    ///
    /// ```
    /// use gonzalo_core::{Body, Identity, Meta, Record, RecordKey, RecordKind, Revision};
    ///
    /// let key = RecordKey::new("caliban", "topics", "rust");
    /// let meta = Meta::new(Identity::new("ada"), "example");
    /// let v0 = Record::create(key, RecordKind::Topic, Body::Inline(b"first".to_vec()), meta);
    /// assert_eq!(v0.revision, Revision::initial(b"first"));
    /// assert_eq!(v0.parent, None);
    ///
    /// // The next version descends from it: `put(v1, Some(v0.revision))` commits.
    /// let meta = Meta::new(Identity::new("ada"), "example");
    /// let v1 = v0.update(Body::Inline(b"second".to_vec()), meta);
    /// assert_eq!(v1.revision, v0.revision.next(b"second"));
    /// assert_eq!(v1.parent, Some(v0.revision));
    /// ```
    pub fn create(key: RecordKey, kind: RecordKind, body: Body, meta: Meta) -> Self {
        Self {
            revision: Revision::initial(body.bytes()),
            key,
            kind,
            parent: None,
            body,
            meta,
            links: Vec::new(),
            ancestors: Vec::new(),
            deleted_at: None,
        }
    }

    /// The next version of this record carrying `body`, written by `meta`.
    ///
    /// Its revision follows this one and its `parent` is this revision, so
    /// `put(next, Some(current.revision))` commits it and a stale read
    /// conflicts — which is the pairing that is easy to get wrong by hand
    /// (gonzalo#305). `key`, `kind` and `links` carry over.
    ///
    /// `ancestors` is left empty: the store folds this record's revision and
    /// history in when it commits, bounded by its ancestor cap. `deleted_at`
    /// is cleared, since only a store writes a tombstone.
    pub fn update(&self, body: Body, meta: Meta) -> Self {
        Self {
            key: self.key.clone(),
            kind: self.kind,
            revision: self.revision.next(body.bytes()),
            parent: Some(self.revision.clone()),
            body,
            meta,
            links: self.links.clone(),
            ancestors: Vec::new(),
            deleted_at: None,
        }
    }

    /// Whether this record is a deletion marker.
    pub fn is_tombstone(&self) -> bool {
        self.kind == RecordKind::Tombstone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- builders (#305) ----

    fn meta_by(author: &str) -> Meta {
        Meta::new(Identity::new(author), "test")
    }

    #[test]
    fn meta_new_carries_author_and_origin_and_nothing_else() {
        let m = meta_by("ada");
        assert_eq!(m.author, Identity::new("ada"));
        assert_eq!(m.origin_system, "test");
        assert_eq!((m.created, m.updated), (0, 0));
        assert!(m.labels.is_empty());
    }

    #[test]
    fn create_is_a_first_revision_with_no_history() {
        let key = RecordKey::new("ns", "col", "id");
        let body = Body::Inline(b"hello".to_vec());
        let r = Record::create(key.clone(), RecordKind::Topic, body.clone(), meta_by("ada"));

        assert_eq!(r.key, key);
        assert_eq!(r.kind, RecordKind::Topic);
        assert_eq!(r.revision, Revision::initial(b"hello"));
        assert_eq!(r.parent, None);
        assert_eq!(r.body, body);
        assert!(r.links.is_empty() && r.ancestors.is_empty());
        assert_eq!(r.deleted_at, None);
    }

    #[test]
    fn update_follows_the_revision_it_was_built_from() {
        let key = RecordKey::new("ns", "col", "id");
        let mut v0 = Record::create(
            key.clone(),
            RecordKind::Topic,
            Body::Inline(b"v0".to_vec()),
            meta_by("ada"),
        );
        v0.links = vec![RecordKey::new("ns", "col", "other")];

        let v1 = v0.update(Body::Inline(b"v1".to_vec()), meta_by("grace"));

        assert_eq!(v1.revision, v0.revision.next(b"v1"));
        assert_eq!(v1.parent, Some(v0.revision.clone()));
        assert_eq!(v1.body, Body::Inline(b"v1".to_vec()));
        assert_eq!(
            v1.meta.author,
            Identity::new("grace"),
            "the updater writes it"
        );
        assert_eq!((v1.key, v1.kind), (key, RecordKind::Topic));
        assert_eq!(v1.links, v0.links, "links carry over");
        assert!(
            v1.ancestors.is_empty(),
            "the store folds history in on commit"
        );
    }

    #[tokio::test]
    async fn built_records_commit_through_occ() {
        use crate::{PutResult, Store, memstore::MemStore};

        let store = MemStore::new();
        let key = RecordKey::new("ns", "col", "id");
        let v0 = Record::create(
            key.clone(),
            RecordKind::Topic,
            Body::Inline(b"v0".to_vec()),
            meta_by("ada"),
        );
        assert!(matches!(
            store.put(v0.clone(), None).await.unwrap(),
            PutResult::Committed(_)
        ));

        // Update what was read back, the way a consumer does.
        let current = store.get(&key).await.unwrap().unwrap();
        let v1 = current.update(Body::Inline(b"v1".to_vec()), meta_by("ada"));
        assert!(matches!(
            store.put(v1, Some(current.revision.clone())).await.unwrap(),
            PutResult::Committed(_)
        ));

        // A second update built from the now-stale read conflicts, as OCC should.
        let stale = current.update(Body::Inline(b"v1b".to_vec()), meta_by("ada"));
        assert!(matches!(
            store
                .put(stale, Some(current.revision.clone()))
                .await
                .unwrap(),
            PutResult::Conflict(_)
        ));
    }

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
        assert_eq!(RecordKind::Person.merge_class(), MergeClass::Structured);
        assert_eq!(
            RecordKind::IdentityBinding.merge_class(),
            MergeClass::Structured
        );
        assert_eq!(RecordKind::RoleGrant.merge_class(), MergeClass::Structured);
        assert_eq!(
            RecordKind::ChannelConfig.merge_class(),
            MergeClass::Structured
        );
        assert_eq!(RecordKind::LinkToken.merge_class(), MergeClass::Opaque);
        assert_eq!(RecordKind::AuditEntry.merge_class(), MergeClass::Opaque);
    }

    #[test]
    fn fleet_kinds_serialize_as_their_names() {
        for (kind, name) in [
            (RecordKind::Person, "\"Person\""),
            (RecordKind::IdentityBinding, "\"IdentityBinding\""),
            (RecordKind::RoleGrant, "\"RoleGrant\""),
            (RecordKind::ChannelConfig, "\"ChannelConfig\""),
            (RecordKind::LinkToken, "\"LinkToken\""),
            (RecordKind::AuditEntry, "\"AuditEntry\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), name);
            let back: RecordKind = serde_json::from_str(name).unwrap();
            assert_eq!(back, kind);
        }
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
