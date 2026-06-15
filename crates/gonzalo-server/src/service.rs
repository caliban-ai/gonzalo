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
    /// the backing store. The error is typed so each transport can return the
    /// right status: a misconfigured request is a client error, a
    /// build/ingest failure is a server error.
    pub async fn ticket_sync(
        &self,
        conn: &Connection,
        author: &str,
    ) -> std::result::Result<IngestSummary, TicketSyncError> {
        let source = gonzalo_ticket_config::build_source(conn).map_err(classify_config_err)?;
        gonzalo_ticket::ingest(source.as_ref(), self.store.as_ref(), author)
            .await
            .map_err(|e| TicketSyncError::Internal(e.to_string()))
    }
}

/// Error from a ticket sync, split so transports can return the right status:
/// a misconfigured request is a client error (400 / invalid_argument), a
/// build/ingest/transport failure is a server error (500 / internal).
#[derive(Debug, thiserror::Error)]
pub enum TicketSyncError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// A misconfigured connection is the caller's fault; a failure constructing the
/// underlying client is ours.
fn classify_config_err(e: gonzalo_ticket_config::ConfigError) -> TicketSyncError {
    use gonzalo_ticket_config::ConfigError::*;
    let msg = e.to_string();
    match e {
        Read(..) | Parse(..) | MissingEnv { .. } | UnknownProvider { .. } | BadCategory { .. } => {
            TicketSyncError::BadRequest(msg)
        }
        Source(..) => TicketSyncError::Internal(msg),
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
        let result = svc.ticket_sync(&conn, "tester").await;
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("SVC_TEST_TOKEN");
        }
        let err = result.unwrap_err();
        assert!(matches!(err, TicketSyncError::BadRequest(_)), "got {err:?}");
        assert!(err.to_string().contains("unknown provider"));
    }
}
