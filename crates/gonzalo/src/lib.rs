//! gonzalo — a robust, shareable persistence layer for caliban.
//!
//! This facade re-exports the curated surface most consumers need and
//! selects storage substrates via Cargo features (`fs` is on by default).

pub use gonzalo_core::{
    Body, Conflict, ContentHash, CoreError, Identity, KeyPrefix, MergeClass, MergeOutcome, Meta,
    PutResult, Record, RecordKey, RecordKind, Result, Revision, Store, SyncConflict, SyncReport,
    merge, sync,
};

pub use gonzalo_domain::{
    Actor, ActorRole, BodyFormat, Checkpoint, Container, Link, LinkKind, LinkTarget, MemoryTier,
    Priority, PriorityLevel, Provider, RecordCodec, Resolution, Session, State, StateCategory,
    Ticket, TicketBody, TicketEvent, Topic, Turn,
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
    CodeGraph, GraphStore, InMemoryGraphStore, Reference, Symbol, SymbolKind, build_rust,
};

#[cfg(feature = "ticket")]
pub use gonzalo_ticket::{
    Capabilities, Cursor, FieldMapping, InMemorySource, Page, SourceError, StateMapping,
    StateSignal, TicketSource, record_key,
};

#[cfg(feature = "ticket-github")]
pub use gonzalo_ticket_github::GitHubSource;

#[cfg(feature = "ticket-jira")]
pub use gonzalo_ticket_jira::JiraSource;

#[cfg(feature = "ticket-linear")]
pub use gonzalo_ticket_linear::LinearSource;

#[cfg(feature = "ticket-gitlab")]
pub use gonzalo_ticket_gitlab::GitLabSource;

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
        };

        assert!(matches!(
            store.put(rec, None).await.unwrap(),
            PutResult::Committed(_)
        ));
        let got = store.get(&key).await.unwrap().unwrap();
        assert_eq!(Topic::from_body(&got.body).unwrap(), topic);
    }
}
