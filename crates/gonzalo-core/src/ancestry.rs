//! A [`Store`] decorator that retains each committed body in a content-addressed
//! [`BlobStore`], keyed by its revision hash, so [`sync`](crate::sync) can fetch
//! a divergence's common ancestor for a true 3-way merge (ADR 0016).

use async_trait::async_trait;

use crate::{
    BlobStore, DeleteResult, KeyPrefix, PutResult, Record, RecordKey, Result, Revision, Store,
};

/// Wraps a record [`Store`] and an ancestry [`BlobStore`]. On a committed `put`
/// it also writes the record's `body.bytes()` to the ancestry store; because
/// `Revision.hash == ContentHash::of(body.bytes())`, each version's body is
/// later retrievable by its revision hash. `get`/`list` delegate unchanged.
pub struct AncestryStore<S, B> {
    inner: S,
    ancestry: B,
}

impl<S, B> AncestryStore<S, B> {
    pub fn new(inner: S, ancestry: B) -> Self {
        Self { inner, ancestry }
    }

    /// The ancestry blob store (revision hash → body bytes), to hand to
    /// [`sync_with_ancestry`](crate::sync::sync_with_ancestry).
    pub fn ancestry(&self) -> &B {
        &self.ancestry
    }

    /// The wrapped record store.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

#[async_trait]
impl<S: Store, B: BlobStore> Store for AncestryStore<S, B> {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.inner.get(key).await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        // Retain the body under its revision hash on a successful commit, so a
        // later divergence can be merged against this exact version.
        let body_bytes = record.body.bytes().to_vec();
        let outcome = self.inner.put(record, expected).await?;
        if matches!(outcome, PutResult::Committed(_)) {
            self.ancestry.put_blob(&body_bytes).await?;
        }
        Ok(outcome)
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.inner.list(prefix).await
    }

    async fn delete(&self, key: &RecordKey, expected: Option<Revision>) -> Result<DeleteResult> {
        // Delete is local and leaves ancestry blobs untouched: retained bodies
        // stay available for a later divergence's 3-way merge (ADR 0016), and a
        // sync from a peer may resurrect the record (ADR 0018).
        self.inner.delete(key, expected).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::Conflict;
    use crate::{Body, ContentHash, CoreError, Identity, Meta, RecordKind, revision::Revision};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// An in-memory `Store` + `BlobStore` double.
    #[derive(Default)]
    pub(crate) struct Mem {
        records: Mutex<BTreeMap<RecordKey, Record>>,
        blobs: Mutex<BTreeMap<ContentHash, Vec<u8>>>,
    }

    #[async_trait]
    impl Store for Mem {
        async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
            Ok(self.records.lock().unwrap().get(key).cloned())
        }
        async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            let mut g = self.records.lock().unwrap();
            let current = g.get(&record.key).map(|r| r.revision.clone());
            if current != expected {
                if let Some(cur) = g.get(&record.key).cloned() {
                    return Ok(PutResult::Conflict(Box::new(Conflict {
                        key: record.key.clone(),
                        expected,
                        current: cur,
                    })));
                }
                return Err(CoreError::NotFound(record.key.clone()));
            }
            let rev = record.revision.clone();
            g.insert(record.key.clone(), record);
            Ok(PutResult::Committed(rev))
        }
        async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .keys()
                .filter(|k| prefix.matches(k))
                .cloned()
                .collect())
        }
        async fn delete(
            &self,
            key: &RecordKey,
            expected: Option<Revision>,
        ) -> Result<DeleteResult> {
            let mut g = self.records.lock().unwrap();
            match g.get(key) {
                None => Ok(DeleteResult::Deleted),
                Some(cur) if expected.is_none() || expected.as_ref() == Some(&cur.revision) => {
                    g.remove(key);
                    Ok(DeleteResult::Deleted)
                }
                Some(cur) => Ok(DeleteResult::Conflict(Box::new(Conflict {
                    key: key.clone(),
                    expected,
                    current: cur.clone(),
                }))),
            }
        }
    }

    #[async_trait]
    impl BlobStore for Mem {
        async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
            let hash = ContentHash::of(content);
            self.blobs
                .lock()
                .unwrap()
                .insert(hash.clone(), content.to_vec());
            Ok(hash)
        }
        async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
            Ok(self.blobs.lock().unwrap().get(hash).cloned())
        }
        async fn list_blobs(&self) -> Result<Vec<ContentHash>> {
            Ok(self.blobs.lock().unwrap().keys().cloned().collect())
        }
        async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
            self.blobs.lock().unwrap().remove(hash);
            Ok(())
        }
    }

    pub(crate) fn rec(id: &str, kind: RecordKind, payload: &str) -> Record {
        let body = Body::Inline(payload.as_bytes().to_vec());
        Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind,
            key: RecordKey::new("ns", "col", id),
            meta: Meta {
                author: Identity::new("t"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
        }
    }

    #[tokio::test]
    async fn put_retains_body_under_revision_hash_and_delegates() {
        let store = AncestryStore::new(Mem::default(), Mem::default());
        let r = rec("k", RecordKind::MemoryTier, r#"{"a":1}"#);
        let rev = r.revision.clone();
        assert!(matches!(
            store.put(r.clone(), None).await.unwrap(),
            PutResult::Committed(_)
        ));

        // The body is retrievable from the ancestry store by its revision hash.
        let retained = store.ancestry().get_blob(&rev.hash).await.unwrap();
        assert_eq!(retained.as_deref(), Some(r.body.bytes()));

        // get/list delegate to the wrapped record store.
        assert_eq!(store.get(&r.key).await.unwrap().unwrap().revision, rev);
        assert_eq!(
            store.list(&KeyPrefix::default()).await.unwrap(),
            vec![r.key]
        );
    }

    #[tokio::test]
    async fn conflicting_put_does_not_retain() {
        let store = AncestryStore::new(Mem::default(), Mem::default());
        let r = rec("k", RecordKind::MemoryTier, r#"{"a":1}"#);
        let _ = store.put(r.clone(), None).await.unwrap();

        // A stale write (wrong `expected`) conflicts and must not retain.
        let other = rec("k", RecordKind::MemoryTier, r#"{"a":2}"#);
        let other_rev = other.revision.clone();
        assert!(matches!(
            store.put(other, None).await.unwrap(),
            PutResult::Conflict(_)
        ));
        assert!(
            store
                .ancestry()
                .get_blob(&other_rev.hash)
                .await
                .unwrap()
                .is_none()
        );
    }
}
