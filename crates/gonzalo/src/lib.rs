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
//! Domain types map to and from a record body with [`codec::RecordCodec`], so
//! a consumer works with its own structs rather than bytes. Each lives under
//! the module naming its domain — `memory`, `session`, `ticket`, `fleet` — so
//! nothing here occupies a name a consumer might want (ADR 0026).
//!
//! ```
//! use gonzalo::{FsStore, Identity, Meta, Record, RecordKey, Store};
//! use gonzalo::{codec::RecordCodec, memory::Topic};
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

// The root holds the record and store core: what every consumer touches
// whatever else they use, and the part of the surface that was never ambiguous
// (ADR 0026). Everything that is a *domain noun* lives under the module naming
// its domain, mirroring the structure `gonzalo-domain` already has.
pub use gonzalo_core::{
    AncestryStore, BlobStore, Body, CollectReport, Conflict, ContentHash, CoreError, DeleteResult,
    GcReport, Identity, KeyPrefix, MergeClass, MergeOutcome, Meta, PutResult, Record, RecordKey,
    RecordKind, ResetReport, Result, Revision, Store, SyncConflict, SyncReport, collect, gc_blobs,
    merge, now_ms, reset, reset_as, sync, sync_with_ancestry,
};

#[cfg(feature = "fs")]
pub use gonzalo_store_fs::FsStore;

#[cfg(feature = "git")]
pub use gonzalo_store_git::GitStore;

#[cfg(feature = "s3")]
pub use gonzalo_store_s3::S3Store;

#[cfg(feature = "remote")]
pub use gonzalo_store_server::ServerStore;

/// Memory-tier records and the topics they summarize.
pub mod memory {
    pub use gonzalo_domain::memory::{MemoryTier, Topic};
}

/// Conversation sessions and their turns.
pub mod session {
    pub use gonzalo_domain::session::{Session, Turn};
}

/// Point-in-time checkpoints over a session.
pub mod checkpoint {
    pub use gonzalo_domain::checkpoint::Checkpoint;
}

/// The trait tying a typed view to the record body that stores it.
pub mod codec {
    pub use gonzalo_domain::codec::RecordCodec;
}

/// Fleet access-control records: people, identity bindings, role grants,
/// channel configuration, link tokens and the audit trail (ADR 0022, 0023).
pub mod fleet {
    pub use gonzalo_domain::fleet::*;
}

/// Tickets: the typed records, and — with the `ticket` feature — the source
/// layer and its connectors that import a board into them.
pub mod ticket {
    pub use gonzalo_domain::ticket::{
        Actor, ActorRole, BodyFormat, Container, Link, LinkKind, LinkTarget, Priority,
        PriorityLevel, Provider, Resolution, State, StateCategory, Ticket, TicketBody, TicketEvent,
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
}

/// Vector search: the `Embedder` trait, the indexes over it, and the durable
/// index's manifest. `VectorManifest` lives in `gonzalo-core` rather than
/// `gonzalo-vector` (ADR 0008's layering: `gonzalo gc`'s mark-set builder must
/// parse it, and core cannot depend on a capability layer), but it is a
/// vector-layer noun, so it is re-exported here rather than at the root — the
/// same way the code-graph `Manifest` is not at the root either (ADR 0026).
#[cfg(feature = "vector")]
pub mod vector {
    pub use gonzalo_core::VectorManifest;
    pub use gonzalo_vector::{
        DEFAULT_SHARDS, Embedder, Match, MemoryVectorIndex, RecordVectorIndex, VectorIndex,
        shard_of,
    };
}

/// The tree-sitter code graph.
///
/// `Page` lives here as well as in [`ticket`] — two layers using one word,
/// which the previous flat surface could not express (ADR 0026).
#[cfg(feature = "graph")]
pub mod graph {
    pub use gonzalo_graph::{
        CodeGraph, GraphStore, InMemoryGraphStore, Located, Reference, Symbol, SymbolKind,
        assemble, build_rust,
    };
}

/// Records plus vector search by [`RecordKey`], resolving hits back to whole
/// records (ADR 0011).
#[cfg(feature = "knowledge")]
pub mod knowledge {
    pub use gonzalo_knowledge::{Hit, KnowledgeStore, knowledge_text};
}

/// Compile-checked: a re-export dropped from the blocks above would otherwise
/// go unnoticed until a downstream consumer's build broke. Since ADR 0026 the
/// cases also pin *where* each name lives — the grouping is the public contract
/// now, so a name quietly promoted back to the root is as much a regression as
/// one that disappeared.
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
        use crate::codec::RecordCodec;
        use crate::fleet::{
            AuditEntry, AuditResult, Authenticator, BindingOrigin, ChannelConfig, Consumption,
            FleetActor, FleetKeyError, FleetRole, GrantScope, IdentityBinding, LinkSecret,
            LinkSecretError, LinkToken, Person, RedeemError, RoleGrant, VerifiedEmail,
        };

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

    #[test]
    #[allow(clippy::type_complexity)]
    fn domain_nouns_live_under_their_module() {
        // The grouping itself, asserted by path (ADR 0026). Every name here was
        // at the root before 0.8.0, and each module keeps its own.
        let _: Option<(memory::MemoryTier, memory::Topic)> = None;
        let _: Option<(session::Session, session::Turn)> = None;
        let _: Option<checkpoint::Checkpoint> = None;
        let _: Option<(
            ticket::Ticket,
            ticket::TicketBody,
            ticket::TicketEvent,
            ticket::State,
            ticket::StateCategory,
            ticket::Actor,
            ticket::ActorRole,
            ticket::Priority,
            ticket::PriorityLevel,
            ticket::Resolution,
            ticket::Provider,
            ticket::Container,
            ticket::Link,
            ticket::LinkKind,
            ticket::LinkTarget,
            ticket::BodyFormat,
        )> = None;
        // `RecordCodec` is the seam every typed view implements, so it is the
        // one that would be most tempting to promote back to the root.
        fn _codec_is_a_trait<T: codec::RecordCodec>() {}
    }

    /// The capability layers, each behind its own feature.
    #[test]
    #[cfg(feature = "graph")]
    fn graph_lives_under_its_module() {
        let _: Option<(graph::CodeGraph, graph::Symbol, graph::SymbolKind)> = None;
        let _ = graph::build_rust;
    }

    #[test]
    #[cfg(feature = "vector")]
    fn vector_lives_under_its_module() {
        let _: Option<(vector::Match, vector::MemoryVectorIndex)> = None;
        let _ = vector::DEFAULT_SHARDS;
        let _ = vector::shard_of;
        let _: Option<vector::VectorManifest> = None;
    }

    // `RecordVectorIndex` needs a concrete `Store` to name, which only exists
    // with `fs` enabled (the default). Split out so `vector` alone (were it
    // ever built without `fs`) still exercises the rest of this module's shape.
    #[test]
    #[cfg(all(feature = "vector", feature = "fs"))]
    fn vector_durable_index_lives_under_its_module() {
        // A facade user with the `vector` feature must be able to reach the
        // durable index (gonzalo#323 fix-round), not just the in-memory one.
        let _: Option<vector::RecordVectorIndex<crate::FsStore>> = None;
    }

    #[test]
    #[cfg(feature = "knowledge")]
    fn knowledge_lives_under_its_module() {
        let _: Option<knowledge::Hit> = None;
        let _ = knowledge::knowledge_text;
    }

    #[test]
    #[cfg(feature = "ticket")]
    #[allow(clippy::type_complexity)]
    fn the_ticket_source_layer_joins_the_ticket_module() {
        // The typed records and the connectors that import them are one domain
        // even though they are three crates, so they share one module.
        let _: Option<(
            ticket::Capabilities,
            ticket::Cursor,
            ticket::FieldMapping,
            ticket::InMemorySource,
            ticket::Page,
            ticket::SourceError,
            ticket::StateMapping,
            ticket::StateSignal,
        )> = None;
        let _ = ticket::record_key;
        let _ = ticket::scoped_uid;
    }
}

#[cfg(all(test, feature = "fs"))]
mod tests {
    use super::*;
    use crate::codec::RecordCodec;
    use crate::memory::Topic;
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
