//! Ticket / work-item capability layer for gonzalo.
//!
//! A typed [`Ticket`](gonzalo_domain::Ticket) is persisted as a `Record`
//! (defined in `gonzalo-domain`); this crate adds the *capability* surface over
//! it (ADR 0010): the [`TicketSource`] provider boundary, a per-connection
//! field/state [`mapping`] policy, capability negotiation, and a `RecordKey`
//! convention so tickets compose with the vector and graph layers by shared key.
//!
//! Phase 1 is read-only import; write-back is capability-gated. Concrete
//! provider connectors (GitHub, Jira, Linear, …) are separate, feature-gated
//! crates built on this surface.

pub mod conformance;
pub mod ingest;
pub mod mapping;
pub mod mock;
pub mod source;

pub use ingest::{IngestError, IngestSummary, ingest};
pub use mapping::{FieldMapping, StateMapping, StateSignal};
pub use mock::InMemorySource;
pub use source::{Capabilities, Cursor, Page, Result, SourceError, TicketSource};

use gonzalo_core::RecordKey;
use gonzalo_domain::{Provider, Ticket};

/// The stable `RecordKey` for a ticket: `tickets / <provider> / <uid>`.
///
/// Keying off a deterministic `RecordKey` is what lets ticket queries join the
/// vector and graph layers (ADR 0008/0010) — they all address the same record.
pub fn record_key(ticket: &Ticket) -> RecordKey {
    RecordKey::new("tickets", provider_slug(&ticket.provider), &ticket.uid)
}

fn provider_slug(provider: &Provider) -> String {
    match provider {
        Provider::GitHub => "github".into(),
        Provider::Jira => "jira".into(),
        Provider::Linear => "linear".into(),
        Provider::GitLab => "gitlab".into(),
        Provider::Asana => "asana".into(),
        Provider::AzureDevOps => "azure-devops".into(),
        Provider::Bugzilla => "bugzilla".into(),
        Provider::Other(name) => name.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_domain::{BodyFormat, State, StateCategory, TicketBody};
    use std::collections::BTreeMap;

    fn ticket(uid: &str) -> Ticket {
        Ticket {
            provider: Provider::GitHub,
            uid: uid.into(),
            display: "#1".into(),
            item_type: "issue".into(),
            title: "t".into(),
            state: State {
                category: StateCategory::Open,
                resolution: None,
                raw_name: "open".into(),
                raw_id: None,
            },
            priority: None,
            actors: vec![],
            labels: vec![],
            containers: vec![],
            links: vec![],
            body: TicketBody {
                markdown: String::new(),
                format: BodyFormat::Markdown,
                raw: None,
            },
            fields: BTreeMap::new(),
        }
    }

    #[test]
    fn record_key_follows_tickets_provider_uid_convention() {
        let k = record_key(&ticket("caliban-ai/gonzalo#15"));
        assert_eq!(k.namespace, "tickets");
        assert_eq!(k.collection, "github");
        assert_eq!(k.id, "caliban-ai/gonzalo#15");
    }

    #[tokio::test]
    async fn in_memory_source_fetches_and_gets() {
        let src = InMemorySource::new(vec![ticket("a"), ticket("b")]);
        let page = src.fetch_changed(&Cursor::default()).await.unwrap();
        assert_eq!(page.tickets.len(), 2);
        assert_eq!(src.get("b").await.unwrap().uid, "b");
        assert!(src.get("missing").await.is_err());
    }

    #[tokio::test]
    async fn read_only_source_rejects_writes() {
        let src = InMemorySource::new(vec![ticket("a")]);
        assert!(!src.capabilities().push);
        let err = src.set_state("a", StateCategory::Done).await.unwrap_err();
        assert!(matches!(err, SourceError::Unsupported("set_state")));
    }
}
