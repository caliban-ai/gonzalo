//! HTTP/JSON transport over the shared `Service`, using axum.

use crate::Service;
use crate::auth::{Access, Auth, Principal};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{Next, from_fn},
    response::{IntoResponse, Response},
    routing::get,
};
use gonzalo_core::{Identity, KeyPrefix, PutResult, RecordKey};
use gonzalo_proto::http::{PutBody, PutOutcome};
use serde::Deserialize;
use std::sync::Arc;

/// Build the axum router. `auth` governs per-namespace authorization (ADR 0015);
/// `Auth::Disabled` serves open. The middleware authenticates every non-probe
/// request (bearer → [`Principal`], or `401`) and hands the principal to the
/// handlers, which authorize against the target namespace.
pub fn router(service: Service, auth: Arc<Auth>) -> Router {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route(
            "/v1/records/{ns}/{col}/{id}",
            get(get_record).put(put_record),
        )
        .route("/v1/keys", get(list_keys))
        .route("/v1/tickets/sync", axum::routing::post(ticket_sync))
        .route("/v1/graph/definitions", get(graph_definitions))
        .route("/v1/graph/references", get(graph_references_to))
        .route("/v1/graph/callers", get(graph_callers_of))
        .route("/v1/graph/callees", get(graph_callees))
        .route("/v1/graph/impact", get(graph_impact))
        .with_state(Arc::new(service));
    app.layer(from_fn(move |mut req: Request, next: Next| {
        let auth = auth.clone();
        async move {
            // Health/readiness probes are unauthenticated: k8s liveness and
            // readiness checks carry no bearer token, and a probe gated behind
            // auth would fail closed and get the pod killed.
            if is_probe_path(req.uri().path()) {
                return next.run(req).await;
            }
            match auth.authenticate(bearer(req.headers())) {
                Some(principal) => {
                    req.extensions_mut().insert(principal);
                    next.run(req).await
                }
                None => StatusCode::UNAUTHORIZED.into_response(),
            }
        }
    }))
}

/// `403` when a principal lacks the required access on a namespace.
fn forbidden(principal: &Principal, access: Access, namespace: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        format!(
            "principal {:?} lacks {access:?} on namespace {namespace:?}",
            principal.name()
        ),
    )
        .into_response()
}

/// Paths served without authentication (k8s probes).
fn is_probe_path(path: &str) -> bool {
    path == "/healthz" || path == "/readyz"
}

/// Liveness: the process is up and serving. No store access — a `/healthz` that
/// touched the store would conflate liveness with readiness and kill a pod that
/// is merely waiting on its backend.
async fn healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// Readiness: `200` when the backing store is reachable, `503` otherwise, so a
/// load balancer only routes to replicas that can actually serve.
async fn readyz(State(svc): State<Arc<Service>>) -> Response {
    if svc.ready().await {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
    }
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn server_error<E: std::fmt::Display>(e: E) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

async fn get_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path((ns, col, id)): Path<(String, String, String)>,
) -> Response {
    if !principal.allows(Access::Read, &ns) {
        return forbidden(&principal, Access::Read, &ns);
    }
    match svc.get(&RecordKey::new(ns, col, id)).await {
        Ok(Some(rec)) => (StatusCode::OK, Json(rec)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => server_error(e),
    }
}

async fn put_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Json(mut body): Json<PutBody>,
) -> Response {
    let ns = &body.record.key.namespace;
    if !principal.allows(Access::Write, ns) {
        return forbidden(&principal, Access::Write, &ns.clone());
    }
    // Stamp the author from the authenticated principal — unforgeable (ADR
    // 0015). Open mode (no auth) leaves the record's author untouched.
    if principal.is_authenticated() {
        body.record.meta.author = Identity::new(principal.name());
    }
    match svc.put(body.record, body.expected).await {
        Ok(PutResult::Committed(revision)) => {
            (StatusCode::OK, Json(PutOutcome::Committed { revision })).into_response()
        }
        Ok(PutResult::Conflict(conflict)) => (
            StatusCode::CONFLICT,
            Json(PutOutcome::Conflict { conflict }),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    namespace: Option<String>,
    collection: Option<String>,
}

async fn list_keys(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<ListQuery>,
) -> Response {
    // No namespace → spans all → requires admin (`read` on `"*"`).
    let ns = q.namespace.as_deref().unwrap_or("*");
    if !principal.allows(Access::Read, ns) {
        return forbidden(&principal, Access::Read, ns);
    }
    let prefix = KeyPrefix {
        namespace: q.namespace,
        collection: q.collection,
    };
    match svc.list(&prefix).await {
        Ok(keys) => (StatusCode::OK, Json(keys)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn ticket_sync(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Json(conn): Json<gonzalo_ticket_config::Connection>,
) -> Response {
    // Ticket sync writes records in the `tickets` namespace.
    if !principal.allows(Access::Write, "tickets") {
        return forbidden(&principal, Access::Write, "tickets");
    }
    match svc.ticket_sync(&conn, "gonzalod").await {
        Ok(summary) => (StatusCode::OK, Json(summary)).into_response(),
        Err(crate::service::TicketSyncError::BadRequest(m)) => {
            (StatusCode::BAD_REQUEST, m).into_response()
        }
        Err(crate::service::TicketSyncError::Internal(m)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, m).into_response()
        }
    }
}

/// Selects a code-graph view `(repo, view)` and the `name` a query is about,
/// e.g. `GET /v1/graph/impact?repo=acme/widgets&view=main&name=helper`.
#[derive(Deserialize)]
struct GraphQuery {
    repo: String,
    view: String,
    name: String,
}

/// Graph queries read the view for `repo`, whose records live in the `repo`
/// namespace — so they require `read` on `repo`.
fn graph_authz(principal: &Principal, repo: &str) -> Option<Response> {
    (!principal.allows(Access::Read, repo)).then(|| forbidden(principal, Access::Read, repo))
}

async fn graph_definitions(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<GraphQuery>,
) -> Response {
    if let Some(denied) = graph_authz(&principal, &q.repo) {
        return denied;
    }
    match svc.graph_definitions(&q.repo, &q.view, &q.name).await {
        Ok(items) => (StatusCode::OK, Json(items)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_references_to(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<GraphQuery>,
) -> Response {
    if let Some(denied) = graph_authz(&principal, &q.repo) {
        return denied;
    }
    match svc.graph_references_to(&q.repo, &q.view, &q.name).await {
        Ok(items) => (StatusCode::OK, Json(items)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_callers_of(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<GraphQuery>,
) -> Response {
    if let Some(denied) = graph_authz(&principal, &q.repo) {
        return denied;
    }
    match svc.graph_callers_of(&q.repo, &q.view, &q.name).await {
        Ok(names) => (StatusCode::OK, Json(names)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_callees(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<GraphQuery>,
) -> Response {
    if let Some(denied) = graph_authz(&principal, &q.repo) {
        return denied;
    }
    match svc.graph_callees(&q.repo, &q.view, &q.name).await {
        Ok(names) => (StatusCode::OK, Json(names)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_impact(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<GraphQuery>,
) -> Response {
    if let Some(denied) = graph_authz(&principal, &q.repo) {
        return denied;
    }
    match svc.graph_impact(&q.repo, &q.view, &q.name).await {
        Ok(names) => (StatusCode::OK, Json(names)).into_response(),
        Err(e) => server_error(e),
    }
}

/// Serve HTTP/JSON on an already-bound listener until the process ends.
pub async fn serve_http(
    listener: tokio::net::TcpListener,
    service: Service,
    auth: Arc<Auth>,
) -> std::io::Result<()> {
    axum::serve(listener, router(service, auth)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use gonzalo_core::{CoreError, Record, Result as CoreResult, Revision, Store};
    use gonzalo_store_fs::FsStore;
    use tempfile::TempDir;
    use tower::ServiceExt; // oneshot

    /// A Service backed by a fresh filesystem store (reachable → ready).
    fn fs_service() -> (Service, TempDir) {
        let dir = TempDir::new().unwrap();
        let fs = Arc::new(FsStore::new(dir.path()));
        (Service::new(fs.clone(), fs), dir)
    }

    fn open() -> Arc<Auth> {
        Arc::new(Auth::Disabled)
    }

    /// An `Enabled` registry with one admin token.
    fn admin_token(token: &str) -> Arc<Auth> {
        Arc::new(Auth::Enabled(std::collections::HashMap::from([(
            token.to_string(),
            Principal::admin("admin"),
        )])))
    }

    async fn status_of(service: Service, auth: Arc<Auth>, path: &str) -> StatusCode {
        router(service, auth)
            .oneshot(
                HttpRequest::builder()
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let (svc, _dir) = fs_service();
        assert_eq!(status_of(svc, open(), "/healthz").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_ok_when_store_reachable() {
        let (svc, _dir) = fs_service();
        assert_eq!(status_of(svc, open(), "/readyz").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn probes_bypass_auth_but_other_routes_do_not() {
        let (svc, _d1) = fs_service();
        assert_eq!(
            status_of(svc, admin_token("secret"), "/healthz").await,
            StatusCode::OK,
            "healthz must not require a token"
        );
        let (svc, _d2) = fs_service();
        assert_eq!(
            status_of(svc, admin_token("secret"), "/readyz").await,
            StatusCode::OK,
            "readyz must not require a token"
        );
        // A normal route without the token is still rejected.
        let (svc, _d3) = fs_service();
        assert_eq!(
            status_of(svc, admin_token("secret"), "/v1/keys").await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// A store whose every operation fails — models an unreachable backend.
    struct DownStore;

    #[async_trait::async_trait]
    impl Store for DownStore {
        async fn get(&self, _key: &RecordKey) -> CoreResult<Option<Record>> {
            Err(CoreError::Backend("store unreachable".into()))
        }
        async fn put(&self, _record: Record, _expected: Option<Revision>) -> CoreResult<PutResult> {
            Err(CoreError::Backend("store unreachable".into()))
        }
        async fn list(&self, _prefix: &KeyPrefix) -> CoreResult<Vec<RecordKey>> {
            Err(CoreError::Backend("store unreachable".into()))
        }
    }

    // --- namespace-scoped auth (ADR 0015) ---

    /// `writer` scoped to `memory`, plus an `admin`.
    fn scoped() -> Arc<Auth> {
        Arc::new(Auth::Enabled(std::collections::HashMap::from([
            (
                "wtok".to_string(),
                Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
            ),
            ("atok".to_string(), Principal::admin("admin")),
        ])))
    }

    async fn call(
        service: Service,
        auth: Arc<Auth>,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> (StatusCode, Vec<u8>) {
        let mut b = HttpRequest::builder().method(method).uri(path);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let req = b
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .unwrap();
        let resp = router(service, auth).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, bytes)
    }

    fn put_body(namespace: &str, author: &str) -> Vec<u8> {
        let record = Record {
            revision: Revision::initial(b"{}"),
            parent: None,
            body: gonzalo_core::Body::Inline(b"{}".to_vec()),
            kind: gonzalo_core::RecordKind::MemoryTier,
            meta: gonzalo_core::Meta {
                author: gonzalo_core::Identity::new(author),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: std::collections::BTreeMap::new(),
            },
            links: Vec::new(),
            key: RecordKey::new(namespace, "col", "x"),
        };
        serde_json::to_vec(&PutBody {
            record,
            expected: None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn read_scope_and_missing_token() {
        let (svc, _d) = fs_service();
        // In-scope read of an absent record: authorized → 404 (not 401/403).
        let (s, _) = call(
            svc,
            scoped(),
            "GET",
            "/v1/records/memory/col/x",
            Some("wtok"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        let (svc, _d) = fs_service();
        // Out-of-scope read → 403.
        let (s, _) = call(
            svc,
            scoped(),
            "GET",
            "/v1/records/secrets/col/x",
            Some("wtok"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        let (svc, _d) = fs_service();
        // No token → 401.
        let (s, _) = call(svc, scoped(), "GET", "/v1/records/memory/col/x", None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn write_scope_and_author_stamping() {
        // Out-of-scope write → 403.
        let (svc, _d) = fs_service();
        let (s, _) = call(
            svc,
            scoped(),
            "PUT",
            "/v1/records/secrets/col/x",
            Some("wtok"),
            Some(put_body("secrets", "writer")),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // In-scope write commits and stamps the authenticated principal over the
        // client-claimed "forged" author.
        let (svc, dir) = fs_service();
        let auth = scoped();
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            "/v1/records/memory/col/x",
            Some("wtok"),
            Some(put_body("memory", "forged")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let _ = dir;

        let (s, body) = call(
            svc,
            auth,
            "GET",
            "/v1/records/memory/col/x",
            Some("wtok"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let record: Record = serde_json::from_slice(&body).unwrap();
        assert_eq!(record.meta.author, gonzalo_core::Identity::new("writer"));
    }

    #[tokio::test]
    async fn list_without_namespace_requires_admin() {
        let (svc, _d) = fs_service();
        let (s, _) = call(svc, scoped(), "GET", "/v1/keys", Some("wtok"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        let (svc, _d) = fs_service();
        let (s, _) = call(svc, scoped(), "GET", "/v1/keys", Some("atok"), None).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_503_when_store_unreachable() {
        let dir = TempDir::new().unwrap();
        // Records via the down store; blobs via fs (readiness only probes records).
        let blobs = Arc::new(FsStore::new(dir.path()));
        let svc = Service::new(Arc::new(DownStore), blobs);
        assert_eq!(
            status_of(svc, open(), "/readyz").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
