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
    #[error("write conflict on {key}: expected {expected:?}, store has {current:?}")]
    Conflict {
        key: gonzalo_core::RecordKey,
        expected: Option<Revision>,
        current: Revision,
    },
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

/// Per-ticket result of an upsert, tallied into [`IngestSummary`].
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
        PutResult::Conflict(c) => Err(IngestError::Conflict {
            key: c.key,
            expected: c.expected,
            current: c.current.revision,
        }),
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

    /// A source that returns two pages, driving the pagination loop across the
    /// cursor boundary. Stateless: it branches on the incoming cursor value.
    struct PagedSource;

    #[async_trait::async_trait]
    impl crate::TicketSource for PagedSource {
        fn capabilities(&self) -> crate::Capabilities {
            crate::Capabilities::default()
        }

        async fn fetch_changed(&self, cursor: &crate::Cursor) -> crate::Result<crate::Page> {
            match cursor.0.as_deref() {
                None => Ok(crate::Page {
                    tickets: vec![ticket("p1", "P1")],
                    next: crate::Cursor(Some("p2".into())),
                }),
                Some("p2") => Ok(crate::Page {
                    tickets: vec![ticket("p2", "P2")],
                    next: crate::Cursor::default(),
                }),
                Some(other) => Err(crate::SourceError::Backend(format!(
                    "unexpected cursor: {other}"
                ))),
            }
        }

        async fn get(&self, uid: &str) -> crate::Result<Ticket> {
            Ok(ticket(uid, uid))
        }
    }

    #[tokio::test]
    async fn paginates_across_multiple_pages() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());

        let summary = ingest(&PagedSource, &store, "tester").await.unwrap();
        assert_eq!(summary.imported, 2);
    }

    /// A store whose `put` always conflicts, to exercise the conflict arm.
    struct ConflictStore;

    #[async_trait::async_trait]
    impl gonzalo_core::Store for ConflictStore {
        async fn get(
            &self,
            _key: &gonzalo_core::RecordKey,
        ) -> gonzalo_core::Result<Option<Record>> {
            Ok(None)
        }

        async fn put(
            &self,
            record: Record,
            expected: Option<Revision>,
        ) -> gonzalo_core::Result<PutResult> {
            Ok(PutResult::Conflict(Box::new(gonzalo_core::Conflict {
                key: record.key.clone(),
                expected,
                current: record,
            })))
        }

        async fn list(
            &self,
            _prefix: &gonzalo_core::KeyPrefix,
        ) -> gonzalo_core::Result<Vec<gonzalo_core::RecordKey>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn surfaces_store_conflict() {
        let src = InMemorySource::new(vec![ticket("a", "A")]);
        let err = ingest(&src, &ConflictStore, "tester").await.unwrap_err();
        assert!(matches!(err, IngestError::Conflict { .. }));
    }
}
