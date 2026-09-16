//! HTTP/JSON transport over the shared `Service`, using axum.

use crate::Service;
use crate::auth::{Access, Auth, Principal};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{Next, from_fn},
    response::{IntoResponse, Response},
    routing::get,
};
use gonzalo_core::{
    ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, PutResult, RecordKey,
};
use gonzalo_proto::http::{
    DeleteBody, DeleteOutcome, PurgeBody, PutBody, PutOutcome, RawRecordBody,
};
use serde::Deserialize;
use std::sync::Arc;

/// Blobs are namespace-agnostic; they authorize against this reserved
/// namespace (ADR 0015). Admins (`*`) and open mode cover it; a scoped
/// principal is granted blob access by listing `_blobs` in its read/write set.
const BLOB_NS: &str = "_blobs";

/// Build the axum router. `auth` governs per-namespace authorization (ADR 0015);
/// `Auth::Disabled` serves open. The middleware authenticates every non-probe
/// request (bearer → [`Principal`], or `401`) and hands the principal to the
/// handlers, which authorize against the target namespace.
pub fn router(service: Service, auth: Arc<Auth>) -> Router {
    let max_blob = service.max_blob_size();
    let blob_routes = Router::new()
        .route(
            "/v1/blobs/{hash}",
            get(get_blob).put(put_blob).delete(delete_blob),
        )
        .route("/v1/blobs", get(list_blobs))
        .layer(DefaultBodyLimit::max(max_blob));

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route(
            "/v1/records/{ns}/{col}/{id}",
            get(get_record).put(put_record).delete(delete_record),
        )
        .route("/v1/keys", get(list_keys))
        // Replication surface (gonzalo#203): tombstones visible, purge is admin.
        .route(
            "/v1/raw/records/{ns}/{col}/{id}",
            get(get_raw_record).put(put_raw_record),
        )
        .route("/v1/raw/keys", get(list_raw_keys))
        .route(
            "/v1/purge/{ns}/{col}/{id}",
            axum::routing::post(purge_record),
        )
        .route("/v1/tickets/sync", axum::routing::post(ticket_sync))
        .route("/v1/graph/definitions", get(graph_definitions))
        .route("/v1/graph/references", get(graph_references_to))
        .route("/v1/graph/callers", get(graph_callers_of))
        .route("/v1/graph/callees", get(graph_callees))
        .route("/v1/graph/impact", get(graph_impact))
        .merge(blob_routes)
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

/// `403` when an operation requires an admin (`read` and `write` on `"*"`).
fn forbidden_admin(principal: &Principal, operation: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        format!(
            "principal {:?} is not an admin; {operation} requires admin",
            principal.name()
        ),
    )
        .into_response()
}

/// `400` when the URL path and the body's record key disagree (#158), else
/// `None`. Shared by `PUT /v1/records/...` and `PUT /v1/raw/records/...`.
fn path_key_mismatch(path: &(String, String, String), key: &RecordKey) -> Option<Response> {
    let (ns, col, id) = path;
    (*ns != key.namespace || *col != key.collection || *id != key.id).then(|| {
        (
            StatusCode::BAD_REQUEST,
            "URL path does not match record key",
        )
            .into_response()
    })
}

/// `200` + `Committed`, `409` + `Conflict`, `412` when `expected` names a
/// revision the store does not hold (`CoreError::NotFound`), or an opaque
/// `500`. `412` rather than `404`: on the raw routes a `404` means "old daemon".
/// Shared by both put routes.
fn put_outcome_response(result: gonzalo_core::Result<PutResult>) -> Response {
    match result {
        Ok(PutResult::Committed(revision)) => {
            (StatusCode::OK, Json(PutOutcome::Committed { revision })).into_response()
        }
        Ok(PutResult::Conflict(conflict)) => (
            StatusCode::CONFLICT,
            Json(PutOutcome::Conflict { conflict }),
        )
            .into_response(),
        Err(CoreError::NotFound(key)) => (
            StatusCode::PRECONDITION_FAILED,
            format!("record not found: {key}"),
        )
            .into_response(),
        // The caller asked for something the store will never accept, such as a
        // consumer put of a tombstone. Retrying is pointless, so say so with a
        // 400 instead of hiding it behind an opaque 500 (gonzalo#299).
        Err(CoreError::Invalid(reason)) => (StatusCode::BAD_REQUEST, reason).into_response(),
        Err(e) => server_error(e),
    }
}

/// `200` + `Deleted`, `409` + `Conflict`, or an opaque `500`. Shared by
/// `DELETE /v1/records/...` and `POST /v1/purge/...`.
fn delete_outcome_response(result: gonzalo_core::Result<DeleteResult>) -> Response {
    match result {
        Ok(DeleteResult::Deleted) => (StatusCode::OK, Json(DeleteOutcome::Deleted)).into_response(),
        Ok(DeleteResult::Conflict(conflict)) => (
            StatusCode::CONFLICT,
            Json(DeleteOutcome::Conflict { conflict }),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
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

/// Map a backend failure to an opaque `500`. The full error is logged
/// server-side; the client sees only "internal error" so on-disk graph paths
/// (`view_db_path`), SQLite text, and S3 endpoint/bucket detail never leak to
/// the network (#148).
fn server_error<E: std::fmt::Display>(e: E) -> Response {
    eprintln!("gonzalod: internal error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
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
    Path(path): Path<(String, String, String)>,
    Json(mut body): Json<PutBody>,
) -> Response {
    // The URL path addresses the record; the body must agree with it. Without
    // this check the path is decorative and authz/write key off the body alone,
    // so a path-based proxy control could be bypassed by a mismatched body
    // (#158). Reject the disagreement with 400 before any authz or write.
    if let Some(bad) = path_key_mismatch(&path, &body.record.key) {
        return bad;
    }
    let ns = &body.record.key.namespace;
    if !principal.allows(Access::Write, ns) {
        return forbidden(&principal, Access::Write, &ns.clone());
    }
    // Stamp the author from the authenticated principal — unforgeable (ADR
    // 0015). Open mode (no auth) leaves the record's author untouched.
    if principal.is_authenticated() {
        body.record.meta.author = Identity::new(principal.name());
    }
    put_outcome_response(svc.put(body.record, body.expected).await)
}

/// The URL path addresses the record; the OCC precondition and an optional
/// claimed deleter identity ride in an optional JSON body. Authorize `Write`
/// on the path's namespace, then delegate — the key is taken from the path,
/// so there is no body-key-vs-path check to make.
///
/// The tombstone's author follows `Principal::delete_author` (gonzalo#203,
/// mirrors `put_raw`'s authorship rule): a non-admin is always stamped as
/// itself and any claimed author in the body is ignored; an admin or open
/// mode may name the deleter via the body's `author` field; and otherwise an
/// authenticated admin is stamped as itself while open mode keeps the prior
/// author (no identity to stamp, ADR 0015).
async fn delete_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path((ns, col, id)): Path<(String, String, String)>,
    body: Option<Json<DeleteBody>>,
) -> Response {
    if !principal.allows(Access::Write, &ns) {
        return forbidden(&principal, Access::Write, &ns);
    }
    let (expected, claimed) = body
        .map(|Json(b)| (b.expected, b.author))
        .unwrap_or_default();
    let author = principal.delete_author(claimed);
    let key = RecordKey::new(ns, col, id);
    delete_outcome_response(svc.delete_as(&key, expected, author).await)
}

#[derive(Deserialize)]
struct ListQuery {
    namespace: Option<String>,
    collection: Option<String>,
}

/// Authorize a key listing (`read` on the namespace; `read` on `"*"` when
/// unscoped) and build its prefix. Shared by `/v1/keys` and `/v1/raw/keys`.
// `Response` is large (axum::http::Response<Body>); boxing it isn't worth it
// for an error path returned at most once per request (D12).
#[allow(clippy::result_large_err)]
fn list_prefix(principal: &Principal, q: ListQuery) -> Result<KeyPrefix, Response> {
    // No namespace → spans all → requires admin (`read` on `"*"`).
    let ns = q.namespace.as_deref().unwrap_or("*");
    if !principal.allows(Access::Read, ns) {
        return Err(forbidden(principal, Access::Read, ns));
    }
    Ok(KeyPrefix {
        namespace: q.namespace,
        collection: q.collection,
    })
}

async fn list_keys(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<ListQuery>,
) -> Response {
    let prefix = match list_prefix(&principal, q) {
        Ok(p) => p,
        Err(denied) => return denied,
    };
    match svc.list(&prefix).await {
        Ok(keys) => (StatusCode::OK, Json(keys)).into_response(),
        Err(e) => server_error(e),
    }
}

/// `GET /v1/raw/records/{ns}/{col}/{id}` — replication read that includes
/// tombstones (gonzalo#203). Always `200` with a [`RawRecordBody`]; absence is
/// `{"record": null}` so `404` unambiguously means "no such route".
async fn get_raw_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path((ns, col, id)): Path<(String, String, String)>,
) -> Response {
    if !principal.allows(Access::Read, &ns) {
        return forbidden(&principal, Access::Read, &ns);
    }
    match svc.get_raw(&RecordKey::new(ns, col, id)).await {
        Ok(record) => (StatusCode::OK, Json(RawRecordBody { record })).into_response(),
        Err(e) => server_error(e),
    }
}

/// `PUT /v1/raw/records/{ns}/{col}/{id}` with a [`PutBody`] — replication write
/// (gonzalo#203). The store writes the record verbatim (revision and
/// tombstones included). Authorization is `Write` on the namespace, with the
/// same path/body agreement check as `put` (#158).
///
/// **Authorship stays unforgeable (ADR 0015).** An admin token is the
/// replication credential: daemon-to-daemon and operator replication run with
/// one, so an admin's raw write keeps the replicated record's original
/// `meta.author`. Any other principal is restamped exactly as `put` restamps,
/// so a namespace writer cannot forge authorship through the raw route. Open
/// mode's implicit principal is an admin, so it keeps the author too.
async fn put_raw_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<(String, String, String)>,
    Json(mut body): Json<PutBody>,
) -> Response {
    if let Some(bad) = path_key_mismatch(&path, &body.record.key) {
        return bad;
    }
    let ns = &body.record.key.namespace;
    if !principal.allows(Access::Write, ns) {
        return forbidden(&principal, Access::Write, &ns.clone());
    }
    if !principal.is_admin() {
        body.record.meta.author = Identity::new(principal.name());
    }
    put_outcome_response(svc.put_raw(body.record, body.expected).await)
}

/// `GET /v1/raw/keys?namespace=&collection=` — replication list that includes
/// tombstoned keys. Same authorization as `/v1/keys`: no namespace spans all
/// namespaces and requires `read` on `"*"`.
async fn list_raw_keys(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<ListQuery>,
) -> Response {
    let prefix = match list_prefix(&principal, q) {
        Ok(p) => p,
        Err(denied) => return denied,
    };
    match svc.list_raw(&prefix).await {
        Ok(keys) => (StatusCode::OK, Json(keys)).into_response(),
        Err(e) => server_error(e),
    }
}

/// `POST /v1/purge/{ns}/{col}/{id}` with a [`PurgeBody`] — physically remove the
/// record iff its revision is `expected` (gonzalo#203). **Admin only**: an early
/// purge is data loss that surfaces later on another machine (spec §5.2). The
/// body is parsed only after the admin check (#146).
async fn purge_record(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path((ns, col, id)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    if !principal.is_admin() {
        return forbidden_admin(&principal, "purge");
    }
    let PurgeBody { expected } = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid purge body: {e}")).into_response();
        }
    };
    let key = RecordKey::new(ns, col, id);
    delete_outcome_response(svc.purge(&key, expected).await)
}

/// `GET /v1/blobs/{hash}` — raw blob bytes, or `404`. Authorized `Read` on the
/// reserved `_blobs` namespace.
async fn get_blob(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path(hash): Path<String>,
) -> Response {
    if !principal.allows(Access::Read, BLOB_NS) {
        return forbidden(&principal, Access::Read, BLOB_NS);
    }
    match svc.get_blob(&ContentHash(hash)).await {
        Ok(Some(bytes)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => server_error(e),
    }
}

/// `PUT /v1/blobs/{hash}` — store raw body content, write-if-absent. The server
/// recomputes the content hash and rejects a mismatch with the URL `{hash}`
/// (`400`) before writing, so the address is authoritative. Authorized `Write`
/// on `_blobs`. A body over `max_blob_size` is rejected upstream as `413` by the
/// route's `DefaultBodyLimit`.
async fn put_blob(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path(hash): Path<String>,
    body: Bytes,
) -> Response {
    if !principal.allows(Access::Write, BLOB_NS) {
        return forbidden(&principal, Access::Write, BLOB_NS);
    }
    let computed = ContentHash::of(&body);
    if computed.0 != hash {
        return (
            StatusCode::BAD_REQUEST,
            "blob content does not match the URL hash",
        )
            .into_response();
    }
    match svc.put_blob(&body).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => server_error(e),
    }
}

/// `DELETE /v1/blobs/{hash}` — idempotent delete. Authorized `Write` on `_blobs`.
async fn delete_blob(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
    Path(hash): Path<String>,
) -> Response {
    if !principal.allows(Access::Write, BLOB_NS) {
        return forbidden(&principal, Access::Write, BLOB_NS);
    }
    match svc.delete_blob(&ContentHash(hash)).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => server_error(e),
    }
}

/// `GET /v1/blobs` — JSON array of every stored blob hash. Authorized `Read` on
/// `_blobs`.
async fn list_blobs(
    State(svc): State<Arc<Service>>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if !principal.allows(Access::Read, BLOB_NS) {
        return forbidden(&principal, Access::Read, BLOB_NS);
    }
    match svc.list_blobs().await {
        Ok(hashes) => (StatusCode::OK, Json(hashes)).into_response(),
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
        // A misconfigured request is the caller's own input → safe to echo. An
        // internal failure goes through `server_error` so its detail is logged,
        // not leaked (#148).
        Err(crate::service::TicketSyncError::BadRequest(m)) => {
            (StatusCode::BAD_REQUEST, m).into_response()
        }
        Err(crate::service::TicketSyncError::Internal(m)) => server_error(m),
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
    match svc.graph_impact_names(&q.repo, &q.view, &q.name).await {
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

    fn down() -> CoreError {
        CoreError::Backend("store unreachable".into())
    }

    #[async_trait::async_trait]
    impl Store for DownStore {
        async fn get(&self, _key: &RecordKey) -> CoreResult<Option<Record>> {
            Err(down())
        }
        async fn put(&self, _record: Record, _expected: Option<Revision>) -> CoreResult<PutResult> {
            Err(down())
        }
        async fn list(&self, _prefix: &KeyPrefix) -> CoreResult<Vec<RecordKey>> {
            Err(down())
        }
        async fn delete_as(
            &self,
            _key: &RecordKey,
            _expected: Option<Revision>,
            _author: Option<Identity>,
        ) -> CoreResult<DeleteResult> {
            Err(down())
        }
        async fn get_raw(&self, _key: &RecordKey) -> CoreResult<Option<Record>> {
            Err(down())
        }
        async fn list_raw(&self, _prefix: &KeyPrefix) -> CoreResult<Vec<RecordKey>> {
            Err(down())
        }
        async fn put_raw(
            &self,
            _record: Record,
            _expected: Option<Revision>,
        ) -> CoreResult<PutResult> {
            Err(down())
        }
        async fn purge(&self, _key: &RecordKey, _expected: Revision) -> CoreResult<DeleteResult> {
            Err(down())
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
            ancestors: Vec::new(),
            deleted_at: None,
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
    async fn put_rejects_url_path_body_key_mismatch() {
        // Body key is memory/col/x; URL path addresses .../col/y — a disagreement
        // that must be rejected with 400 before any authz or write (#158).
        let (svc, _d) = fs_service();
        let (s, _) = call(
            svc,
            scoped(),
            "PUT",
            "/v1/records/memory/col/y",
            Some("wtok"),
            Some(put_body("memory", "writer")),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // A matching path still commits.
        let (svc, _d) = fs_service();
        let (s, _) = call(
            svc,
            scoped(),
            "PUT",
            "/v1/records/memory/col/x",
            Some("wtok"),
            Some(put_body("memory", "writer")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn consumer_put_of_a_tombstone_is_a_bad_request() {
        // Reading a record through a raw route and writing it back through the
        // consumer route is an easy mistake. The store refuses it, and that
        // refusal must read as the caller's error, not an outage (#299).
        let (svc, _d) = fs_service();
        let mut record = record_at("writer", Revision::initial(b"{}"));
        record.kind = gonzalo_core::RecordKind::Tombstone;
        record.deleted_at = Some(1);
        let (s, body) = call(
            svc,
            scoped(),
            "PUT",
            "/v1/records/memory/col/x",
            Some("wtok"),
            Some(put_body_with(record, None)),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let body = String::from_utf8(body).unwrap();
        assert!(
            body.contains("delete_as"),
            "should name the API to use: {body}"
        );
    }

    #[tokio::test]
    async fn backend_error_is_opaque() {
        // A forced backend failure yields an opaque 500 body — the leaky
        // "store unreachable" detail never reaches the client (#148).
        let dir = TempDir::new().unwrap();
        let blobs = Arc::new(FsStore::new(dir.path()));
        let svc = Service::new(Arc::new(DownStore), blobs);
        let (s, body) = call(
            svc,
            scoped(),
            "GET",
            "/v1/records/memory/col/x",
            Some("wtok"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        let text = String::from_utf8(body).unwrap();
        assert_eq!(text, "internal error");
        assert!(!text.contains("unreachable"));
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

    // --- blob routes (#184) ---

    #[tokio::test]
    async fn blob_put_get_list_delete_roundtrip_open() {
        let (svc, _d) = fs_service();
        let auth = open();
        let content = b"remote blob body".to_vec();
        let hash = gonzalo_core::ContentHash::of(&content).0;

        // PUT the blob at its hash-addressed URL.
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            &format!("/v1/blobs/{hash}"),
            None,
            Some(content.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        // GET returns the raw bytes.
        let (s, body) = call(
            svc.clone(),
            auth.clone(),
            "GET",
            &format!("/v1/blobs/{hash}"),
            None,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body, content);

        // LIST reports the hash.
        let (s, body) = call(svc.clone(), auth.clone(), "GET", "/v1/blobs", None, None).await;
        assert_eq!(s, StatusCode::OK);
        let hashes: Vec<gonzalo_core::ContentHash> = serde_json::from_slice(&body).unwrap();
        assert_eq!(hashes, vec![gonzalo_core::ContentHash::of(&content)]);

        // DELETE removes it; a follow-up GET is 404.
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "DELETE",
            &format!("/v1/blobs/{hash}"),
            None,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(svc, auth, "GET", &format!("/v1/blobs/{hash}"), None, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn blob_put_rejects_hash_mismatch_with_400() {
        let (svc, _d) = fs_service();
        // Address the PUT with a hash that does NOT match the body.
        let wrong = gonzalo_core::ContentHash::of(b"a different thing").0;
        let (s, _) = call(
            svc,
            open(),
            "PUT",
            &format!("/v1/blobs/{wrong}"),
            None,
            Some(b"actual body".to_vec()),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn blob_put_over_limit_is_413() {
        let dir = tempfile::TempDir::new().unwrap();
        let fs = Arc::new(FsStore::new(dir.path()));
        // A tiny limit so a small body trips it.
        let svc = Service::new(fs.clone(), fs).with_max_blob_size(8);
        let big = vec![b'x'; 64];
        let hash = gonzalo_core::ContentHash::of(&big).0;
        let (s, _) = call(
            svc,
            open(),
            "PUT",
            &format!("/v1/blobs/{hash}"),
            None,
            Some(big),
        )
        .await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn blob_ops_require_blobs_namespace_scope() {
        // `scoped()` grants read/write on `memory` only — not `_blobs`.
        let (svc, _d) = fs_service();
        let content = b"scoped blob".to_vec();
        let hash = gonzalo_core::ContentHash::of(&content).0;

        // Write without `_blobs` scope → 403.
        let (s, _) = call(
            svc.clone(),
            scoped(),
            "PUT",
            &format!("/v1/blobs/{hash}"),
            Some("wtok"),
            Some(content.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // Read without `_blobs` scope → 403.
        let (s, _) = call(
            svc.clone(),
            scoped(),
            "GET",
            "/v1/blobs",
            Some("wtok"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // Admin (wildcard) may write then read.
        let (s, _) = call(
            svc.clone(),
            scoped(),
            "PUT",
            &format!("/v1/blobs/{hash}"),
            Some("atok"),
            Some(content),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(svc, scoped(), "GET", "/v1/blobs", Some("atok"), None).await;
        assert_eq!(s, StatusCode::OK);
    }

    // --- replication surface: raw reads/writes, purge, delete stamping (#203) ---

    /// `reader` reads `memory` only; `writer` reads and writes `memory`;
    /// `admin` is `*`/`*`.
    fn tomb_auth() -> Arc<Auth> {
        Arc::new(Auth::Enabled(std::collections::HashMap::from([
            (
                "rtok".to_string(),
                Principal::new("reader", vec!["memory".into()], vec![]),
            ),
            (
                "wtok".to_string(),
                Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
            ),
            ("atok".to_string(), Principal::admin("admin")),
        ])))
    }

    const LIVE: &str = "/v1/records/memory/col/x";
    const RAW: &str = "/v1/raw/records/memory/col/x";
    const PURGE: &str = "/v1/purge/memory/col/x";

    /// A record at memory/col/x with `author` and `revision`.
    fn record_at(author: &str, revision: Revision) -> Record {
        Record {
            revision,
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
            ancestors: Vec::new(),
            deleted_at: None,
            key: RecordKey::new("memory", "col", "x"),
        }
    }

    fn put_body_with(record: Record, expected: Option<Revision>) -> Vec<u8> {
        serde_json::to_vec(&PutBody { record, expected }).unwrap()
    }

    fn delete_body() -> Vec<u8> {
        serde_json::to_vec(&DeleteBody::default()).unwrap()
    }

    /// A DELETE body claiming `author` as the deleter (R1: gonzalo#203).
    fn delete_body_claiming(author: &str) -> Vec<u8> {
        serde_json::to_vec(&DeleteBody {
            expected: None,
            author: Some(gonzalo_core::Identity::new(author)),
        })
        .unwrap()
    }

    fn purge_body(expected: &Revision) -> Vec<u8> {
        serde_json::to_vec(&PurgeBody {
            expected: expected.clone(),
        })
        .unwrap()
    }

    /// PUT a live record at memory/col/x with `put_token`, then DELETE it with
    /// `delete_token`, leaving a tombstone. Tokens are `None` in open mode.
    async fn seed_tombstone(
        svc: &Service,
        auth: &Arc<Auth>,
        put_token: Option<&str>,
        delete_token: Option<&str>,
    ) {
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            LIVE,
            put_token,
            Some(put_body("memory", "client")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "DELETE",
            LIVE,
            delete_token,
            Some(delete_body()),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    async fn raw_get(
        svc: &Service,
        auth: &Arc<Auth>,
        path: &str,
        token: Option<&str>,
    ) -> (StatusCode, Option<Record>) {
        let (s, body) = call(svc.clone(), auth.clone(), "GET", path, token, None).await;
        let record = if s == StatusCode::OK {
            serde_json::from_slice::<RawRecordBody>(&body)
                .unwrap()
                .record
        } else {
            None
        };
        (s, record)
    }

    async fn keys(svc: &Service, auth: &Arc<Auth>, path: &str, token: &str) -> Vec<RecordKey> {
        let (s, body) = call(svc.clone(), auth.clone(), "GET", path, Some(token), None).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn delete_over_the_wire_leaves_a_tombstone_for_raw_reads() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let key = RecordKey::new("memory", "col", "x");

        // Consumer surface: gone.
        let (s, _) = call(svc.clone(), auth.clone(), "GET", LIVE, Some("rtok"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(
            !keys(&svc, &auth, "/v1/keys?namespace=memory", "rtok")
                .await
                .contains(&key)
        );

        // Raw surface: a reader sees the tombstone.
        let (s, tomb) = raw_get(&svc, &auth, RAW, Some("rtok")).await;
        assert_eq!(s, StatusCode::OK);
        let tomb = tomb.expect("tombstone");
        assert_eq!(tomb.kind, gonzalo_core::RecordKind::Tombstone);
        assert_eq!(tomb.revision.hash, gonzalo_core::tombstone_hash());
        assert!(tomb.deleted_at.is_some());
        assert!(
            keys(&svc, &auth, "/v1/raw/keys?namespace=memory", "rtok")
                .await
                .contains(&key)
        );
    }

    #[tokio::test]
    async fn delete_stamps_the_deleter_on_the_tombstone() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        // `writer` wrote the live record; `admin` deletes it.
        seed_tombstone(&svc, &auth, Some("wtok"), Some("atok")).await;
        let (_, tomb) = raw_get(&svc, &auth, RAW, Some("atok")).await;
        assert_eq!(
            tomb.expect("tombstone").meta.author,
            gonzalo_core::Identity::new("admin")
        );
    }

    #[tokio::test]
    async fn open_mode_delete_keeps_the_prior_author() {
        // Open mode has no identity to stamp (ADR 0015), for delete as for put.
        let (svc, _d) = fs_service();
        let auth = open();
        seed_tombstone(&svc, &auth, None, None).await;
        let (_, tomb) = raw_get(&svc, &auth, RAW, None).await;
        assert_eq!(
            tomb.expect("tombstone").meta.author,
            gonzalo_core::Identity::new("client")
        );
    }

    #[tokio::test]
    async fn http_open_mode_delete_keeps_a_claimed_author() {
        // Open mode's implicit principal is an admin (`delete_author` returns
        // the claim), so a claimed deleter is honoured, unlike the prior
        // author kept when there is no claim at all.
        let (svc, _d) = fs_service();
        let auth = open();
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            LIVE,
            None,
            Some(put_body("memory", "client")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "DELETE",
            LIVE,
            None,
            Some(delete_body_claiming("origin")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, tomb) = raw_get(&svc, &auth, RAW, None).await;
        assert_eq!(
            tomb.expect("tombstone").meta.author,
            gonzalo_core::Identity::new("origin")
        );
    }

    #[tokio::test]
    async fn http_delete_by_non_admin_ignores_a_claimed_author() {
        // A non-admin cannot forge the deleter's identity through the wire
        // field either (R1, mirrors `Principal::delete_author`).
        //
        // The live record is seeded by `atok` (admin), so the prior author is
        // "admin" — distinct from both "forged" (the claim) and "writer" (the
        // deleter). Asserting "writer" then fails if the claim is honoured
        // ("forged") and fails if the author is left untouched ("admin"): the
        // only way to pass is for the handler to stamp the non-admin deleter's
        // own identity, exactly as `delete_stamps_the_deleter_on_the_tombstone`
        // discriminates the admin branch.
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            LIVE,
            Some("atok"),
            Some(put_body("memory", "client")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "DELETE",
            LIVE,
            Some("wtok"),
            Some(delete_body_claiming("forged")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, tomb) = raw_get(&svc, &auth, RAW, Some("wtok")).await;
        assert_eq!(
            tomb.expect("tombstone").meta.author,
            gonzalo_core::Identity::new("writer")
        );
    }

    #[tokio::test]
    async fn http_delete_by_admin_keeps_a_claimed_author() {
        // An admin token is the replication credential: it may name the
        // deleter (R1, mirrors `Principal::delete_author`).
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            LIVE,
            Some("wtok"),
            Some(put_body("memory", "client")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "DELETE",
            LIVE,
            Some("atok"),
            Some(delete_body_claiming("origin")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, tomb) = raw_get(&svc, &auth, RAW, Some("atok")).await;
        assert_eq!(
            tomb.expect("tombstone").meta.author,
            gonzalo_core::Identity::new("origin")
        );
    }

    #[tokio::test]
    async fn raw_reads_need_read_scope_and_absence_is_200_null() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();

        // In scope, absent: 200 with a null record, never 404 (404 is reserved
        // for "this daemon has no raw route").
        let (s, rec) = raw_get(
            &svc,
            &auth,
            "/v1/raw/records/memory/col/absent",
            Some("rtok"),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(rec, None);

        // Out of scope.
        let (s, _) = raw_get(&svc, &auth, "/v1/raw/records/secrets/col/x", Some("rtok")).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "GET",
            "/v1/raw/keys?namespace=secrets",
            Some("rtok"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // No token.
        let (s, _) = call(svc, auth, "GET", RAW, None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unscoped_list_raw_requires_admin() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        for token in ["rtok", "wtok"] {
            let (s, _) = call(
                svc.clone(),
                auth.clone(),
                "GET",
                "/v1/raw/keys",
                Some(token),
                None,
            )
            .await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{token}");
        }
        let (s, _) = call(svc, auth, "GET", "/v1/raw/keys", Some("atok"), None).await;
        assert_eq!(s, StatusCode::OK);
    }

    /// `put_raw` a record claiming author `"origin"` with `token`, assert it
    /// committed at the incoming revision, and return the stored author.
    async fn put_raw_author(
        svc: &Service,
        auth: &Arc<Auth>,
        token: Option<&str>,
    ) -> gonzalo_core::Identity {
        let replica = record_at("origin", Revision::initial(b"{}"));
        let (s, body) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            token,
            Some(put_body_with(replica.clone(), None)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(matches!(
            serde_json::from_slice::<PutOutcome>(&body).unwrap(),
            PutOutcome::Committed { revision } if revision == replica.revision
        ));
        let (_, stored) = raw_get(svc, auth, RAW, token).await;
        stored.expect("stored").meta.author
    }

    #[tokio::test]
    async fn put_raw_needs_write_scope_and_matching_path() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        let replica = record_at("origin", Revision::initial(b"{}"));

        // A reader may not write.
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            Some("rtok"),
            Some(put_body_with(replica.clone(), None)),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // Path and body key must agree (#158).
        let (s, _) = call(
            svc,
            auth,
            "PUT",
            "/v1/raw/records/memory/col/y",
            Some("wtok"),
            Some(put_body_with(replica, None)),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_raw_by_scoped_writer_is_stamped_with_the_writer() {
        // A non-admin cannot forge authorship through the raw route (ADR 0015).
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        assert_eq!(
            put_raw_author(&svc, &auth, Some("wtok")).await,
            gonzalo_core::Identity::new("writer")
        );
    }

    #[tokio::test]
    async fn put_raw_by_admin_keeps_the_incoming_author() {
        // An admin token is the replication credential: the original writer
        // survives replication.
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        assert_eq!(
            put_raw_author(&svc, &auth, Some("atok")).await,
            gonzalo_core::Identity::new("origin")
        );
    }

    #[tokio::test]
    async fn put_raw_in_open_mode_keeps_the_incoming_author() {
        // Open mode's implicit principal is an admin.
        let (svc, _d) = fs_service();
        let auth = open();
        assert_eq!(
            put_raw_author(&svc, &auth, None).await,
            gonzalo_core::Identity::new("origin")
        );
    }

    #[tokio::test]
    async fn put_raw_overwrites_a_tombstone_verbatim_and_none_conflicts() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let (_, tomb) = raw_get(&svc, &auth, RAW, Some("atok")).await;
        let tomb = tomb.expect("tombstone");

        // expected = None over a tombstone is a conflict for put_raw, and the
        // conflict may carry the tombstone.
        let peer = record_at("peer", Revision::initial(b"peer"));
        let (s, body) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            Some("wtok"),
            Some(put_body_with(peer.clone(), None)),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(matches!(
            serde_json::from_slice::<PutOutcome>(&body).unwrap(),
            PutOutcome::Conflict { conflict } if conflict.current.revision == tomb.revision
        ));

        // expected = the tombstone's revision writes the record unchanged.
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            Some("wtok"),
            Some(put_body_with(peer.clone(), Some(tomb.revision.clone()))),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, stored) = raw_get(&svc, &auth, RAW, Some("rtok")).await;
        let stored = stored.expect("stored");
        assert_eq!(stored.revision, peer.revision);
        assert_eq!(stored.kind, gonzalo_core::RecordKind::MemoryTier);
    }

    #[tokio::test]
    async fn put_not_found_is_412_on_both_put_routes() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        let never = Revision::initial(b"never current");

        // put_raw with Some over an absent key → NotFound → 412.
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "PUT",
            RAW,
            Some("wtok"),
            Some(put_body_with(
                record_at("w", Revision::initial(b"{}")),
                Some(never.clone()),
            )),
        )
        .await;
        assert_eq!(s, StatusCode::PRECONDITION_FAILED);

        // Consumer put with Some over a tombstone → NotFound → 412.
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let (s, _) = call(
            svc,
            auth,
            "PUT",
            LIVE,
            Some("wtok"),
            Some(put_body_with(
                record_at("w", Revision::initial(b"{}")),
                Some(never),
            )),
        )
        .await;
        assert_eq!(s, StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn purge_requires_admin() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let (_, tomb) = raw_get(&svc, &auth, RAW, Some("atok")).await;
        let rev = tomb.expect("tombstone").revision;

        for token in ["rtok", "wtok"] {
            let (s, _) = call(
                svc.clone(),
                auth.clone(),
                "POST",
                PURGE,
                Some(token),
                Some(purge_body(&rev)),
            )
            .await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{token}");
        }

        let (s, body) = call(
            svc.clone(),
            auth.clone(),
            "POST",
            PURGE,
            Some("atok"),
            Some(purge_body(&rev)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(matches!(
            serde_json::from_slice::<DeleteOutcome>(&body).unwrap(),
            DeleteOutcome::Deleted
        ));
        let (s, rec) = raw_get(&svc, &auth, RAW, Some("atok")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(rec, None);
    }

    #[tokio::test]
    async fn purge_with_stale_expected_is_409() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        seed_tombstone(&svc, &auth, Some("wtok"), Some("wtok")).await;
        let (s, body) = call(
            svc,
            auth,
            "POST",
            PURGE,
            Some("atok"),
            Some(purge_body(&Revision::initial(b"not the tombstone"))),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(matches!(
            serde_json::from_slice::<DeleteOutcome>(&body).unwrap(),
            DeleteOutcome::Conflict { .. }
        ));
    }

    #[tokio::test]
    async fn purge_authorizes_before_parsing_the_body() {
        let (svc, _d) = fs_service();
        let auth = tomb_auth();
        let (s, _) = call(
            svc.clone(),
            auth.clone(),
            "POST",
            PURGE,
            Some("wtok"),
            Some(b"not json".to_vec()),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(
            svc,
            auth,
            "POST",
            PURGE,
            Some("atok"),
            Some(b"not json".to_vec()),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn raw_backend_error_is_opaque() {
        let dir = TempDir::new().unwrap();
        let blobs = Arc::new(FsStore::new(dir.path()));
        let svc = Service::new(Arc::new(DownStore), blobs);
        let (s, body) = call(svc, scoped(), "GET", RAW, Some("wtok"), None).await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(String::from_utf8(body).unwrap(), "internal error");
    }
}
