//! HTTP/JSON transport over the shared `Service`, using axum.

use crate::Service;
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{Next, from_fn},
    response::{IntoResponse, Response},
    routing::get,
};
use gonzalo_core::{KeyPrefix, PutResult, RecordKey};
use gonzalo_proto::http::{PutBody, PutOutcome};
use serde::Deserialize;
use std::sync::Arc;

/// Build the axum router. When `auth` is `Some`, every request must carry
/// `Authorization: Bearer <token>`.
pub fn router(service: Service, auth: Option<String>) -> Router {
    let mut app = Router::new()
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
    if let Some(token) = auth {
        let token = Arc::new(token);
        app = app.layer(from_fn(move |req: Request, next: Next| {
            let token = token.clone();
            async move {
                // Health/readiness probes are unauthenticated: k8s liveness and
                // readiness checks carry no bearer token, and a probe gated
                // behind auth would fail closed and get the pod killed.
                if is_probe_path(req.uri().path()) {
                    return next.run(req).await;
                }
                let ok = bearer(req.headers())
                    .map(|t| t == token.as_str())
                    .unwrap_or(false);
                if ok {
                    next.run(req).await
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }
        }));
    }
    app
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
    Path((ns, col, id)): Path<(String, String, String)>,
) -> Response {
    match svc.get(&RecordKey::new(ns, col, id)).await {
        Ok(Some(rec)) => (StatusCode::OK, Json(rec)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => server_error(e),
    }
}

async fn put_record(State(svc): State<Arc<Service>>, Json(body): Json<PutBody>) -> Response {
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

async fn list_keys(State(svc): State<Arc<Service>>, Query(q): Query<ListQuery>) -> Response {
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
    Json(conn): Json<gonzalo_ticket_config::Connection>,
) -> Response {
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

async fn graph_definitions(
    State(svc): State<Arc<Service>>,
    Query(q): Query<GraphQuery>,
) -> Response {
    match svc.graph_definitions(&q.repo, &q.view, &q.name).await {
        Ok(items) => (StatusCode::OK, Json(items)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_references_to(
    State(svc): State<Arc<Service>>,
    Query(q): Query<GraphQuery>,
) -> Response {
    match svc.graph_references_to(&q.repo, &q.view, &q.name).await {
        Ok(items) => (StatusCode::OK, Json(items)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_callers_of(
    State(svc): State<Arc<Service>>,
    Query(q): Query<GraphQuery>,
) -> Response {
    match svc.graph_callers_of(&q.repo, &q.view, &q.name).await {
        Ok(names) => (StatusCode::OK, Json(names)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_callees(State(svc): State<Arc<Service>>, Query(q): Query<GraphQuery>) -> Response {
    match svc.graph_callees(&q.repo, &q.view, &q.name).await {
        Ok(names) => (StatusCode::OK, Json(names)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn graph_impact(State(svc): State<Arc<Service>>, Query(q): Query<GraphQuery>) -> Response {
    match svc.graph_impact(&q.repo, &q.view, &q.name).await {
        Ok(names) => (StatusCode::OK, Json(names)).into_response(),
        Err(e) => server_error(e),
    }
}

/// Serve HTTP/JSON on an already-bound listener until the process ends.
pub async fn serve_http(
    listener: tokio::net::TcpListener,
    service: Service,
    auth: Option<String>,
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

    async fn status_of(service: Service, auth: Option<String>, path: &str) -> StatusCode {
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
        assert_eq!(status_of(svc, None, "/healthz").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_ok_when_store_reachable() {
        let (svc, _dir) = fs_service();
        assert_eq!(status_of(svc, None, "/readyz").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn probes_bypass_auth_but_other_routes_do_not() {
        let auth = Some("secret".to_string());
        let (svc, _d1) = fs_service();
        assert_eq!(
            status_of(svc, auth.clone(), "/healthz").await,
            StatusCode::OK,
            "healthz must not require a token"
        );
        let (svc, _d2) = fs_service();
        assert_eq!(
            status_of(svc, auth.clone(), "/readyz").await,
            StatusCode::OK,
            "readyz must not require a token"
        );
        // A normal route without the token is still rejected.
        let (svc, _d3) = fs_service();
        assert_eq!(
            status_of(svc, auth, "/v1/keys").await,
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

    #[tokio::test]
    async fn readyz_is_503_when_store_unreachable() {
        let dir = TempDir::new().unwrap();
        // Records via the down store; blobs via fs (readiness only probes records).
        let blobs = Arc::new(FsStore::new(dir.path()));
        let svc = Service::new(Arc::new(DownStore), blobs);
        assert_eq!(
            status_of(svc, None, "/readyz").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
