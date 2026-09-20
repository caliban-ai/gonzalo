//! Test-only helpers shared by the reset and collect tests (gonzalo#203).

use crate::memstore::MemStore;
use crate::{
    Body, DeleteResult, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind,
    Result, Revision, Store,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) fn rec(ns: &str, col: &str, id: &str, payload: &str) -> Record {
    let body = Body::Inline(payload.as_bytes().to_vec());
    Record {
        key: RecordKey::new(ns, col, id),
        kind: RecordKind::Topic,
        revision: Revision::initial(body.bytes()),
        parent: None,
        body,
        meta: Meta {
            author: Identity::new("t"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        ancestors: Vec::new(),
        deleted_at: None,
        deleted_blob: None,
    }
}

pub(crate) async fn seed(store: &dyn Store, record: Record) {
    assert!(matches!(
        store.put(record, None).await.unwrap(),
        PutResult::Committed(_)
    ));
}

/// How a [`HookedStore`] interferes, once, with its target key.
pub(crate) enum Hook {
    /// Commit a concurrent edit to the key just before its `delete_as` (reset
    /// race).
    EditBeforeDelete(RecordKey),
    /// Delete and purge the key just before its `get`, so `get` returns
    /// `None`.
    VanishBeforeGet(RecordKey),
    /// Recreate the key as a fresh live record just before its `purge`
    /// (collect race).
    RecreateBeforePurge(RecordKey),
}

/// A `MemStore` that fires its [`Hook`] once, then delegates everything.
pub(crate) struct HookedStore {
    pub(crate) inner: MemStore,
    pub(crate) hook: Hook,
    pub(crate) fired: AtomicBool,
}

#[async_trait]
impl Store for HookedStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        if let Hook::VanishBeforeGet(target) = &self.hook
            && key == target
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            let current = self
                .inner
                .get_raw(key)
                .await?
                .expect("target is live before vanishing");
            assert_eq!(
                self.inner
                    .delete(key, Some(current.revision.clone()))
                    .await?,
                DeleteResult::Deleted
            );
            let tombstone = self
                .inner
                .get_raw(key)
                .await?
                .expect("tombstone exists after delete");
            assert_eq!(
                self.inner.purge(key, tombstone.revision).await?,
                DeleteResult::Deleted
            );
        }
        self.inner.get(key).await
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        self.inner.put(record, expected).await
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.inner.list(prefix).await
    }

    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        if let Hook::EditBeforeDelete(target) = &self.hook
            && key == target
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            let current = self.inner.get(key).await?.expect("target is live");
            let mut edited = current.clone();
            edited.body = Body::Inline(b"edited concurrently".to_vec());
            edited.revision = current.revision.next(edited.body.bytes());
            edited.parent = Some(current.revision.clone());
            assert!(matches!(
                self.inner.put(edited, Some(current.revision)).await?,
                PutResult::Committed(_)
            ));
        }
        self.inner.delete_as(key, expected, author).await
    }

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.inner.get_raw(key).await
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.inner.list_raw(prefix).await
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        self.inner.put_raw(record, expected).await
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        if let Hook::RecreateBeforePurge(target) = &self.hook
            && key == target
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            let fresh = rec(&key.namespace, &key.collection, &key.id, "recreated");
            assert!(matches!(
                self.inner.put(fresh, None).await?,
                PutResult::Committed(_)
            ));
        }
        self.inner.purge(key, expected).await
    }
}
