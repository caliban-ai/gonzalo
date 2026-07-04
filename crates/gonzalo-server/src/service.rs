//! The transport-agnostic service layer. Both the gRPC and HTTP transports
//! delegate to this; it forwards record ops to the backing `Store` and answers
//! code-graph queries by assembling a view from the store + blob store.

use gonzalo_core::{
    BlobStore, KeyPrefix, Manifest, PutResult, Record, RecordKey, Result, Revision, Store,
};
use gonzalo_graph::{GraphStore, InMemoryGraphStore, Located, Reference, Symbol, assemble};
use gonzalo_ticket::IngestSummary;
use gonzalo_ticket_config::Connection;
use std::sync::Arc;

/// Wraps a `Store` (records) and a `BlobStore` (content-addressed slices) and
/// exposes their operations to the daemon transports. The daemon backs both
/// with the same `FsStore`.
#[derive(Clone)]
pub struct Service {
    store: Arc<dyn Store>,
    blobs: Arc<dyn BlobStore>,
}

impl Service {
    pub fn new(store: Arc<dyn Store>, blobs: Arc<dyn BlobStore>) -> Self {
        Self { store, blobs }
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

    // --- Code graph queries (EPIC C) ---
    // Each query selects a view by `(repo, view_id)`, loads its manifest,
    // assembles the view's slices, and answers server-side. Assembly is
    // per-call for now; a view cache is a follow-on.

    /// Load a view's manifest, or an empty manifest if the view has none yet —
    /// an unknown/empty view simply yields empty query results.
    async fn load_manifest(&self, repo: &str, view_id: &str) -> Result<Manifest> {
        match self.store.get(&Manifest::key(repo, view_id)).await? {
            Some(record) => Manifest::from_body(&record.body),
            None => Ok(Manifest::new()),
        }
    }

    /// Assemble the `(repo, view_id)` view into a queryable graph.
    async fn view(&self, repo: &str, view_id: &str) -> Result<InMemoryGraphStore> {
        let manifest = self.load_manifest(repo, view_id).await?;
        assemble(&manifest, self.blobs.as_ref()).await
    }

    /// Definitions of `name` in the view, each with its path.
    pub async fn graph_definitions(
        &self,
        repo: &str,
        view_id: &str,
        name: &str,
    ) -> Result<Vec<Located<Symbol>>> {
        Ok(self.view(repo, view_id).await?.definitions(name))
    }

    /// References to `name` in the view, each with its path.
    pub async fn graph_references_to(
        &self,
        repo: &str,
        view_id: &str,
        name: &str,
    ) -> Result<Vec<Located<Reference>>> {
        Ok(self.view(repo, view_id).await?.references_to(name))
    }

    /// Enclosing functions that call `name` in the view.
    pub async fn graph_callers_of(
        &self,
        repo: &str,
        view_id: &str,
        name: &str,
    ) -> Result<Vec<String>> {
        Ok(self.view(repo, view_id).await?.callers_of(name))
    }

    /// Names called from within `name` in the view.
    pub async fn graph_callees(
        &self,
        repo: &str,
        view_id: &str,
        name: &str,
    ) -> Result<Vec<String>> {
        Ok(self.view(repo, view_id).await?.callees(name))
    }

    /// Transitive caller closure of `name` (impact of changing it).
    pub async fn graph_impact(&self, repo: &str, view_id: &str, name: &str) -> Result<Vec<String>> {
        Ok(self.view(repo, view_id).await?.impact(name))
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
    use gonzalo_core::{Identity, Meta, RecordKind};
    use gonzalo_graph::build_rust;
    use gonzalo_store_fs::FsStore;
    use gonzalo_ticket_config::Connection;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn fresh_fs() -> Arc<FsStore> {
        Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()))
    }

    /// Store each file's slice blob and a manifest record for `(repo, view)`,
    /// exactly as the sync path would, so the service can assemble the view.
    async fn seed_view(fs: &FsStore, repo: &str, view: &str, files: &[(&str, &str)]) {
        let mut manifest = Manifest::new();
        for (path, src) in files {
            let hash = fs
                .put_blob(&build_rust(src).to_slice_bytes())
                .await
                .unwrap();
            manifest.insert(*path, hash);
        }
        let body = manifest.to_body();
        let record = Record {
            revision: Revision::initial(body.bytes()),
            parent: None,
            body,
            kind: RecordKind::GraphManifest,
            meta: Meta {
                author: Identity::new("tester"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            key: Manifest::key(repo, view),
        };
        let outcome = fs.put(record, None).await.unwrap();
        assert!(matches!(outcome, PutResult::Committed(_)));
    }

    #[tokio::test]
    async fn graph_queries_answer_over_an_assembled_view() {
        let fs = fresh_fs();
        seed_view(
            &fs,
            "r",
            "main",
            &[
                ("src/lib.rs", "fn helper() {}"),
                ("src/main.rs", "fn main() { helper(); }"),
            ],
        )
        .await;
        let svc = Service::new(fs.clone(), fs);

        let defs = svc.graph_definitions("r", "main", "helper").await.unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path, "src/lib.rs");

        assert_eq!(
            svc.graph_callers_of("r", "main", "helper").await.unwrap(),
            vec!["main".to_string()]
        );
        assert_eq!(
            svc.graph_callees("r", "main", "main").await.unwrap(),
            vec!["helper".to_string()]
        );
        assert_eq!(
            svc.graph_impact("r", "main", "helper").await.unwrap(),
            vec!["main".to_string()]
        );
        assert_eq!(
            svc.graph_references_to("r", "main", "helper")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn unknown_view_yields_empty_results() {
        let fs = fresh_fs();
        let svc = Service::new(fs.clone(), fs);
        assert!(
            svc.graph_definitions("r", "absent", "x")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            svc.graph_impact("r", "absent", "x")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn ticket_sync_rejects_unknown_provider() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(FsStore::new(dir.path()));
        let svc = Service::new(fs.clone(), fs);
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
            set_targets: BTreeMap::new(),
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
