//! The ingest engine: pull a [`TicketSource`] and persist each [`Ticket`] as a
//! `Record`, using optimistic concurrency (ADR 0005). Re-sync is idempotent —
//! unchanged tickets (same body hash) are skipped, so a full board re-scan is
//! cheap. Depends only on the trait + `Store`, never on a concrete connector.

use crate::{TicketSource, record_key};
use gonzalo_core::{ContentHash, Identity, Meta, PutResult, Record, Revision, Store};
use gonzalo_domain::{RecordCodec, Ticket};
use std::collections::BTreeMap;

/// How many tickets a sync created, updated, or left untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IngestSummary {
    pub imported: usize,
    pub updated: usize,
    pub unchanged: usize,
}

/// Failures during ingest.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("ticket source: {0}")]
    Source(#[from] crate::SourceError),
    #[error("store: {0}")]
    Store(#[from] gonzalo_core::CoreError),
    #[error("write conflict on {0}")]
    Conflict(String),
}

/// Pull all changed tickets from `source` and upsert them into `store`,
/// attributing writes to `author`.
pub async fn ingest(
    source: &dyn TicketSource,
    store: &dyn Store,
    author: &str,
) -> Result<IngestSummary, IngestError> {
    let mut summary = IngestSummary::default();
    let mut cursor = crate::Cursor::default();
    loop {
        let page = source.fetch_changed(&cursor).await?;
        for ticket in &page.tickets {
            match upsert(store, ticket, author).await? {
                Outcome::Imported => summary.imported += 1,
                Outcome::Updated => summary.updated += 1,
                Outcome::Unchanged => summary.unchanged += 1,
            }
        }
        if page.next.0.is_none() || page.next == cursor {
            break;
        }
        cursor = page.next;
    }
    Ok(summary)
}

enum Outcome {
    Imported,
    Updated,
    Unchanged,
}

async fn upsert(store: &dyn Store, ticket: &Ticket, author: &str) -> Result<Outcome, IngestError> {
    let key = record_key(ticket);
    let body = ticket.to_body()?;
    let new_hash = ContentHash::of(body.bytes());

    let existing = store.get(&key).await?;
    if let Some(rec) = &existing
        && rec.revision.hash == new_hash
    {
        return Ok(Outcome::Unchanged);
    }

    let expected: Option<Revision> = existing.as_ref().map(|r| r.revision.clone());
    let revision = match &expected {
        Some(prev) => prev.next(body.bytes()),
        None => Revision::initial(body.bytes()),
    };
    let record = Record {
        key: key.clone(),
        kind: Ticket::KIND,
        revision,
        parent: expected.clone(),
        body,
        meta: Meta {
            author: Identity::new(author),
            origin_system: "ticket-ingest".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: vec![],
    };
    match store.put(record, expected).await? {
        PutResult::Committed(_) => Ok(if existing.is_some() {
            Outcome::Updated
        } else {
            Outcome::Imported
        }),
        PutResult::Conflict(_) => Err(IngestError::Conflict(format!("{key}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemorySource;
    use gonzalo_domain::{BodyFormat, Provider, State, StateCategory, TicketBody};
    use gonzalo_store_fs::FsStore;

    fn ticket(uid: &str, title: &str) -> Ticket {
        Ticket {
            provider: Provider::GitHub,
            uid: uid.into(),
            display: "#1".into(),
            item_type: "issue".into(),
            title: title.into(),
            state: State {
                category: StateCategory::Open,
                resolution: None,
                raw_name: "Todo".into(),
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

    #[tokio::test]
    async fn imports_then_is_idempotent_then_updates() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());

        let src = InMemorySource::new(vec![ticket("a", "A"), ticket("b", "B")]);
        let s1 = ingest(&src, &store, "tester").await.unwrap();
        assert_eq!(
            s1,
            IngestSummary {
                imported: 2,
                updated: 0,
                unchanged: 0
            }
        );

        let s2 = ingest(&src, &store, "tester").await.unwrap();
        assert_eq!(
            s2,
            IngestSummary {
                imported: 0,
                updated: 0,
                unchanged: 2
            }
        );

        let src2 = InMemorySource::new(vec![ticket("a", "A2"), ticket("b", "B")]);
        let s3 = ingest(&src2, &store, "tester").await.unwrap();
        assert_eq!(
            s3,
            IngestSummary {
                imported: 0,
                updated: 1,
                unchanged: 1
            }
        );
    }
}
