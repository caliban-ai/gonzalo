//! The replica dispatcher — the harness's stand-in for the k8s Service.
//!
//! Holds one [`Store`] handle per `gonzalod` replica, round-robins each op
//! (`get`, `get_raw`, `put`, `delete`) to a replica, and fails over on a
//! `Backend` error (transport, dead replica); `Conflict` and `NotFound` are
//! answers, never failover triggers. A [`PutResult::Conflict`] is a valid
//! answer from a live replica and is returned unchanged — it is never a
//! failover trigger (the caller re-reads and retries the RMW). This is
//! exactly the failover path a k8s agent pod relies on when a `gonzalod` pod
//! dies behind the Service.

use gonzalo_core::{
    CoreError, DeleteResult, PutResult, Record, RecordKey, Result, Revision, Store,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Fans record ops across N replica stores with round-robin + failover.
pub struct Dispatcher {
    replicas: Vec<Arc<dyn Store>>,
    next: AtomicUsize,
}

impl Dispatcher {
    /// Build a dispatcher over one or more replica stores.
    pub fn new(replicas: Vec<Arc<dyn Store>>) -> Self {
        assert!(
            !replicas.is_empty(),
            "dispatcher needs at least one replica"
        );
        Self {
            replicas,
            next: AtomicUsize::new(0),
        }
    }

    /// Number of replica handles (live or not — deadness is discovered per op).
    pub fn len(&self) -> usize {
        self.replicas.len()
    }

    /// Always false; provided to satisfy clippy's `len`-without-`is_empty` lint.
    pub fn is_empty(&self) -> bool {
        self.replicas.is_empty()
    }

    /// Every replica handle, in construction order. Used to read **each**
    /// replica directly (no round-robin, no failover) once the run has settled.
    pub fn replicas(&self) -> &[Arc<dyn Store>] {
        &self.replicas
    }

    fn start(&self) -> usize {
        self.next.fetch_add(1, Ordering::Relaxed) % self.replicas.len()
    }

    /// Try replicas in round-robin order. Only `CoreError::Backend` (transport,
    /// 5xx, a dead or killed replica) fails over. `NotFound` and `Serde` are
    /// answers from a live replica over shared storage and return at once, like
    /// `Conflict`.
    async fn with_failover<T, F, Fut>(&self, op: F) -> Result<T>
    where
        F: Fn(Arc<dyn Store>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let n = self.replicas.len();
        let start = self.start();
        let mut last_err = None;
        for offset in 0..n {
            match op(self.replicas[(start + offset) % n].clone()).await {
                Err(e @ CoreError::Backend(_)) => last_err = Some(e),
                other => return other,
            }
        }
        Err(last_err.expect("at least one replica was attempted"))
    }

    /// `get` with round-robin start + failover across all replicas.
    pub async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.with_failover(|s| {
            let key = key.clone();
            async move { s.get(&key).await }
        })
        .await
    }

    /// `put` with round-robin start + failover across all replicas. The record is
    /// cloned per attempt so a failover can re-issue it to another replica. A
    /// `Conflict` is a valid `Ok` outcome and returns immediately (not a failover).
    pub async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        self.with_failover(|s| {
            let record = record.clone();
            let expected = expected.clone();
            async move { s.put(record, expected).await }
        })
        .await
    }

    /// `delete` with round-robin start + failover across all replicas. A
    /// `Conflict` is a valid `Ok` outcome and returns immediately (not a failover).
    /// A failover after a delete that did commit on the dead replica is safe: the
    /// retry sees a tombstone and returns `Deleted` without writing.
    pub async fn delete(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
    ) -> Result<DeleteResult> {
        self.with_failover(|s| {
            let key = key.clone();
            let expected = expected.clone();
            async move { s.delete(&key, expected).await }
        })
        .await
    }

    /// `get_raw` (tombstones visible) with round-robin start + failover.
    pub async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.with_failover(|s| {
            let key = key.clone();
            async move { s.get_raw(&key).await }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use gonzalo_core::{
        Body, CoreError, DeleteResult, Identity, KeyPrefix, Meta, RecordKind, store::Conflict,
    };
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicBool;

    /// A `Store` double whose liveness and canned outcome are controllable.
    struct MockStore {
        alive: AtomicBool,
        calls: AtomicUsize,
    }
    impl MockStore {
        fn alive() -> Arc<Self> {
            Arc::new(Self {
                alive: AtomicBool::new(true),
                calls: AtomicUsize::new(0),
            })
        }
        fn dead() -> Arc<Self> {
            Arc::new(Self {
                alive: AtomicBool::new(false),
                calls: AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl Store for MockStore {
        async fn get(&self, _key: &RecordKey) -> Result<Option<Record>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.alive.load(Ordering::SeqCst) {
                Ok(None)
            } else {
                Err(CoreError::Backend("connection refused".into()))
            }
        }
        async fn put(&self, _record: Record, _expected: Option<Revision>) -> Result<PutResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.alive.load(Ordering::SeqCst) {
                Ok(PutResult::Committed(Revision::initial(b"x")))
            } else {
                Err(CoreError::Backend("connection refused".into()))
            }
        }
        async fn list(&self, _prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            Ok(Vec::new())
        }
        async fn delete_as(
            &self,
            _key: &RecordKey,
            _expected: Option<Revision>,
            _author: Option<Identity>,
        ) -> Result<DeleteResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.alive.load(Ordering::SeqCst) {
                Ok(DeleteResult::Deleted)
            } else {
                Err(CoreError::Backend("connection refused".into()))
            }
        }
        async fn put_raw(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
            <Self as Store>::put(self, record, expected).await
        }
        async fn get_raw(&self, key: &RecordKey) -> Result<Option<Record>> {
            self.get(key).await
        }
        async fn list_raw(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            self.list(prefix).await
        }
        async fn purge(&self, key: &RecordKey, expected: Revision) -> Result<DeleteResult> {
            self.delete(key, Some(expected)).await
        }
    }

    fn as_store(m: &Arc<MockStore>) -> Arc<dyn Store> {
        m.clone()
    }

    fn rec() -> Record {
        let body = Body::Inline(b"x".to_vec());
        Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind: RecordKind::Topic,
            meta: Meta {
                author: Identity::new("soak"),
                origin_system: "soak".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            key: RecordKey::new("ns", "col", "k"),
            ancestors: Vec::new(),
            deleted_at: None,
        }
    }

    /// A live replica that answers `put` and `delete_as` with fixed results and
    /// counts calls. Every other method answers empty.
    struct Answering {
        calls: AtomicUsize,
        put: fn() -> Result<PutResult>,
        delete: fn() -> Result<DeleteResult>,
    }
    impl Answering {
        fn new(put: fn() -> Result<PutResult>, delete: fn() -> Result<DeleteResult>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                put,
                delete,
            })
        }
    }
    #[async_trait]
    impl Store for Answering {
        async fn get(&self, _key: &RecordKey) -> Result<Option<Record>> {
            Ok(None)
        }
        async fn put(&self, _record: Record, _expected: Option<Revision>) -> Result<PutResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.put)()
        }
        async fn list(&self, _prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            Ok(Vec::new())
        }
        async fn delete_as(
            &self,
            _key: &RecordKey,
            _expected: Option<Revision>,
            _author: Option<Identity>,
        ) -> Result<DeleteResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.delete)()
        }
        async fn get_raw(&self, _key: &RecordKey) -> Result<Option<Record>> {
            Ok(None)
        }
        async fn put_raw(&self, _record: Record, _expected: Option<Revision>) -> Result<PutResult> {
            Ok(PutResult::Committed(Revision::initial(b"x")))
        }
        async fn list_raw(&self, _prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
            Ok(Vec::new())
        }
        async fn purge(&self, _key: &RecordKey, _expected: Revision) -> Result<DeleteResult> {
            Ok(DeleteResult::Deleted)
        }
    }

    #[tokio::test]
    async fn failover_skips_dead_replica() {
        let dead = MockStore::dead();
        let live = MockStore::alive();
        let d = Dispatcher::new(vec![as_store(&dead), as_store(&live)]);
        // Round-robin starts at replica 0 (dead) → must fail over to 1 (live).
        let out = d.get(&RecordKey::new("ns", "col", "k")).await;
        assert!(
            out.is_ok(),
            "should have failed over to the live replica: {out:?}"
        );
        assert_eq!(dead.calls(), 1, "dead replica was tried");
        assert_eq!(live.calls(), 1, "then the live replica served it");
    }

    #[tokio::test]
    async fn all_dead_returns_error() {
        let a = MockStore::dead();
        let b = MockStore::dead();
        let d = Dispatcher::new(vec![as_store(&a), as_store(&b)]);
        assert!(d.get(&RecordKey::new("ns", "col", "k")).await.is_err());
        assert_eq!(a.calls() + b.calls(), 2, "both replicas were attempted");
    }

    #[tokio::test]
    async fn live_replica_short_circuits() {
        let a = MockStore::alive();
        let b = MockStore::alive();
        let c = MockStore::alive();
        let d = Dispatcher::new(vec![as_store(&a), as_store(&b), as_store(&c)]);
        let _ = d.get(&RecordKey::new("ns", "col", "k")).await.unwrap();
        assert_eq!(
            a.calls() + b.calls() + c.calls(),
            1,
            "exactly one replica served the op"
        );
    }

    #[tokio::test]
    async fn round_robin_distributes_load() {
        let a = MockStore::alive();
        let b = MockStore::alive();
        let c = MockStore::alive();
        let d = Dispatcher::new(vec![as_store(&a), as_store(&b), as_store(&c)]);
        for _ in 0..3 {
            d.get(&RecordKey::new("ns", "col", "k")).await.unwrap();
        }
        assert_eq!(
            (a.calls(), b.calls(), c.calls()),
            (1, 1, 1),
            "round-robin over 3 replicas"
        );
    }

    #[tokio::test]
    async fn put_fails_over_too() {
        let dead = MockStore::dead();
        let live = MockStore::alive();
        let d = Dispatcher::new(vec![as_store(&dead), as_store(&live)]);
        let out = d.put(rec(), None).await;
        assert!(
            matches!(out, Ok(PutResult::Committed(_))),
            "put failed over: {out:?}"
        );
    }

    #[tokio::test]
    async fn delete_fails_over_too() {
        let dead = MockStore::dead();
        let live = MockStore::alive();
        let d = Dispatcher::new(vec![as_store(&dead), as_store(&live)]);
        let out = d.delete(&RecordKey::new("ns", "col", "k"), None).await;
        assert!(
            matches!(out, Ok(DeleteResult::Deleted)),
            "delete failed over: {out:?}"
        );
        assert_eq!(dead.calls(), 1, "dead replica was tried");
        assert_eq!(live.calls(), 1, "then the live replica served it");
    }

    #[tokio::test]
    async fn get_raw_fails_over_too() {
        let dead = MockStore::dead();
        let live = MockStore::alive();
        let d = Dispatcher::new(vec![as_store(&dead), as_store(&live)]);
        let out = d.get_raw(&RecordKey::new("ns", "col", "k")).await;
        assert!(matches!(out, Ok(None)), "get_raw failed over: {out:?}");
        assert_eq!(dead.calls() + live.calls(), 2);
    }

    #[tokio::test]
    async fn replicas_exposes_every_handle_in_order() {
        let a = MockStore::alive();
        let b = MockStore::dead();
        let d = Dispatcher::new(vec![as_store(&a), as_store(&b)]);
        assert_eq!(d.replicas().len(), 2);
        // Reading each handle directly bypasses round-robin and failover.
        assert!(
            d.replicas()[0]
                .get_raw(&RecordKey::new("ns", "col", "k"))
                .await
                .is_ok()
        );
        assert!(
            d.replicas()[1]
                .get_raw(&RecordKey::new("ns", "col", "k"))
                .await
                .is_err()
        );
    }

    // A conflict from a live replica is returned as-is, never retried elsewhere.
    #[tokio::test]
    async fn conflict_is_returned_not_retried() {
        let c0 = Answering::new(
            || {
                Ok(PutResult::Conflict(Box::new(Conflict {
                    key: RecordKey::new("ns", "col", "k"),
                    expected: None,
                    current: rec(),
                })))
            },
            || Ok(DeleteResult::Deleted),
        );
        let c1 = Answering::new(
            || {
                Ok(PutResult::Conflict(Box::new(Conflict {
                    key: RecordKey::new("ns", "col", "k"),
                    expected: None,
                    current: rec(),
                })))
            },
            || Ok(DeleteResult::Deleted),
        );
        let d = Dispatcher::new(vec![
            c0.clone() as Arc<dyn Store>,
            c1.clone() as Arc<dyn Store>,
        ]);
        let out = d.put(rec(), None).await;
        assert!(
            matches!(out, Ok(PutResult::Conflict(_))),
            "conflict returned: {out:?}"
        );
        assert_eq!(
            c0.calls.load(Ordering::SeqCst) + c1.calls.load(Ordering::SeqCst),
            1,
            "a conflict is a definitive answer — not retried on another replica"
        );
    }

    // A delete conflict from a live replica is a definitive answer, never retried.
    #[tokio::test]
    async fn delete_conflict_is_returned_not_retried() {
        let c0 = Answering::new(
            || Ok(PutResult::Committed(Revision::initial(b"x"))),
            || {
                Ok(DeleteResult::Conflict(Box::new(Conflict {
                    key: RecordKey::new("ns", "col", "k"),
                    expected: Some(Revision::initial(b"stale")),
                    current: rec(),
                })))
            },
        );
        let c1 = Answering::new(
            || Ok(PutResult::Committed(Revision::initial(b"x"))),
            || {
                Ok(DeleteResult::Conflict(Box::new(Conflict {
                    key: RecordKey::new("ns", "col", "k"),
                    expected: Some(Revision::initial(b"stale")),
                    current: rec(),
                })))
            },
        );
        let d = Dispatcher::new(vec![
            c0.clone() as Arc<dyn Store>,
            c1.clone() as Arc<dyn Store>,
        ]);
        let out = d
            .delete(
                &RecordKey::new("ns", "col", "k"),
                Some(Revision::initial(b"stale")),
            )
            .await;
        assert!(
            matches!(out, Ok(DeleteResult::Conflict(_))),
            "conflict returned: {out:?}"
        );
        assert_eq!(
            c0.calls.load(Ordering::SeqCst) + c1.calls.load(Ordering::SeqCst),
            1,
            "a delete conflict is not retried on another replica"
        );
    }

    // A `NotFound` from a live replica is an answer, not a failover trigger.
    #[tokio::test]
    async fn not_found_is_returned_not_retried() {
        let c0 = Answering::new(
            || Err(CoreError::NotFound(RecordKey::new("ns", "col", "k"))),
            || Ok(DeleteResult::Deleted),
        );
        let c1 = Answering::new(
            || Err(CoreError::NotFound(RecordKey::new("ns", "col", "k"))),
            || Ok(DeleteResult::Deleted),
        );
        let d = Dispatcher::new(vec![
            c0.clone() as Arc<dyn Store>,
            c1.clone() as Arc<dyn Store>,
        ]);
        let out = d.put(rec(), None).await;
        assert!(
            matches!(out, Err(CoreError::NotFound(_))),
            "not found returned: {out:?}"
        );
        assert_eq!(
            c0.calls.load(Ordering::SeqCst) + c1.calls.load(Ordering::SeqCst),
            1,
            "a NotFound answer from a live replica is not retried on another replica"
        );
    }
}
