//! The replica dispatcher — the harness's stand-in for the k8s Service.
//!
//! Holds one [`Store`] handle per `gonzalod` replica, round-robins each op to a
//! replica, and on a **transport error** (a dead/killed replica) fails over to
//! the next one. A [`PutResult::Conflict`] is a valid answer from a live replica
//! and is returned unchanged — it is never a failover trigger (the caller
//! re-reads and retries the RMW). This is exactly the failover path a k8s agent
//! pod relies on when a `gonzalod` pod dies behind the Service.

use gonzalo_core::{PutResult, Record, RecordKey, Result, Revision, Store};
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

    fn start(&self) -> usize {
        self.next.fetch_add(1, Ordering::Relaxed) % self.replicas.len()
    }

    /// `get` with round-robin start + failover across all replicas.
    pub async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        let n = self.replicas.len();
        let start = self.start();
        let mut last_err = None;
        for offset in 0..n {
            let idx = (start + offset) % n;
            match self.replicas[idx].get(key).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("at least one replica was attempted"))
    }

    /// `put` with round-robin start + failover across all replicas. The record is
    /// cloned per attempt so a failover can re-issue it to another replica. A
    /// `Conflict` is a valid `Ok` outcome and returns immediately (not a failover).
    pub async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        let n = self.replicas.len();
        let start = self.start();
        let mut last_err = None;
        for offset in 0..n {
            let idx = (start + offset) % n;
            match self.replicas[idx]
                .put(record.clone(), expected.clone())
                .await
            {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("at least one replica was attempted"))
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
        async fn delete(
            &self,
            _key: &RecordKey,
            _expected: Option<Revision>,
        ) -> Result<DeleteResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.alive.load(Ordering::SeqCst) {
                Ok(DeleteResult::Deleted)
            } else {
                Err(CoreError::Backend("connection refused".into()))
            }
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

    // A conflict from a live replica is returned as-is, never retried elsewhere.
    #[tokio::test]
    async fn conflict_is_returned_not_retried() {
        struct Conflicter(AtomicUsize);
        #[async_trait]
        impl Store for Conflicter {
            async fn get(&self, _k: &RecordKey) -> Result<Option<Record>> {
                Ok(None)
            }
            async fn put(&self, _r: Record, _e: Option<Revision>) -> Result<PutResult> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(PutResult::Conflict(Box::new(Conflict {
                    key: RecordKey::new("ns", "col", "k"),
                    expected: None,
                    current: rec(),
                })))
            }
            async fn list(&self, _p: &KeyPrefix) -> Result<Vec<RecordKey>> {
                Ok(Vec::new())
            }
            async fn delete(&self, _k: &RecordKey, _e: Option<Revision>) -> Result<DeleteResult> {
                Ok(DeleteResult::Deleted)
            }
        }
        let c0 = Arc::new(Conflicter(AtomicUsize::new(0)));
        let c1 = Arc::new(Conflicter(AtomicUsize::new(0)));
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
            c0.0.load(Ordering::SeqCst) + c1.0.load(Ordering::SeqCst),
            1,
            "a conflict is a definitive answer — not retried on another replica"
        );
    }
}
