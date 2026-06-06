//! The transport-agnostic service layer. Both the gRPC and HTTP transports
//! delegate to this; it simply forwards to the backing `Store`.

use gonzalo_core::{KeyPrefix, PutResult, Record, RecordKey, Result, Revision, Store};
use std::sync::Arc;

/// Wraps a `Store` and exposes its operations to the daemon transports.
#[derive(Clone)]
pub struct Service {
    store: Arc<dyn Store>,
}

impl Service {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    pub async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.store.get(key).await
    }

    pub async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        self.store.put(record, expected).await
    }

    pub async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.store.list(prefix).await
    }
}
