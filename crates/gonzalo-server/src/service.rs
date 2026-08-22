//! The transport-agnostic service layer. Both the gRPC and HTTP transports
//! delegate to this; it forwards record ops to the backing `Store` and answers
//! code-graph queries. A view is served from its persistent SQLite graph when
//! one has been indexed (under `graph_root`); otherwise it is assembled from the
//! content-addressed slices on the fly.

use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DeleteResult, KeyPrefix, Manifest, PutResult, Record,
    RecordKey, Result, Revision, Store,
};
use gonzalo_graph::{
    GraphStore, ImpactReport, Located, Page, RankedSymbol, Ranking, Reference, Symbol,
    SymbolFilter, ViewOverview, assemble, resolved_impact,
};
use gonzalo_graph_sqlite::{SqliteGraphStore, view_db_path};
use gonzalo_ticket::IngestSummary;
use gonzalo_ticket_config::Connection;
use std::path::PathBuf;
use std::sync::Arc;

/// One indexed view, as reported by [`Service::graph_views`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ViewSummary {
    pub repo: String,
    pub view_id: String,
    /// Paths in the view's manifest.
    pub files: usize,
    /// The commit this view was last indexed at, when `gonzalo index` recorded
    /// one. Compare against the checkout's HEAD to detect a stale view.
    pub base_commit: Option<String>,
}

/// Wraps a `Store` (records) and a `BlobStore` (content-addressed slices) and
/// exposes their operations to the daemon transports. The daemon backs both
/// with the same `FsStore`.
#[derive(Clone)]
pub struct Service {
    store: Arc<dyn Store>,
    blobs: Arc<dyn BlobStore>,
    /// Root under which per-view SQLite graphs live (`gonzalo index` writes
    /// them). When set and a view's db exists, queries read it instead of
    /// assembling from slices.
    graph_root: Option<PathBuf>,
    /// Ceiling for a single blob over the transports (bytes). Defaults to
    /// `DEFAULT_MAX_BLOB_SIZE`; the daemon may raise it from the environment.
    max_blob_size: usize,
}

impl Service {
    pub fn new(store: Arc<dyn Store>, blobs: Arc<dyn BlobStore>) -> Self {
        Self {
            store,
            blobs,
            graph_root: None,
            max_blob_size: gonzalo_proto::DEFAULT_MAX_BLOB_SIZE,
        }
    }

    /// Override the per-blob size ceiling (bytes) used by the HTTP body limit
    /// and the gRPC decode limit.
    pub fn with_max_blob_size(mut self, n: usize) -> Self {
        self.max_blob_size = n;
        self
    }

    /// The per-blob size ceiling (bytes).
    pub fn max_blob_size(&self) -> usize {
        self.max_blob_size
    }

    // --- Content-addressed blobs (BlobStore over the daemon, gonzalo#184) ---

    pub async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        self.blobs.get_blob(hash).await
    }

    pub async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
        self.blobs.put_blob(content).await
    }

    pub async fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        self.blobs.list_blobs().await
    }

    pub async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
        self.blobs.delete_blob(hash).await
    }

    /// Serve code-graph queries from persistent SQLite graphs rooted at
    /// `graph_root` (matching `gonzalo index`'s `<store_root>/graphs`), falling
    /// back to slice assembly for views without an indexed db.
    pub fn with_graph_root(mut self, graph_root: impl Into<PathBuf>) -> Self {
        self.graph_root = Some(graph_root.into());
        self
    }

    pub async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.store.get(key).await
    }

    /// Readiness probe: whether the backing store is reachable. Does a cheap
    /// point lookup of a sentinel key — `Ok` (even `Ok(None)`) means the store
    /// answered, `Err` means it is unreachable (bad endpoint/bucket, down
    /// backend), so a load balancer should route around this replica. Backs
    /// `GET /readyz`; liveness (`/healthz`) needs no store access.
    pub async fn ready(&self) -> bool {
        self.store
            .get(&RecordKey::new("_gonzalo", "_health", "_probe"))
            .await
            .is_ok()
    }

    pub async fn put(&self, record: Record, expected: Option<Revision>) -> Result<PutResult> {
        self.store.put(record, expected).await
    }

    pub async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        self.store.list(prefix).await
    }

    pub async fn delete(
        &self,
        key: &RecordKey,
        expected: Option<Revision>,
    ) -> Result<DeleteResult> {
        self.store.delete(key, expected).await
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
        // Scope record keys by connection name so the same issue synced from two
        // boards produces two distinct records instead of colliding (#159).
        gonzalo_ticket::ingest(
            source.as_ref(),
            self.store.as_ref(),
            author,
            Some(&conn.name),
        )
        .await
        .map_err(|e| TicketSyncError::Internal(e.to_string()))
    }

    // --- Code graph queries (EPIC C) ---
    // Each query selects a view by `(repo, view_id)` and answers server-side,
    // preferring the view's persistent SQLite graph and falling back to
    // on-the-fly slice assembly.

    /// Load a view's manifest, or [`CoreError::NotFound`] if the view has none.
    ///
    /// This used to return an empty manifest for an unknown view, which made a
    /// misconfigured selector indistinguishable from a query that found nothing
    /// (#210) — a typo in `view_id` produced a confident, wrong "nothing calls
    /// this" rather than an error the caller could notice.
    async fn load_manifest(&self, repo: &str, view_id: &str) -> Result<Manifest> {
        let key = Manifest::key(repo, view_id);
        match self.store.get(&key).await? {
            Some(record) => Manifest::from_body(&record.body),
            None => Err(CoreError::NotFound(key)),
        }
    }

    /// Whether `(repo, view_id)` names a view that exists — a persistent graph
    /// under `graph_root`, or a manifest record in the store.
    pub async fn view_exists(&self, repo: &str, view_id: &str) -> Result<bool> {
        if let Some(root) = &self.graph_root
            && view_db_path(root, repo, view_id).exists()
        {
            return Ok(true);
        }
        Ok(self
            .store
            .get(&Manifest::key(repo, view_id))
            .await?
            .is_some())
    }

    /// Every indexed view, for discovery. Cheap by design — this is the call an
    /// agent makes first, so it reads manifests (and the recorded base commit)
    /// rather than loading each view's graph. Use `overview` for symbol counts.
    pub async fn graph_views(&self) -> Result<Vec<ViewSummary>> {
        let prefix = KeyPrefix {
            namespace: None,
            collection: Some(Manifest::collection().to_string()),
        };
        let mut out = Vec::new();
        for key in self.store.list(&prefix).await? {
            let Some(record) = self.store.get(&key).await? else {
                continue; // listed then removed — skip rather than fail discovery
            };
            let manifest = Manifest::from_body(&record.body)?;
            out.push(ViewSummary {
                files: manifest.entries.len(),
                base_commit: self.recorded_base(&key.namespace, &key.id),
                repo: key.namespace,
                view_id: key.id,
            });
        }
        out.sort_by(|a, b| a.repo.cmp(&b.repo).then_with(|| a.view_id.cmp(&b.view_id)));
        Ok(out)
    }

    /// The commit a view was last indexed at, recorded by `gonzalo index`
    /// alongside the view's graph. Lets a caller detect a stale view by
    /// comparing against the checkout's HEAD — the quieter form of #210, where
    /// results are plausible but describe code that has moved on.
    fn recorded_base(&self, repo: &str, view_id: &str) -> Option<String> {
        let root = self.graph_root.as_ref()?;
        let base = view_db_path(root, repo, view_id).with_extension("base");
        let sha = std::fs::read_to_string(base).ok()?;
        let sha = sha.trim().to_string();
        (!sha.is_empty()).then_some(sha)
    }

    /// A queryable graph for `(repo, view_id)`: the persistent SQLite graph if
    /// one has been indexed under `graph_root`, else assembled from slices.
    ///
    /// Errors with [`CoreError::NotFound`] when the selector names no view.
    async fn view(&self, repo: &str, view_id: &str) -> Result<Box<dyn GraphStore>> {
        if let Some(root) = &self.graph_root {
            let db = view_db_path(root, repo, view_id);
            if db.exists() {
                let store =
                    SqliteGraphStore::open(&db).map_err(|e| CoreError::Backend(e.to_string()))?;
                return Ok(Box::new(store));
            }
        }
        let manifest = self.load_manifest(repo, view_id).await?;
        Ok(Box::new(assemble(&manifest, self.blobs.as_ref()).await?))
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
    /// Symbols transitively affected if `name` changes, following only call
    /// edges that resolve to a specific definition.
    ///
    /// Uses [`resolved_impact`] rather than the name-matched
    /// [`GraphStore::impact`], which merged unrelated subgraphs through shared
    /// identifiers (#207). Edges that cannot be attributed are counted in the
    /// report instead of being traversed or silently dropped.
    pub async fn graph_impact(
        &self,
        repo: &str,
        view_id: &str,
        name: &str,
        max_depth: Option<usize>,
    ) -> Result<ImpactReport> {
        let view = self.view(repo, view_id).await?;
        Ok(resolved_impact(view.as_ref(), name, max_depth))
    }

    /// [`graph_impact`](Self::graph_impact) flattened to distinct caller names.
    ///
    /// The daemon transports speak a shared name-list shape for `callers_of`,
    /// `callees` and `impact`, so they take this projection. They still get the
    /// resolution-gated walk — only the `ambiguous_edges` and per-node paths are
    /// dropped, which is why the MCP surface returns the full report.
    pub async fn graph_impact_names(
        &self,
        repo: &str,
        view_id: &str,
        name: &str,
    ) -> Result<Vec<String>> {
        let report = self.graph_impact(repo, view_id, name, None).await?;
        let mut names: Vec<String> = report.reached.into_iter().map(|n| n.name).collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Aggregate shape of the view: counts, breakdowns by kind and language,
    /// and the `largest` files by symbol count.
    pub async fn graph_overview(
        &self,
        repo: &str,
        view_id: &str,
        largest: usize,
    ) -> Result<ViewOverview> {
        Ok(self.view(repo, view_id).await?.overview(largest))
    }

    /// Top `limit` symbol names in the view by `ranking`.
    pub async fn graph_top(
        &self,
        repo: &str,
        view_id: &str,
        ranking: Ranking,
        limit: usize,
    ) -> Result<Page<RankedSymbol>> {
        Ok(self.view(repo, view_id).await?.top(ranking, limit))
    }

    /// Symbols in the view matching `filter`, bounded by `limit`.
    pub async fn graph_list(
        &self,
        repo: &str,
        view_id: &str,
        filter: &SymbolFilter,
        limit: usize,
    ) -> Result<Page<Located<Symbol>>> {
        Ok(self.view(repo, view_id).await?.list(filter, limit))
    }

    /// Dead-code candidates in the view: symbols with no inbound reference,
    /// heuristic — see [`GraphStore::unreferenced`] for the blind spots.
    pub async fn graph_unreferenced(
        &self,
        repo: &str,
        view_id: &str,
        filter: &SymbolFilter,
        exclude_tests: bool,
        limit: usize,
    ) -> Result<Page<Located<Symbol>>> {
        Ok(self
            .view(repo, view_id)
            .await?
            .unreferenced(filter, exclude_tests, limit))
    }

    /// Structural diff of two views of `repo` (`view_a` → `view_b`): symbols and
    /// references added or removed.
    pub async fn graph_diff(
        &self,
        repo: &str,
        view_a: &str,
        view_b: &str,
    ) -> Result<gonzalo_graph::GraphDiff> {
        let a = self.view(repo, view_a).await?;
        let b = self.view(repo, view_b).await?;
        Ok(gonzalo_graph::diff(a.as_ref(), b.as_ref()))
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
    async fn graph_queries_prefer_the_persistent_sqlite_graph() {
        let dir = tempfile::tempdir().unwrap().keep();
        let fs = Arc::new(FsStore::new(&dir));
        let graph_root = dir.join("graphs");

        // Write ONLY the SQLite graph — no manifest/slices — so a non-empty
        // answer proves the query read SQLite rather than assembling.
        {
            let mut g = SqliteGraphStore::open(view_db_path(&graph_root, "r", "main")).unwrap();
            g.insert(
                "lib.rs",
                build_rust("fn helper() {}\nfn main() { helper(); }"),
            );
        }
        let svc = Service::new(fs.clone(), fs).with_graph_root(graph_root);

        let defs = svc.graph_definitions("r", "main", "helper").await.unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path, "lib.rs");
        assert_eq!(
            svc.graph_callers_of("r", "main", "helper").await.unwrap(),
            vec!["main".to_string()]
        );

        // A view with neither an indexed db nor a manifest is unresolvable, and
        // now says so rather than falling back to an empty assembly (#210).
        assert!(svc.graph_impact("r", "absent", "x", None).await.is_err());
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
            svc.graph_impact_names("r", "main", "helper").await.unwrap(),
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
    async fn graph_diff_reports_changes_between_two_views() {
        let fs = fresh_fs();
        seed_view(&fs, "r", "v1", &[("lib.rs", "fn keep() {}\nfn gone() {}")]).await;
        seed_view(&fs, "r", "v2", &[("lib.rs", "fn keep() {}\nfn fresh() {}")]).await;
        let svc = Service::new(fs.clone(), fs);

        let d = svc.graph_diff("r", "v1", "v2").await.unwrap();
        assert!(d.added_symbols.iter().any(|l| l.item.name == "fresh"));
        assert!(d.removed_symbols.iter().any(|l| l.item.name == "gone"));
        assert!(!d.added_symbols.iter().any(|l| l.item.name == "keep"));
    }

    #[tokio::test]
    async fn unknown_view_is_an_error_not_an_empty_result() {
        // Inverted from `unknown_view_yields_empty_results`: returning `[]` for
        // an unresolvable selector made a typo indistinguishable from a genuine
        // miss, so callers reported "nothing calls this" as fact (#210).
        let fs = fresh_fs();
        let svc = Service::new(fs.clone(), fs);
        for res in [
            svc.graph_definitions("r", "absent", "x").await.err(),
            svc.graph_impact("r", "absent", "x", None).await.err(),
        ] {
            let err = res.expect("an unknown view must error");
            assert!(
                matches!(&err, CoreError::NotFound(k) if k.namespace == "r" && k.id == "absent"),
                "the error must name the selector: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_symbol_in_a_known_view_is_still_empty() {
        // The other half: only the *selector* became an error. A real miss in a
        // real view must stay an ordinary empty result.
        let fs = fresh_fs();
        seed_view(&fs, "r", "main", &[("lib.rs", "fn helper() {}")]).await;
        let svc = Service::new(fs.clone(), fs);
        assert!(
            svc.graph_definitions("r", "main", "nonexistent")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn graph_views_lists_indexed_views() {
        let fs = fresh_fs();
        seed_view(&fs, "r", "main", &[("lib.rs", "fn helper() {}")]).await;
        seed_view(&fs, "other", "dev", &[("a.rs", "fn a() {}")]).await;
        let svc = Service::new(fs.clone(), fs);

        let views = svc.graph_views().await.unwrap();
        let pairs: Vec<(&str, &str)> = views
            .iter()
            .map(|v| (v.repo.as_str(), v.view_id.as_str()))
            .collect();
        assert_eq!(pairs, vec![("other", "dev"), ("r", "main")], "sorted");
        assert_eq!(views[1].files, 1);
    }

    #[tokio::test]
    async fn view_exists_distinguishes_known_from_unknown() {
        let fs = fresh_fs();
        seed_view(&fs, "r", "main", &[("lib.rs", "fn helper() {}")]).await;
        let svc = Service::new(fs.clone(), fs);
        assert!(svc.view_exists("r", "main").await.unwrap());
        assert!(!svc.view_exists("r", "mian").await.unwrap());
        assert!(!svc.view_exists("nope", "main").await.unwrap());
    }

    #[tokio::test]
    async fn blob_methods_delegate_and_default_size_is_64_mib() {
        let fs = fresh_fs();
        let svc = Service::new(fs.clone(), fs);
        assert_eq!(svc.max_blob_size(), gonzalo_proto::DEFAULT_MAX_BLOB_SIZE);

        let hash = svc.put_blob(b"checkpoint pre-image").await.unwrap();
        assert_eq!(hash, gonzalo_core::ContentHash::of(b"checkpoint pre-image"));
        assert_eq!(
            svc.get_blob(&hash).await.unwrap().as_deref(),
            Some(&b"checkpoint pre-image"[..])
        );
        assert_eq!(svc.list_blobs().await.unwrap(), vec![hash.clone()]);
        svc.delete_blob(&hash).await.unwrap();
        assert_eq!(svc.get_blob(&hash).await.unwrap(), None);

        let tuned = Service::new(fresh_fs(), fresh_fs()).with_max_blob_size(123);
        assert_eq!(tuned.max_blob_size(), 123);
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
