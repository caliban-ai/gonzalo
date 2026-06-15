//! The transport-agnostic service layer. Both the gRPC and HTTP transports
//! delegate to this; it simply forwards to the backing `Store`.

use gonzalo_core::{KeyPrefix, PutResult, Record, RecordKey, Result, Revision, Store};
use gonzalo_ticket::IngestSummary;
use gonzalo_ticket_config::Connection;
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

    /// Build a source for `conn` from the registry and ingest its tickets into
    /// the backing store. Errors are flattened to strings at this boundary so
    /// both transports can surface them uniformly.
    pub async fn ticket_sync(
        &self,
        conn: &Connection,
        author: &str,
    ) -> std::result::Result<IngestSummary, String> {
        let source = gonzalo_ticket_config::build_source(conn).map_err(|e| e.to_string())?;
        gonzalo_ticket::ingest(source.as_ref(), self.store.as_ref(), author)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_store_fs::FsStore;
    use gonzalo_ticket_config::Connection;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn ticket_sync_rejects_unknown_provider() {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::new(Arc::new(FsStore::new(dir.path())));
        // Token must exist so we reach the provider match.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("SVC_TEST_TOKEN", "x")
        };
        let conn = Connection {
            name: "bad".into(),
            provider: "nope".into(),
            org: "caliban-ai".into(),
            project: 1,
            token_env: "SVC_TEST_TOKEN".into(),
            state_map: BTreeMap::new(),
        };
        let err = svc.ticket_sync(&conn, "tester").await.unwrap_err();
        assert!(err.contains("unknown provider"));
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("SVC_TEST_TOKEN")
        };
    }
}
