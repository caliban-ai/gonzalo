//! gonzalo — a robust, shareable persistence layer for caliban.
//!
//! This facade re-exports the curated surface most consumers need and
//! selects storage substrates via Cargo features (`fs` is on by default).

pub use gonzalo_core::{
    AncestryStore, BlobStore, Body, CollectReport, Conflict, ContentHash, CoreError, DeleteResult,
    Identity, KeyPrefix, MergeClass, MergeOutcome, Meta, PutResult, Record, RecordKey, RecordKind,
    ResetReport, Result, Revision, Store, SyncConflict, SyncReport, collect, merge, now_ms, reset,
    reset_as, sync, sync_with_ancestry,
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
        };

        assert!(matches!(
            store.put(rec, None).await.unwrap(),
            PutResult::Committed(_)
        ));
        let got = store.get(&key).await.unwrap().unwrap();
        assert_eq!(Topic::from_body(&got.body).unwrap(), topic);
    }
}
