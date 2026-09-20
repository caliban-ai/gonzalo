//! A reference in-memory [`Store`] whose every decision comes from the planners
//! in [`crate::tombstone`]. Core tests use it (sync, reset, collect), and it is
//! the conformance suite's self-test: if `MemStore` and a real substrate
//! disagree, the substrate is wrong.

use crate::{
    DEFAULT_ANCESTOR_CAP, DeletePlan, DeleteResult, Identity, KeyPrefix, PurgePlan, PutPlan,
    PutResult, Record, RecordKey, Result, Revision, Store, now_ms, plan_delete, plan_purge,
    plan_put, plan_put_raw, validate_ancestor_cap,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// A store-side planner for a record write: [`plan_put`] or [`plan_put_raw`].
type PutPlanner = fn(Option<&Record>, Record, Option<Revision>, i64, usize) -> PutPlan;

/// Reference in-memory store. See the module docs.
pub struct MemStore {
    records: Mutex<BTreeMap<RecordKey, Record>>,
    cap: usize,
    clock: Option<i64>,
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemStore {
    /// An empty store with the default ancestor cap and the wall clock.
    pub fn new() -> Self {
        Self {
            records: Mutex::new(BTreeMap::new()),
            cap: DEFAULT_ANCESTOR_CAP,
            clock: None,
        }
    }

    /// Use `cap` ancestors. Panics on 0; this is a test helper, and real stores
    /// return the error from `validate_ancestor_cap` instead.
    pub fn with_ancestor_cap(mut self, cap: usize) -> Self {
        self.cap = validate_ancestor_cap(cap).unwrap_or_else(|e| panic!("{e}"));
        self
    }

    /// Stamp every tombstone with `now_ms` instead of the wall clock, for
    /// deterministic collection tests.
    pub fn with_clock(mut self, now_ms: i64) -> Self {
        self.clock = Some(now_ms);
        self
    }

    /// Every stored record, tombstones included.
    pub fn raw_snapshot(&self) -> BTreeMap<RecordKey, Record> {
        self.records.lock().unwrap().clone()
    }

    fn now(&self) -> i64 {
        self.clock.unwrap_or_else(now_ms)
    }

    /// Shared write path for `put` and `put_raw`: lock, ask `plan` for a
    /// decision, and apply it. Synchronous, so the lock never crosses an
    /// `.await`.
    fn write(
        &self,
        record: Record,
        expected: Option<Revision>,
        plan: PutPlanner,
    ) -> Result<PutResult> {
        let mut g = self.records.lock().unwrap();
        let key = record.key.clone();
        match plan(g.get(&key), record, expected, self.now(), self.cap) {
            PutPlan::Write(stored) => {
                let rev = stored.revision.clone();
                g.insert(key, stored);
                Ok(PutResult::Committed(rev))
            }
            PutPlan::Conflict(c) => Ok(PutResult::Conflict(c)),
            PutPlan::NotFound => Err(crate::CoreError::NotFound(key)),
            PutPlan::Rejected(reason) => Err(crate::CoreError::Invalid(reason.to_string())),
        }
    }
}

#[async_trait]
impl Store for MemStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .get(key)
            .filter(|r| !r.is_tombstone())
            .cloned())
    }

    async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        self.write(record, expected, plan_put)
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .values()
            .filter(|r| !r.is_tombstone() && prefix.matches(&r.key))
            .map(|r| r.key.clone())
            .collect())
    }

    async fn delete_as(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
        author: Option<Identity>,
    ) -> Result<DeleteResult> {
        let now = self.now();
        let mut g = self.records.lock().unwrap();
        match plan_delete(g.get(key), expected, now, self.cap, author.as_ref()) {
            DeletePlan::Write(tombstone) => {
                g.insert(key.clone(), tombstone);
                Ok(DeleteResult::Deleted)
            }
            DeletePlan::Noop => Ok(DeleteResult::Deleted),
            DeletePlan::Conflict(c) => Ok(DeleteResult::Conflict(c)),
        }
    }

    async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        Ok(self.records.lock().unwrap().get(key).cloned())
    }

    async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .keys()
            .filter(|k| prefix.matches(k))
            .cloned()
            .collect())
    }

    async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        self.write(record, expected, plan_put_raw)
    }

    async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
        let mut g = self.records.lock().unwrap();
        match plan_purge(g.get(key), &expected) {
            PurgePlan::Remove => {
                g.remove(key);
                Ok(DeleteResult::Deleted)
            }
            PurgePlan::Noop => Ok(DeleteResult::Deleted),
            PurgePlan::Conflict(c) => Ok(DeleteResult::Conflict(c)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Body, DeleteResult, Identity, Meta, PutResult, RecordKind};

    fn rec(id: &str, body: &[u8]) -> Record {
        let body = Body::Inline(body.to_vec());
        Record {
            key: RecordKey::new("ns", "col", id),
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

    #[tokio::test]
    async fn delete_hides_from_consumer_reads_but_not_raw() {
        let store = MemStore::new().with_clock(5);
        let r = rec("a", b"x");
        assert!(matches!(
            store.put(r.clone(), None).await.unwrap(),
            PutResult::Committed(_)
        ));
        assert_eq!(
            store.delete(&r.key, None).await.unwrap(),
            DeleteResult::Deleted
        );

        assert_eq!(store.get(&r.key).await.unwrap(), None);
        assert!(store.list(&KeyPrefix::default()).await.unwrap().is_empty());

        let raw = store.get_raw(&r.key).await.unwrap().unwrap();
        assert!(raw.is_tombstone());
        assert_eq!(raw.deleted_at, Some(5));
        assert_eq!(
            store.list_raw(&KeyPrefix::default()).await.unwrap(),
            vec![r.key.clone()]
        );
        assert_eq!(store.raw_snapshot().len(), 1);
    }

    #[tokio::test]
    async fn purge_removes_the_tombstone() {
        let store = MemStore::new();
        let r = rec("a", b"x");
        let _ = store.put(r.clone(), None).await.unwrap();
        let _ = store.delete(&r.key, None).await.unwrap();
        let t = store.get_raw(&r.key).await.unwrap().unwrap();
        assert_eq!(
            store.purge(&r.key, t.revision).await.unwrap(),
            DeleteResult::Deleted
        );
        assert!(store.raw_snapshot().is_empty());
    }

    #[tokio::test]
    async fn ancestor_cap_is_applied() {
        let store = MemStore::new().with_ancestor_cap(2);
        let mut r = rec("a", b"0");
        let mut rev = match store.put(r.clone(), None).await.unwrap() {
            PutResult::Committed(rev) => rev,
            PutResult::Conflict(c) => panic!("{c:?}"),
        };
        for i in 1..=4u8 {
            r.body = Body::Inline(vec![i]);
            r.revision = rev.next(&[i]);
            rev = match store.put(r.clone(), Some(rev.clone())).await.unwrap() {
                PutResult::Committed(rev) => rev,
                PutResult::Conflict(c) => panic!("{c:?}"),
            };
        }
        assert_eq!(
            store
                .get_raw(&r.key)
                .await
                .unwrap()
                .unwrap()
                .ancestors
                .len(),
            2
        );
    }

    #[test]
    #[should_panic(expected = "ancestor cap must be at least 1")]
    fn zero_cap_panics() {
        let _ = MemStore::new().with_ancestor_cap(0);
    }
}
