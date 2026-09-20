//! gonzalo — a robust, shareable persistence layer for caliban.
//!
//! This facade re-exports the curated surface most consumers need and
//! selects storage substrates via Cargo features (`fs` is on by default).
//!
//! Everything is one [`Record`] — a body plus provenance at a [`RecordKey`] —
//! behind one [`Store`] trait, so the substrate is configuration rather than
//! API. The examples below use [`FsStore`]; a daemon-backed `ServerStore`, git
//! or S3 store behaves the same way.
//!
//! # Store and read a record
//!
//! ```
//! use gonzalo::{Body, FsStore, Identity, Meta, PutResult, Record, RecordKey, RecordKind, Store};
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let dir = tempfile::tempdir().unwrap();
//! let store = FsStore::new(dir.path());
//!
//! let key = RecordKey::new("caliban", "topics", "rust");
//! let meta = Meta::new(Identity::new("ada"), "example");
//! let record = Record::create(
//!     key.clone(),
//!     RecordKind::Topic,
//!     Body::Inline(b"ownership\n".to_vec()),
//!     meta,
//! );
//!
//! // `None` means "this key should not exist yet".
//! let PutResult::Committed(revision) = store.put(record, None).await? else {
//!     panic!("the key was already taken");
//! };
//!
//! let stored = store.get(&key).await?.expect("just written");
//! assert_eq!(stored.revision, revision);
//! assert_eq!(stored.body, Body::Inline(b"ownership\n".to_vec()));
//! # Ok::<(), gonzalo::CoreError>(())
//! # }).unwrap();
//! ```
//!
//! # Update without losing a concurrent write
//!
//! A write names the revision it expects to replace. If someone else got there
//! first, the store returns [`PutResult::Conflict`] carrying their record
//! instead of overwriting it — concurrent edits are never silently lost
//! (ADR 0005). Re-read, re-apply, retry.
//!
//! ```
//! use gonzalo::{Body, FsStore, Identity, Meta, PutResult, Record, RecordKey, RecordKind, Store};
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! # let dir = tempfile::tempdir().unwrap();
//! # let store = FsStore::new(dir.path());
//! # let key = RecordKey::new("caliban", "topics", "rust");
//! # let meta = Meta::new(Identity::new("ada"), "example");
//! # let first = Record::create(key.clone(), RecordKind::Topic, Body::Inline(b"v0\n".to_vec()), meta);
//! # store.put(first, None).await?;
//! let current = store.get(&key).await?.expect("stored above");
//! let meta = Meta::new(Identity::new("ada"), "example");
//! let next = current.update(Body::Inline(b"v1\n".to_vec()), meta);
//!
//! match store.put(next, Some(current.revision.clone())).await? {
//!     PutResult::Committed(revision) => println!("now at {revision:?}"),
//!     PutResult::Conflict(c) => {
//!         // Someone wrote between our read and our write; `c.current` is theirs.
//!         println!("stale: the store holds {:?}", c.current.revision);
//!     }
//! }
//!
//! // Writing again from the same stale read conflicts rather than clobbering.
//! let meta = Meta::new(Identity::new("ada"), "example");
//! let stale = current.update(Body::Inline(b"v1-again\n".to_vec()), meta);
//! assert!(matches!(
//!     store.put(stale, Some(current.revision)).await?,
//!     PutResult::Conflict(_)
//! ));
//! # Ok::<(), gonzalo::CoreError>(())
//! # }).unwrap();
//! ```
//!
//! # Deletes replicate
//!
//! A delete writes a tombstone, so it survives replication: syncing with a peer
//! that still holds the record does not bring it back (ADR 0021). Consumer
//! reads hide tombstones; `get_raw` shows them.
//!
//! ```
//! use gonzalo::{Body, FsStore, Identity, Meta, Record, RecordKey, RecordKind, Store, sync};
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
//! let (a, b) = (FsStore::new(dir_a.path()), FsStore::new(dir_b.path()));
//!
//! let key = RecordKey::new("caliban", "topics", "rust");
//! let meta = Meta::new(Identity::new("ada"), "example");
//! let record = Record::create(key.clone(), RecordKind::Topic, Body::Inline(b"v0\n".to_vec()), meta);
//! let revision = match a.put(record, None).await? {
//!     gonzalo::PutResult::Committed(r) => r,
//!     gonzalo::PutResult::Conflict(_) => unreachable!("fresh store"),
//! };
//!
//! sync(&a, &b).await?;                       // B has it
//! a.delete(&key, Some(revision)).await?;     // deleted on A
//! let report = sync(&a, &b).await?;          // the delete travels
//! assert!(report.conflicts.is_empty() && report.converged());
//!
//! assert!(b.get(&key).await?.is_none(), "gone for consumers");
//! assert!(b.get_raw(&key).await?.unwrap().is_tombstone(), "a tombstone remains");
//! # Ok::<(), gonzalo::CoreError>(())
//! # }).unwrap();
//! ```
//!
//! # Typed views
//!
//! Domain types map to and from a record body with [`RecordCodec`], so a
//! consumer works with its own structs rather than bytes.
//!
//! ```
//! use gonzalo::{FsStore, Identity, Meta, Record, RecordCodec, RecordKey, Store, Topic};
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! # let dir = tempfile::tempdir().unwrap();
//! # let store = FsStore::new(dir.path());
//! let topic = Topic {
//!     slug: "rust".into(),
//!     bullets: vec!["ownership".into(), "borrowing".into()],
//! };
//! let key = RecordKey::new("caliban", "topics", "rust");
//! let meta = Meta::new(Identity::new("ada"), "example");
//! store
//!     .put(Record::create(key.clone(), Topic::KIND, topic.to_body()?, meta), None)
//!     .await?;
//!
//! let stored = store.get(&key).await?.expect("just written");
//! assert_eq!(Topic::from_body(&stored.body)?, topic);
//! # Ok::<(), gonzalo::CoreError>(())
//! # }).unwrap();
//! ```
//!
//! The guide covers the rest: [deletion, reset and collection][del], the
//! [daemon][daemon] and the [fleet records][fleet].
//!
//! [del]: https://caliban-ai.github.io/gonzalo/deletion.html
//! [daemon]: https://caliban-ai.github.io/gonzalo/daemon.html
//! [fleet]: https://caliban-ai.github.io/gonzalo/fleet.html

pub use gonzalo_core::{
    AncestryStore, BlobStore, Body, CollectReport, Conflict, ContentHash, CoreError, DeleteResult,
    GcReport, Identity, KeyPrefix, MergeClass, MergeOutcome, Meta, PutResult, Record, RecordKey,
    RecordKind, ResetReport, Result, Revision, Store, SyncConflict, SyncReport, collect, gc_blobs,
    merge, now_ms, reset, reset_as, sync, sync_with_ancestry,
};

pub use gonzalo_domain::{
    Actor, ActorRole, AuditEntry, AuditResult, Authenticator, BindingOrigin, BodyFormat,
    ChannelConfig, Checkpoint, Consumption, Container, EmptyFollowSet, FleetActor, FleetKeyError,
    FleetRole, Follows, GrantScope, IdentityBinding, Link, LinkKind, LinkSecret, LinkSecretError,
    LinkTarget, LinkToken, MemoryTier, NotifyPreset, Person, Priority, PriorityLevel, Provider,
    RecordCodec, RedeemError, Resolution, RoleGrant, Session, State, StateCategory, Ticket,
    TicketBody, TicketEvent, Topic, Turn, VerifiedEmail, fleet,
};

#[cfg(feature = "fs")]
pub use gonzalo_store_fs::FsStore;

#[cfg(feature = "git")]
pub use gonzalo_store_git::GitStore;

#[cfg(feature = "s3")]
pub use gonzalo_store_s3::S3Store;

#[cfg(feature = "remote")]
pub use gonzalo_store_server::ServerStore;

#[cfg(feature = "vector")]
pub use gonzalo_vector::{Embedder, Match, MemoryVectorIndex, VectorIndex};

#[cfg(feature = "graph")]
pub use gonzalo_graph::{
    CodeGraph, GraphStore, InMemoryGraphStore, Located, Reference, Symbol, SymbolKind, assemble,
    build_rust,
};

#[cfg(feature = "ticket")]
pub use gonzalo_ticket::{
    Capabilities, Cursor, FieldMapping, InMemorySource, Page, SourceError, StateMapping,
    StateSignal, TicketSource, record_key, scoped_uid,
};

#[cfg(feature = "ticket-github")]
pub use gonzalo_ticket_github::GitHubSource;

#[cfg(feature = "ticket-jira")]
pub use gonzalo_ticket_jira::JiraSource;

#[cfg(feature = "ticket-linear")]
pub use gonzalo_ticket_linear::LinearSource;

#[cfg(feature = "ticket-gitlab")]
pub use gonzalo_ticket_gitlab::GitLabSource;

#[cfg(feature = "ticket-asana")]
pub use gonzalo_ticket_asana::AsanaSource;

#[cfg(feature = "knowledge")]
pub use gonzalo_knowledge::{Hit, KnowledgeStore, knowledge_text};

/// Compile-checked: the facade crate has no doctests, and no other test
/// touches replicated deletion, so a re-export dropped from the `pub use
/// gonzalo_core::{...}` block above (`ResetReport`, `CollectReport`,
/// `reset`, `reset_as`, `collect`) would otherwise go unnoticed until a
/// downstream consumer's build broke.
#[cfg(test)]
mod facade_reexports {
    use super::*;

    #[test]
    fn reset_and_collect_are_re_exported() {
        let reset_report = ResetReport::default();
        let collect_report = CollectReport::default();
        assert!(reset_report.deleted.is_empty());
        assert!(collect_report.purged.is_empty());

        let _reset_fn = reset;
        let _reset_as_fn = reset_as;
        let _collect_fn = collect;
    }

    #[test]
    fn blob_gc_is_re_exported() {
        // Reclaiming a deleted record's bytes needs `gc_blobs` as much as it
        // needs `collect` — a tombstone pins its blob until it is collected
        // (gonzalo#292), so a consumer reaching for one reaches for both.
        let report = GcReport::default();
        assert!(report.freed.is_empty());
        assert_eq!(report.retained, 0);
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn fleet_records_are_re_exported() {
        let person = Person {
            display_name: "Ada".into(),
            email: None,
        };
        assert_eq!(Person::KIND, RecordKind::Person);
        assert!(person.to_body().is_ok());
        assert_eq!(fleet::FLEET_NAMESPACE, "fleet");
        assert_eq!(fleet::FLEET_AUDIT_NAMESPACE, "fleet-audit");

        let secret = LinkSecret::from_bytes([0; 32]);
        let token = LinkToken::new(
            &secret,
            FleetRole::Viewer,
            GrantScope::Fleet,
            None,
            FleetActor::Service("test".into()),
            0,
            1,
        );
        assert_eq!(token.key(), LinkToken::key_for_secret(&secret));
        let _: Option<(
            AuditEntry,
            AuditResult,
            Authenticator,
            BindingOrigin,
            ChannelConfig,
            Consumption,
            FleetKeyError,
            IdentityBinding,
            LinkSecretError,
            RedeemError,
            RoleGrant,
            VerifiedEmail,
        )> = None;
    }
}

#[cfg(all(test, feature = "fs"))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn end_to_end_put_get_via_facade() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());

        let topic = Topic {
            slug: "rust".into(),
            bullets: vec!["use clippy".into()],
        };
        let body = topic.to_body().unwrap();
        let key = RecordKey::new("caliban", "topics", "rust");
        let rec = Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind: Topic::KIND,
            meta: Meta {
                author: Identity::new("john"),
                origin_system: "laptop".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            key: key.clone(),
            ancestors: Vec::new(),
            deleted_at: None,
            deleted_blob: None,
        };

        assert!(matches!(
            store.put(rec, None).await.unwrap(),
            PutResult::Committed(_)
        ));
        let got = store.get(&key).await.unwrap().unwrap();
        assert_eq!(Topic::from_body(&got.body).unwrap(), topic);
    }
}
