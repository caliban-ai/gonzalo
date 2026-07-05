//! Knowledge-store capability for gonzalo.
//!
//! A single "what do we know about X" surface that composes the existing
//! capability layers (ADR 0011): a [`Store`] for records, a [`VectorIndex`] for
//! semantic retrieval, and an [`Embedder`] for turning text into vectors — all
//! addressed by the shared [`RecordKey`](gonzalo_core::RecordKey). Queries return first-class
//! [`Record`]s as [`Hit`]s, not bare ids.
//!
//! Which record kinds are knowledge-bearing, and how their text is split into
//! embeddable pieces, lives in [`chunk`] (with [`knowledge_text`] as the joined
//! one-document view). Records are embedded at chunk granularity — a `Session`
//! by turn, a long `MemoryTier`/`Ticket` by paragraph, a `Topic` by bullet — so
//! a query matching one turn/section isn't drowned out by a document average;
//! [`KnowledgeStore::query`] de-dups chunk matches back to one hit per record.
//! A `graph`-backed join ([`KnowledgeStore::query_in_subgraph`]) is a further
//! refinement behind this same surface.

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

/// One chunk-granular search hit: the parent [`Record`], the matching chunk's
/// score, its `ordinal` within the record, and that chunk's `text`. Unlike a
/// [`Hit`], chunk hits are **not** de-duped to one-per-record.
#[derive(Debug, Clone)]
pub struct ChunkHit {
    pub record: Record,
    pub score: f32,
    pub ordinal: usize,
    pub text: String,
}

/// Composes a [`Store`], a [`VectorIndex`], and an [`Embedder`] into one
/// retrieval surface keyed by [`RecordKey`](gonzalo_core::RecordKey).
pub struct KnowledgeStore<S, V, E> {
    store: S,
    index: V,
    embedder: E,
    /// Last-seen chunk count per record, so a re-ingest that shrinks a record
    /// can remove the now-orphaned high-ordinal chunks from the index. Held
    /// in-memory, matching the (currently in-memory) index's lifecycle — see the
    /// design note in the module docs.
    chunk_counts: std::sync::Mutex<std::collections::HashMap<gonzalo_core::RecordKey, usize>>,
}

impl<S: Store, V: VectorIndex, E: Embedder> KnowledgeStore<S, V, E> {
    pub fn new(store: S, index: V, embedder: E) -> Self {
        Self {
            store,
            index,
            embedder,
            chunk_counts: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Borrow the underlying store (e.g. to put records before ingesting them).
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Ingest the record at `key`: split it into per-kind [`chunk`]s, embed each,
    /// and index it under a derived per-chunk key. Returns `false` (without
    /// indexing) if the record is absent or its kind is not knowledge-bearing.
    pub async fn ingest(&self, key: &gonzalo_core::RecordKey) -> Result<bool> {
        let Some(record) = self.store.get(key).await? else {
            return Ok(false);
        };
        let Some(chunks) = chunk(&record) else {
            return Ok(false);
        };

        // Remove chunks orphaned by a shrink since the last ingest. Read the old
        // count under a scoped lock — never held across an `.await`.
        let old = {
            let counts = self.chunk_counts.lock().unwrap();
            counts.get(key).copied().unwrap_or(0)
        };
        for ordinal in chunks.len()..old {
            self.index.remove(&chunk_key(key, ordinal)).await?;
        }

        for (ordinal, text) in chunks.iter().enumerate() {
            let vector = self.embedder.embed(text).await?;
            self.index.upsert(chunk_key(key, ordinal), vector).await?;
        }

        self.chunk_counts
            .lock()
            .unwrap()
            .insert(key.clone(), chunks.len());
        Ok(true)
    }

    /// Semantic query, one hit per record. Embed `text`, over-fetch chunk
    /// matches (restricted to `filter`), collapse them to their parent records
    /// keeping each record's best chunk score, and return the top-`k` records.
    ///
    /// Because a record with many matching chunks can crowd the over-fetch
    /// window, pathological cases may return fewer than `k` hits.
    pub async fn query(&self, text: &str, k: usize, filter: &KeyPrefix) -> Result<Vec<Hit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let query_vec = self.embedder.embed(text).await?;
        let fetch = k.saturating_mul(OVERFETCH);
        let matches = self.index.query(&query_vec, fetch, filter).await?;

        // Collapse chunk matches to parents, keeping the best score per parent.
        let mut best: std::collections::BTreeMap<gonzalo_core::RecordKey, f32> =
            std::collections::BTreeMap::new();
        for m in matches {
            let (parent, _ordinal) = parent_key(&m.key);
            best.entry(parent)
                .and_modify(|s| *s = s.max(m.score))
                .or_insert(m.score);
        }

        // Rank parents by best chunk score, resolve the top-k to records.
        let mut ranked: Vec<(gonzalo_core::RecordKey, f32)> = best.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(k);

        let mut hits = Vec::with_capacity(ranked.len());
        for (parent, score) in ranked {
            if let Some(record) = self.store.get(&parent).await? {
                hits.push(Hit { record, score });
            }
        }
        Ok(hits)
    }

    /// Chunk-granular query: the top-`k` matching chunks (restricted to
    /// `filter`), **not** de-duped to their parents. Each hit carries the parent
    /// record, the chunk's ordinal, and the chunk's text.
    pub async fn query_chunks(
        &self,
        text: &str,
        k: usize,
        filter: &KeyPrefix,
    ) -> Result<Vec<ChunkHit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let query_vec = self.embedder.embed(text).await?;
        let matches = self.index.query(&query_vec, k, filter).await?;
        let mut hits = Vec::with_capacity(matches.len());
        for m in matches {
            let (parent, ordinal) = parent_key(&m.key);
            if let Some(record) = self.store.get(&parent).await? {
                let text = chunk(&record)
                    .and_then(|cs| cs.into_iter().nth(ordinal))
                    .unwrap_or_default();
                hits.push(ChunkHit {
                    record,
                    score: m.score,
                    ordinal,
                    text,
                });
            }
        }
        Ok(hits)
    }

    /// **Vector⋈graph** (ADR 0011): rank the top-`k` candidates by semantic
    /// similarity, then keep only those whose record falls in `root`'s
    /// call-graph neighborhood in `graph` — `root` itself, its callers, and its
    /// callees. Composition is by shared key: a record is in the neighborhood
    /// when its [`RecordKey`](gonzalo_core::RecordKey)`.id` equals a symbol name
    /// in the neighborhood (no new cross-store linkage, ADR 0008/0011).
    ///
    /// So "semantically similar to X **and** structurally near `root`" is one
    /// call. The result is the in-neighborhood subset of the top-`k` (≤ `k`).
    #[cfg(feature = "graph")]
    pub async fn query_in_subgraph(
        &self,
        text: &str,
        k: usize,
        graph: &dyn gonzalo_graph::GraphStore,
        root: &str,
    ) -> Result<Vec<Hit>> {
        use std::collections::BTreeSet;
        let mut neighborhood: BTreeSet<String> = BTreeSet::from([root.to_string()]);
        neighborhood.extend(graph.callers_of(root));
        neighborhood.extend(graph.callees(root));

        let hits = self.query(text, k, &KeyPrefix::default()).await?;
        Ok(hits
            .into_iter()
            .filter(|h| neighborhood.contains(&h.record.key.id))
            .collect())
    }
}

/// The embeddable text for a record, or `None` if its kind is not
/// knowledge-bearing (ADR 0011). Kept as the one-document view: the [`chunk`]s
/// joined back into a single string.
pub fn knowledge_text(record: &Record) -> Option<String> {
    chunk(record).map(|cs| cs.join("\n"))
}

/// Split a record into ordered chunk texts, or `None` if its kind is not
/// knowledge-bearing (ADR 0011). Chunking is per-kind: a `Session` by turn, a
/// long `MemoryTier`/`Ticket` by section/paragraph, a `Topic` by bullet. The
/// kind's title/name is folded into the first chunk so it stays searchable. A
/// kind that yields a single piece is the one-document fallback — identical to
/// phase-1 behavior. Extraction goes through the `gonzalo-domain` typed views.
pub fn chunk(record: &Record) -> Option<Vec<String>> {
    match record.kind {
        RecordKind::MemoryTier => MemoryTier::from_body(&record.body)
            .ok()
            .map(|t| prepend_header(&t.name, split_paragraphs(&t.content))),
        RecordKind::Topic => Topic::from_body(&record.body)
            .ok()
            .map(|t| prepend_header(&t.slug, t.bullets)),
        RecordKind::Session => Session::from_body(&record.body)
            .ok()
            .map(|s| prepend_header(&s.name, s.turns.into_iter().map(|turn| turn.text).collect())),
        RecordKind::Ticket => Ticket::from_body(&record.body).ok().map(|t| {
            let mut cs = vec![format!("{}\n{}", t.title, t.labels.join(" "))];
            cs.extend(split_paragraphs(&t.body.markdown));
            cs
        }),
        RecordKind::TicketEvent => TicketEvent::from_body(&record.body)
            .ok()
            .map(|e| vec![e.body]),
        // Not knowledge-bearing: a checkpoint is opaque state; a graph manifest
        // is a path -> content-hash map, not natural-language text (ADR 0011/0012).
        RecordKind::Checkpoint | RecordKind::GraphManifest => None,
    }
}

/// Fold `header` into the first `pieces` chunk so it stays searchable; if
/// `pieces` is empty, the header stands alone as the single chunk.
fn prepend_header(header: &str, mut pieces: Vec<String>) -> Vec<String> {
    match pieces.first_mut() {
        Some(first) => *first = format!("{header}\n{first}"),
        None => pieces.push(header.to_string()),
    }
    pieces
}

/// How many chunk matches to over-fetch per requested record in [`query`], so
/// de-duping chunks back to their parents still yields up to `k` distinct
/// records.
const OVERFETCH: usize = 8;

/// Separates a parent id from a chunk ordinal in a derived index key. ASCII Unit
/// Separator (never appears in real ids, which may themselves contain `#`/`/`),
/// so [`parent_key`] recovers the parent unambiguously.
const CHUNK_SEP: char = '\u{1f}';

/// The index key for chunk `ordinal` of the record at `parent`: same
/// namespace/collection, with the ordinal folded into the `id` (so a
/// [`KeyPrefix`] filter, which matches on namespace/collection only, is
/// unaffected). Every chunk — including a single-chunk fallback — carries an
/// ordinal, so keys are always parseable.
fn chunk_key(parent: &gonzalo_core::RecordKey, ordinal: usize) -> gonzalo_core::RecordKey {
    gonzalo_core::RecordKey::new(
        &parent.namespace,
        &parent.collection,
        format!("{}{CHUNK_SEP}{ordinal}", parent.id),
    )
}

/// Inverse of [`chunk_key`]: recover the parent key and chunk ordinal. A key
/// without the separator (defensive) is treated as parent, ordinal 0.
fn parent_key(chunk: &gonzalo_core::RecordKey) -> (gonzalo_core::RecordKey, usize) {
    match chunk.id.rsplit_once(CHUNK_SEP) {
        Some((id, ord)) => {
            let ordinal = ord.parse().unwrap_or(0);
            (
                gonzalo_core::RecordKey::new(&chunk.namespace, &chunk.collection, id),
                ordinal,
            )
        }
        None => (chunk.clone(), 0),
    }
}

/// Split text on blank-line boundaries into trimmed, non-empty paragraphs.
fn split_paragraphs(text: &str) -> Vec<String> {
    text.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
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
    fn chunk_splits_session_by_turn() {
        use gonzalo_domain::Turn;
        let session = Session {
            name: "sess".into(),
            turns: vec![
                Turn {
                    role: "user".into(),
                    text: "first turn about rust".into(),
                },
                Turn {
                    role: "assistant".into(),
                    text: "second turn about cooking".into(),
                },
                Turn {
                    role: "user".into(),
                    text: "third turn about music".into(),
                },
            ],
        };
        let key = RecordKey::new("caliban", "sessions", "s1");
        let chunks = chunk(&record(
            &key,
            RecordKind::Session,
            session.to_body().unwrap(),
        ))
        .unwrap();
        assert_eq!(chunks.len(), 3);
        // Session name is prepended to the first chunk so it stays searchable.
        assert!(chunks[0].contains("sess"));
        assert!(chunks[0].contains("first turn about rust"));
        assert!(chunks[1].contains("second turn about cooking"));
        assert!(chunks[2].contains("third turn about music"));
    }

    #[test]
    fn chunk_splits_topic_by_bullet_and_falls_back_to_one() {
        // Multiple bullets -> one chunk each.
        let topic = Topic {
            slug: "rust".into(),
            bullets: vec!["use clippy".into(), "run rustfmt".into()],
        };
        let key = RecordKey::new("caliban", "topics", "rust");
        let chunks = chunk(&record(&key, RecordKind::Topic, topic.to_body().unwrap())).unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].contains("rust"));
        assert!(chunks[0].contains("use clippy"));
        assert!(chunks[1].contains("run rustfmt"));

        // A single-piece record is the one-document fallback.
        let one = Topic {
            slug: "solo".into(),
            bullets: vec!["only bullet".into()],
        };
        let chunks = chunk(&record(&key, RecordKind::Topic, one.to_body().unwrap())).unwrap();
        assert_eq!(chunks.len(), 1);

        // Non-knowledge kinds still gate to None.
        let ck = record(&key, RecordKind::Checkpoint, Body::Inline(b"{}".to_vec()));
        assert_eq!(chunk(&ck), None);
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

    fn turn(text: &str) -> gonzalo_domain::Turn {
        gonzalo_domain::Turn {
            role: "user".into(),
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn query_ranks_a_matching_turn_over_a_diluted_decoy_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());

        // A session whose one-document average is heavily diluted: two short
        // turns match "gamma", one long turn is all "alpha". Averaged into one
        // vector, the alpha turn drowns out the gamma signal.
        let sess = RecordKey::new("caliban", "sessions", "s1");
        put(
            &store,
            record(
                &sess,
                RecordKind::Session,
                Session {
                    name: "s".into(),
                    turns: vec![
                        turn("gamma"),
                        turn("gamma"),
                        turn("alpha alpha alpha alpha alpha"),
                    ],
                }
                .to_body()
                .unwrap(),
            ),
        )
        .await;

        // A decoy whose whole (short) document is closer to "gamma" than the
        // session's diluted average — so under one-document embedding the decoy
        // outranks the session.
        let decoy = RecordKey::new("caliban", "topics", "decoy");
        put(
            &store,
            record(
                &decoy,
                RecordKind::Topic,
                Topic {
                    slug: "gamma".into(),
                    bullets: vec!["alpha".into()],
                }
                .to_body()
                .unwrap(),
            ),
        )
        .await;

        let ks = KnowledgeStore::new(store, MemoryVectorIndex::default(), Bow);
        assert!(ks.ingest(&sess).await.unwrap());
        assert!(ks.ingest(&decoy).await.unwrap());

        let hits = ks.query("gamma", 5, &KeyPrefix::default()).await.unwrap();

        // Chunking lets the matching turn score on its own, so the session ranks
        // first...
        assert_eq!(hits[0].record.key, sess, "matching turn should rank first");
        // ...and the two matching turns de-dup to a single hit for the record.
        assert_eq!(
            hits.iter().filter(|h| h.record.key == sess).count(),
            1,
            "session should appear exactly once"
        );
    }

    #[tokio::test]
    async fn query_chunks_returns_individual_chunks_with_ordinals() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());
        let sess = RecordKey::new("caliban", "sessions", "s1");
        put(
            &store,
            record(
                &sess,
                RecordKind::Session,
                Session {
                    name: "s".into(),
                    turns: vec![turn("gamma"), turn("gamma"), turn("alpha")],
                }
                .to_body()
                .unwrap(),
            ),
        )
        .await;

        let ks = KnowledgeStore::new(store, MemoryVectorIndex::default(), Bow);
        assert!(ks.ingest(&sess).await.unwrap());

        // The two "gamma" turns are the two closest chunks — returned
        // individually, NOT de-duped to the record.
        let hits = ks
            .query_chunks("gamma", 2, &KeyPrefix::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.record.key == sess));
        let ordinals: std::collections::BTreeSet<usize> = hits.iter().map(|h| h.ordinal).collect();
        assert_eq!(ordinals, std::collections::BTreeSet::from([0, 1]));
        assert!(hits.iter().all(|h| h.text.contains("gamma")));
    }

    #[tokio::test]
    async fn reingest_shrink_removes_orphan_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let sess = RecordKey::new("caliban", "sessions", "s1");
        let ks = KnowledgeStore::new(FsStore::new(dir.path()), MemoryVectorIndex::default(), Bow);

        // v1: three turns -> three chunks.
        put(
            ks.store(),
            record(
                &sess,
                RecordKind::Session,
                Session {
                    name: "s".into(),
                    turns: vec![turn("gamma"), turn("delta"), turn("epsilon")],
                }
                .to_body()
                .unwrap(),
            ),
        )
        .await;
        assert!(ks.ingest(&sess).await.unwrap());

        // The third turn's content is indexed while it exists.
        let before = ks
            .query_chunks("epsilon", 10, &KeyPrefix::default())
            .await
            .unwrap();
        assert!(before.iter().any(|h| h.text.contains("epsilon")));

        // v2: shrink to a single turn. The record now has one chunk; the "delta"
        // and "epsilon" chunks are orphaned in the index.
        let current = ks.store().get(&sess).await.unwrap().unwrap();
        let body = Session {
            name: "s".into(),
            turns: vec![turn("gamma")],
        }
        .to_body()
        .unwrap();
        let mut v2 = record(&sess, RecordKind::Session, body.clone());
        v2.parent = Some(current.revision.clone());
        v2.revision = current.revision.next(body.bytes());
        assert!(matches!(
            ks.store().put(v2, Some(current.revision)).await.unwrap(),
            PutResult::Committed(_)
        ));
        assert!(ks.ingest(&sess).await.unwrap());

        // The orphaned "epsilon" (and "delta") chunks are gone from the index;
        // only the surviving "gamma" chunk remains.
        let after = ks
            .query_chunks("epsilon", 10, &KeyPrefix::default())
            .await
            .unwrap();
        assert!(
            !after.iter().any(|h| h.text.contains("epsilon")),
            "orphaned chunk should have been removed on re-ingest"
        );
        assert!(
            !after.iter().any(|h| h.text.contains("delta")),
            "orphaned chunk should have been removed on re-ingest"
        );
        assert!(
            after.iter().any(|h| h.text.contains("gamma")),
            "the surviving turn should still be indexed"
        );
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

    #[cfg(feature = "graph")]
    #[tokio::test]
    async fn query_in_subgraph_restricts_to_the_neighborhood() {
        use gonzalo_graph::{GraphStore, InMemoryGraphStore, build_rust};

        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());

        // Three topic records keyed by symbol name, all mentioning "database",
        // so all three are semantically similar to the query.
        for id in ["root", "helper", "faraway"] {
            let key = RecordKey::new("code", "symbols", id);
            let topic = Topic {
                slug: id.into(),
                bullets: vec!["touches the database layer".into()],
            };
            put(
                &store,
                record(&key, RecordKind::Topic, topic.to_body().unwrap()),
            )
            .await;
        }
        let ks = KnowledgeStore::new(store, MemoryVectorIndex::default(), Bow);
        for id in ["root", "helper", "faraway"] {
            assert!(
                ks.ingest(&RecordKey::new("code", "symbols", id))
                    .await
                    .unwrap()
            );
        }

        // Call graph: root -> helper; faraway is unrelated. So root's
        // neighborhood is {root, helper}.
        let mut graph = InMemoryGraphStore::new();
        graph.insert(
            "lib.rs",
            build_rust("fn root() { helper(); }\nfn helper() {}\nfn faraway() {}"),
        );

        let hits = ks
            .query_in_subgraph("database", 10, &graph, "root")
            .await
            .unwrap();
        let ids: BTreeMap<String, ()> =
            hits.iter().map(|h| (h.record.key.id.clone(), ())).collect();
        assert!(ids.contains_key("root"));
        assert!(ids.contains_key("helper"));
        assert!(
            !ids.contains_key("faraway"),
            "faraway is semantically similar but outside root's neighborhood"
        );
    }
}
