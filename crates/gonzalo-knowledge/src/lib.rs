//! Knowledge-store capability for gonzalo.
//!
//! A single "what do we know about X" surface that composes the existing
//! capability layers (ADR 0011): a [`Store`] for records, a [`VectorIndex`] for
//! semantic retrieval, and an [`Embedder`] for turning text into vectors — all
//! addressed by the shared [`RecordKey`]. Queries return first-class
//! [`Record`]s as [`Hit`]s, not bare ids.
//!
//! Which record kinds are knowledge-bearing, and how their text is extracted,
//! lives in [`knowledge_text`]. Phase 1 embeds one document per record;
//! per-kind chunking and a `graph`-backed join are future refinements behind
//! this same surface.

use gonzalo_core::{KeyPrefix, Record, RecordKind, Result, Store};
use gonzalo_domain::{MemoryTier, RecordCodec, Session, Ticket, TicketEvent, Topic};
use gonzalo_vector::{Embedder, VectorIndex};

/// One search hit: a first-class record and its similarity score (higher is
/// more similar).
#[derive(Debug, Clone)]
pub struct Hit {
    pub record: Record,
    pub score: f32,
}

/// Composes a [`Store`], a [`VectorIndex`], and an [`Embedder`] into one
/// retrieval surface keyed by [`RecordKey`](gonzalo_core::RecordKey).
pub struct KnowledgeStore<S, V, E> {
    store: S,
    index: V,
    embedder: E,
}

impl<S: Store, V: VectorIndex, E: Embedder> KnowledgeStore<S, V, E> {
    pub fn new(store: S, index: V, embedder: E) -> Self {
        Self {
            store,
            index,
            embedder,
        }
    }

    /// Borrow the underlying store (e.g. to put records before ingesting them).
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Ingest the record at `key`: extract its knowledge text, embed it, and
    /// index it under the same key. Returns `false` (without indexing) if the
    /// record is absent or its kind is not knowledge-bearing.
    pub async fn ingest(&self, key: &gonzalo_core::RecordKey) -> Result<bool> {
        let Some(record) = self.store.get(key).await? else {
            return Ok(false);
        };
        let Some(text) = knowledge_text(&record) else {
            return Ok(false);
        };
        let vector = self.embedder.embed(&text).await?;
        self.index.upsert(key.clone(), vector).await?;
        Ok(true)
    }

    /// Semantic query: embed `text`, take the top-`k` keys (restricted to
    /// `filter`), and resolve them to first-class records.
    pub async fn query(&self, text: &str, k: usize, filter: &KeyPrefix) -> Result<Vec<Hit>> {
        let query_vec = self.embedder.embed(text).await?;
        let matches = self.index.query(&query_vec, k, filter).await?;
        let mut hits = Vec::with_capacity(matches.len());
        for m in matches {
            if let Some(record) = self.store.get(&m.key).await? {
                hits.push(Hit {
                    record,
                    score: m.score,
                });
            }
        }
        Ok(hits)
    }
}

/// The embeddable text for a record, or `None` if its kind is not
/// knowledge-bearing (ADR 0011). Extraction goes through the `gonzalo-domain`
/// typed views.
pub fn knowledge_text(record: &Record) -> Option<String> {
    match record.kind {
        RecordKind::MemoryTier => MemoryTier::from_body(&record.body)
            .ok()
            .map(|t| format!("{}\n{}", t.name, t.content)),
        RecordKind::Topic => Topic::from_body(&record.body)
            .ok()
            .map(|t| format!("{}\n{}", t.slug, t.bullets.join("\n"))),
        RecordKind::Session => Session::from_body(&record.body).ok().map(|s| {
            let turns = s
                .turns
                .iter()
                .map(|turn| turn.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            format!("{}\n{}", s.name, turns)
        }),
        RecordKind::Ticket => Ticket::from_body(&record.body)
            .ok()
            .map(|t| format!("{}\n{}\n{}", t.title, t.body.markdown, t.labels.join(" "))),
        RecordKind::TicketEvent => TicketEvent::from_body(&record.body).ok().map(|e| e.body),
        RecordKind::Checkpoint => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use gonzalo_core::{Body, Identity, Meta, PutResult, RecordKey, Revision};
    use gonzalo_domain::{BodyFormat, Provider, State, StateCategory, TicketBody};
    use gonzalo_store_fs::FsStore;
    use gonzalo_vector::MemoryVectorIndex;
    use std::collections::BTreeMap;

    /// A deterministic bag-of-words embedder: cosine similarity tracks word
    /// overlap, enough to rank a matching record first.
    struct Bow;
    #[async_trait]
    impl Embedder for Bow {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let mut v = vec![0f32; 32];
            for word in text.split_whitespace() {
                let h = word.bytes().map(|b| b as usize).sum::<usize>() % 32;
                v[h] += 1.0;
            }
            Ok(v)
        }
    }

    fn record(key: &RecordKey, kind: RecordKind, body: Body) -> Record {
        Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind,
            meta: Meta {
                author: Identity::new("t"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            key: key.clone(),
        }
    }

    #[test]
    fn knowledge_text_per_kind() {
        let topic = Topic {
            slug: "rust".into(),
            bullets: vec!["use clippy".into()],
        };
        let key = RecordKey::new("caliban", "topics", "rust");
        let text = knowledge_text(&record(&key, RecordKind::Topic, topic.to_body().unwrap()));
        assert_eq!(text.as_deref(), Some("rust\nuse clippy"));

        // Checkpoint is not knowledge-bearing.
        let ck = record(&key, RecordKind::Checkpoint, Body::Inline(b"{}".to_vec()));
        assert_eq!(knowledge_text(&ck), None);
    }

    #[test]
    fn ticket_text_includes_title_body_labels() {
        let t = Ticket {
            provider: Provider::GitHub,
            uid: "o/r#1".into(),
            display: "#1".into(),
            item_type: "issue".into(),
            title: "fix the parser".into(),
            state: State {
                category: StateCategory::Open,
                resolution: None,
                raw_name: "open".into(),
                raw_id: None,
            },
            priority: None,
            actors: vec![],
            labels: vec!["bug".into()],
            containers: vec![],
            links: vec![],
            body: TicketBody {
                markdown: "the parser panics".into(),
                format: BodyFormat::Markdown,
                raw: None,
            },
            fields: BTreeMap::new(),
        };
        let key = RecordKey::new("tickets", "github", "o/r#1");
        let text = knowledge_text(&record(&key, RecordKind::Ticket, t.to_body().unwrap())).unwrap();
        assert!(text.contains("fix the parser"));
        assert!(text.contains("the parser panics"));
        assert!(text.contains("bug"));
    }

    async fn put(store: &FsStore, rec: Record) {
        assert!(matches!(
            store.put(rec, None).await.unwrap(),
            PutResult::Committed(_)
        ));
    }

    #[tokio::test]
    async fn ingest_then_query_returns_matching_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());

        let rust = RecordKey::new("caliban", "topics", "rust");
        let cooking = RecordKey::new("caliban", "topics", "cooking");
        put(
            &store,
            record(
                &rust,
                RecordKind::Topic,
                Topic {
                    slug: "rust".into(),
                    bullets: vec!["use clippy and cargo".into()],
                }
                .to_body()
                .unwrap(),
            ),
        )
        .await;
        put(
            &store,
            record(
                &cooking,
                RecordKind::Topic,
                Topic {
                    slug: "cooking".into(),
                    bullets: vec!["simmer the sauce slowly".into()],
                }
                .to_body()
                .unwrap(),
            ),
        )
        .await;

        let ks = KnowledgeStore::new(store, MemoryVectorIndex::default(), Bow);
        assert!(ks.ingest(&rust).await.unwrap());
        assert!(ks.ingest(&cooking).await.unwrap());

        let hits = ks
            .query("clippy cargo", 1, &KeyPrefix::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.key, rust);
    }

    #[tokio::test]
    async fn ingest_skips_non_knowledge_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());
        let key = RecordKey::new("caliban", "checkpoints", "c1");
        put(
            &store,
            record(&key, RecordKind::Checkpoint, Body::Inline(b"{}".to_vec())),
        )
        .await;
        let ks = KnowledgeStore::new(store, MemoryVectorIndex::default(), Bow);
        assert!(!ks.ingest(&key).await.unwrap(), "checkpoint is not indexed");
    }
}
