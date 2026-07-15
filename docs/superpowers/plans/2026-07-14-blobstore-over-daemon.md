# BlobStore over the daemon + remote client — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose content-addressed blobs over the `gonzalo-server` daemon (HTTP + gRPC) and implement `BlobStore` on the remote `ServerStore` client, so a daemon-backed consumer gets the full `Store` + `BlobStore` surface.

**Architecture:** The transport-agnostic `Service` gains four blob pass-through methods and a `max_blob_size` field. The HTTP transport adds hash-addressed `/v1/blobs` routes (raw bytes, server-verified hash, per-route body limit); the gRPC transport adds four blob RPCs (JSON-free — raw `bytes`), with raised decode limits. `ServerStore` implements `BlobStore` over both transports mirroring its existing `Store` impl. All blob ops authorize against a reserved `_blobs` namespace (ADR 0015). A cross-crate conformance test drives `run_blob_store_conformance` against a real daemon over both transports.

**Tech Stack:** Rust (async_trait, tokio), axum (HTTP), tonic/prost (gRPC), reqwest (client HTTP), blake3 (`ContentHash`).

## Global Constraints

- Reserved authz namespace name is exactly `_blobs`. Reads (get/list) require `Access::Read` on `_blobs`; writes (put/delete) require `Access::Write` on `_blobs`.
- `DEFAULT_MAX_BLOB_SIZE: usize = 64 * 1024 * 1024` (64 MiB), defined in `gonzalo-proto` and shared by server and client.
- Blob PUT is content-addressed and self-verifying: the server recomputes `ContentHash::of(&content)` and rejects a mismatch with the path/request hash (`400` HTTP / `InvalidArgument` gRPC) **before** writing.
- Backend failures stay opaque: HTTP via `server_error` → `500 "internal error"`; gRPC via `internal` → `Status::internal("internal error")` (#148).
- Client error decoding follows #147: non-`200`/`404` HTTP statuses surface as `CoreError::Backend("daemon returned <status>: <body>")`, never a masked JSON/parse error.
- The daemon's env knob is `GONZALO_MAX_BLOB_SIZE` (bytes; default `DEFAULT_MAX_BLOB_SIZE`).
- Verification gate before any push (mirrors CI): `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace --all-targets`, `cargo test --workspace`.
- `ContentHash(pub String)` — the hex string is `hash.0`; construct with `ContentHash::of(bytes)`.

---

### Task 1: Shared `DEFAULT_MAX_BLOB_SIZE` constant in `gonzalo-proto`

**Files:**
- Modify: `crates/gonzalo-proto/src/lib.rs`

**Interfaces:**
- Produces: `pub const gonzalo_proto::DEFAULT_MAX_BLOB_SIZE: usize` (= 64 MiB).

- [ ] **Step 1: Add the constant + a guard test**

In `crates/gonzalo-proto/src/lib.rs`, add near the top of the module (after the existing module docs / re-exports):

```rust
/// Default ceiling for a single blob transferred over the daemon, in bytes
/// (64 MiB). Shared by the server (HTTP body limit + gRPC decode limit) and the
/// client (gRPC decode limit) so both agree on the supported blob size. The
/// daemon may raise its own limit via `GONZALO_MAX_BLOB_SIZE`, but a client
/// still caps decoding at this constant (see gonzalo#184 design §4).
pub const DEFAULT_MAX_BLOB_SIZE: usize = 64 * 1024 * 1024;

#[cfg(test)]
mod tests {
    #[test]
    fn default_max_blob_size_is_64_mib() {
        assert_eq!(super::DEFAULT_MAX_BLOB_SIZE, 67_108_864);
    }
}
```

If `lib.rs` already has a `#[cfg(test)] mod tests`, add the `default_max_blob_size_is_64_mib` test inside it instead of creating a second module.

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p gonzalo-proto default_max_blob_size_is_64_mib`
Expected: PASS (1 test).

- [ ] **Step 3: Commit**

```bash
git add crates/gonzalo-proto/src/lib.rs
git commit -m "feat(proto): add shared DEFAULT_MAX_BLOB_SIZE constant (#184)"
```

---

### Task 2: `Service` blob methods + `max_blob_size`

**Files:**
- Modify: `crates/gonzalo-server/src/service.rs`

**Interfaces:**
- Consumes: `gonzalo_proto::DEFAULT_MAX_BLOB_SIZE` (Task 1); `gonzalo_core::{BlobStore, ContentHash}`.
- Produces on `Service`:
  - `fn with_max_blob_size(self, n: usize) -> Self`
  - `fn max_blob_size(&self) -> usize`
  - `async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>>`
  - `async fn put_blob(&self, content: &[u8]) -> Result<ContentHash>`
  - `async fn list_blobs(&self) -> Result<Vec<ContentHash>>`
  - `async fn delete_blob(&self, hash: &ContentHash) -> Result<()>`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `crates/gonzalo-server/src/service.rs`:

```rust
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
```

Note: `fresh_fs()` already exists in this test module and returns `Arc<FsStore>`; `Service::new(store, blobs)` takes two `Arc`s. `gonzalo-proto` is a dependency of `gonzalo-server` — confirm it is listed in `crates/gonzalo-server/Cargo.toml` `[dependencies]` (it is, used by grpc.rs). If a test-only use of `gonzalo_core::ContentHash` isn't imported, the fully-qualified path in the test avoids adding imports.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p gonzalo-server blob_methods_delegate_and_default_size_is_64_mib`
Expected: FAIL to compile — `max_blob_size`, `with_max_blob_size`, `get_blob`, etc. do not exist.

- [ ] **Step 3: Implement the field + methods**

In `crates/gonzalo-server/src/service.rs`:

Add the import (top of file, extend the existing `gonzalo_core` use):

```rust
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DeleteResult, KeyPrefix, Manifest, PutResult, Record,
    RecordKey, Result, Revision, Store,
};
```

Add the field to the struct:

```rust
pub struct Service {
    store: Arc<dyn Store>,
    blobs: Arc<dyn BlobStore>,
    graph_root: Option<PathBuf>,
    /// Ceiling for a single blob over the transports (bytes). Defaults to
    /// `DEFAULT_MAX_BLOB_SIZE`; the daemon may raise it from the environment.
    max_blob_size: usize,
}
```

Set the default in `new` and add the builder + accessors + blob methods:

```rust
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
    // ... existing methods unchanged ...
}
```

Leave `with_graph_root` and all existing methods as-is.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p gonzalo-server blob_methods_delegate_and_default_size_is_64_mib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-server/src/service.rs
git commit -m "feat(server): Service blob methods + max_blob_size (#184)"
```

---

### Task 3: HTTP blob routes

**Files:**
- Modify: `crates/gonzalo-server/src/http.rs`

**Interfaces:**
- Consumes: `Service::{get_blob, put_blob, list_blobs, delete_blob, max_blob_size}` (Task 2); `Access::{Read, Write}`, `Principal`, `server_error`, `forbidden` (existing in this file).
- Produces: routes `GET|PUT|DELETE /v1/blobs/{hash}` and `GET /v1/blobs` on the router built by `router(service, auth)`.

- [ ] **Step 1: Write the failing tests**

The test module in `http.rs` already has the `call(service, auth, method, path, token, body) -> (StatusCode, Vec<u8>)` helper, the `open()` / `scoped()` / `admin_token()` auth fixtures, and `fs_service() -> (Service, TempDir)`. Add:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p gonzalo-server --lib http::tests::blob_`
Expected: FAIL — routes 404 (list assertion / roundtrip fail) and the handlers don't exist yet.

- [ ] **Step 3: Implement the routes + handlers**

In `crates/gonzalo-server/src/http.rs`:

Extend imports:

```rust
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{Next, from_fn},
    response::{IntoResponse, Response},
    routing::get,
};
use gonzalo_core::{ContentHash, DeleteResult, Identity, KeyPrefix, PutResult, RecordKey};
```

The reserved blob namespace — add near the top of the file:

```rust
/// Blobs are namespace-agnostic; they authorize against this reserved
/// namespace (ADR 0015). Admins (`*`) and open mode cover it; a scoped
/// principal is granted blob access by listing `_blobs` in its read/write set.
const BLOB_NS: &str = "_blobs";
```

In `router`, register the blob routes with a body limit scoped to just those
routes (records keep axum's default), then merge into the app before
`.with_state`:

```rust
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
        .route("/v1/tickets/sync", axum::routing::post(ticket_sync))
        .route("/v1/graph/definitions", get(graph_definitions))
        .route("/v1/graph/references", get(graph_references_to))
        .route("/v1/graph/callers", get(graph_callers_of))
        .route("/v1/graph/callees", get(graph_callees))
        .route("/v1/graph/impact", get(graph_impact))
        .merge(blob_routes)
        .with_state(Arc::new(service));
    app.layer(from_fn(move |mut req: Request, next: Next| {
        // ... unchanged auth middleware ...
        let auth = auth.clone();
        async move {
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
```

Add the four handlers (place them after `list_keys`, before `ticket_sync`):

```rust
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
```

Note: `ContentHash` is a tuple struct with a public field (`ContentHash(pub String)`), so `ContentHash(hash)` constructs it directly from the path string.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p gonzalo-server --lib http::tests::blob_`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-server/src/http.rs
git commit -m "feat(server): HTTP blob endpoints under /v1/blobs (#184)"
```

---

### Task 4: gRPC blob RPCs — proto schema

**Files:**
- Modify: `crates/gonzalo-proto/proto/gonzalo.proto`

**Interfaces:**
- Produces (generated into `gonzalo_proto::v1`): `PutBlobRequest{hash, content}`, `PutBlobResponse{hash}`, `GetBlobRequest{hash}`, `GetBlobResponse{found, content}`, `ListBlobsRequest{}`, `ListBlobsResponse{hashes}`, `DeleteBlobRequest{hash}`, `DeleteBlobResponse{}`, and the four `Gonzalo` service methods `PutBlob`/`GetBlob`/`ListBlobs`/`DeleteBlob`.

- [ ] **Step 1: Add the RPCs + messages**

In `crates/gonzalo-proto/proto/gonzalo.proto`, add four RPCs to the `service Gonzalo` block (after the existing graph RPCs):

```proto
  // Content-addressed blobs (gonzalo#184). Bodies ride as raw bytes, not JSON.
  rpc PutBlob(PutBlobRequest) returns (PutBlobResponse);
  rpc GetBlob(GetBlobRequest) returns (GetBlobResponse);
  rpc ListBlobs(ListBlobsRequest) returns (ListBlobsResponse);
  rpc DeleteBlob(DeleteBlobRequest) returns (DeleteBlobResponse);
```

Add the messages at the end of the file:

```proto
// --- Content-addressed blobs (gonzalo#184) ---
// Blob content rides as raw bytes. PutBlob carries the expected ContentHash so
// the server can verify content integrity, mirroring the HTTP hash-addressed
// PUT.
message PutBlobRequest {
  // Expected ContentHash (hex) of `content`; server rejects a mismatch.
  string hash = 1;
  bytes content = 2;
}
message PutBlobResponse {
  // The committed ContentHash (hex).
  string hash = 1;
}
message GetBlobRequest {
  string hash = 1;
}
message GetBlobResponse {
  bool found = 1;
  bytes content = 2;
}
message ListBlobsRequest {}
message ListBlobsResponse {
  // Hex ContentHash of each stored blob.
  repeated string hashes = 1;
}
message DeleteBlobRequest {
  string hash = 1;
}
message DeleteBlobResponse {}
```

- [ ] **Step 2: Build to regenerate + verify the types compile**

Run: `cargo build -p gonzalo-proto`
Expected: PASS — prost/tonic regenerate the new messages and service methods.

- [ ] **Step 3: Commit**

```bash
git add crates/gonzalo-proto/proto/gonzalo.proto
git commit -m "feat(proto): blob RPCs (PutBlob/GetBlob/ListBlobs/DeleteBlob) (#184)"
```

---

### Task 5: gRPC blob handlers + raised decode limit

**Files:**
- Modify: `crates/gonzalo-server/src/grpc.rs`

**Interfaces:**
- Consumes: proto types from Task 4; `Service::{get_blob, put_blob, list_blobs, delete_blob, max_blob_size}` (Task 2); `Access`, `internal`, `bearer` (existing in this file); `gonzalo_core::ContentHash`.
- Produces: `Gonzalo::{put_blob, get_blob, list_blobs, delete_blob}` impls on `GrpcAdapter`; `serve_grpc` raises the server decode limit to `service.max_blob_size()`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `grpc.rs`. The module already has `fs_adapter(auth)`, `scoped_auth()`, `with_token(msg, token)`, and imports `GrpcAdapter`, `Service`, `Request`. Add proto imports to the test `use super::*;` scope as needed (they resolve through `gonzalo_proto::v1::*` already imported at file top — extend that import in Step 3).

```rust
#[tokio::test]
async fn grpc_blob_roundtrip_open() {
    let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
    let adapter = GrpcAdapter::new(Service::new(fs.clone(), fs));
    let content = b"grpc blob body".to_vec();
    let hash = gonzalo_core::ContentHash::of(&content).0;

    let put = adapter
        .put_blob(Request::new(PutBlobRequest {
            hash: hash.clone(),
            content: content.clone(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(put.hash, hash);

    let got = adapter
        .get_blob(Request::new(GetBlobRequest { hash: hash.clone() }))
        .await
        .unwrap()
        .into_inner();
    assert!(got.found);
    assert_eq!(got.content, content);

    let listed = adapter
        .list_blobs(Request::new(ListBlobsRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.hashes, vec![hash.clone()]);

    adapter
        .delete_blob(Request::new(DeleteBlobRequest { hash: hash.clone() }))
        .await
        .unwrap();
    let gone = adapter
        .get_blob(Request::new(GetBlobRequest { hash }))
        .await
        .unwrap()
        .into_inner();
    assert!(!gone.found);
}

#[tokio::test]
async fn grpc_put_blob_hash_mismatch_is_invalid_argument() {
    let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
    let adapter = GrpcAdapter::new(Service::new(fs.clone(), fs));
    let err = adapter
        .put_blob(Request::new(PutBlobRequest {
            hash: gonzalo_core::ContentHash::of(b"not the body").0,
            content: b"the body".to_vec(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn grpc_blob_ops_require_blobs_scope() {
    // `scoped_auth()` grants `memory` only, not `_blobs`.
    let adapter = fs_adapter(scoped_auth());
    let content = b"scoped".to_vec();
    let hash = gonzalo_core::ContentHash::of(&content).0;

    let denied = adapter
        .put_blob(with_token(
            PutBlobRequest {
                hash: hash.clone(),
                content: content.clone(),
            },
            "wtok",
        ))
        .await
        .unwrap_err();
    assert_eq!(denied.code(), tonic::Code::PermissionDenied);

    // Admin token succeeds.
    adapter
        .put_blob(with_token(PutBlobRequest { hash, content }, "atok"))
        .await
        .unwrap();
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p gonzalo-server --lib grpc::tests::grpc_blob grpc::tests::grpc_put_blob`
Expected: FAIL to compile — `put_blob`/`get_blob`/`list_blobs`/`delete_blob` not on `GrpcAdapter`, proto types unimported.

- [ ] **Step 3: Implement the handlers + decode limit**

Extend the proto import at the top of `grpc.rs`:

```rust
use gonzalo_proto::v1::{
    DeleteBlobRequest, DeleteBlobResponse, DeleteRequest, DeleteResponse, GetBlobRequest,
    GetBlobResponse, GetRequest, GetResponse, GraphLocatedResponse, GraphNamesResponse,
    GraphQueryRequest, ListBlobsRequest, ListBlobsResponse, ListRequest, ListResponse,
    PutBlobRequest, PutBlobResponse, PutRequest, PutResponse, TicketSyncRequest, TicketSyncResponse,
    gonzalo_server::{Gonzalo, GonzaloServer},
};
```

Add `ContentHash` to the `gonzalo_core` import:

```rust
use gonzalo_core::{
    ContentHash, DeleteResult, Identity, KeyPrefix, PutResult, Record, RecordKey, Revision,
};
```

Add the reserved namespace constant near the top of the file (module scope):

```rust
/// Reserved authz namespace for namespace-agnostic blob ops (ADR 0015), matching
/// the HTTP transport.
const BLOB_NS: &str = "_blobs";
```

Add the four handler methods inside `impl Gonzalo for GrpcAdapter` (after `graph_impact`):

```rust
    async fn put_blob(
        &self,
        req: Request<PutBlobRequest>,
    ) -> Result<Response<PutBlobResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Write, BLOB_NS)?;
        // Verify the content hashes to the advertised value before writing —
        // same integrity check as the HTTP hash-addressed PUT.
        let computed = ContentHash::of(&r.content);
        if computed.0 != r.hash {
            return Err(Status::invalid_argument(
                "blob content does not match the advertised hash",
            ));
        }
        let hash = self.service.put_blob(&r.content).await.map_err(internal)?;
        Ok(Response::new(PutBlobResponse { hash: hash.0 }))
    }

    async fn get_blob(
        &self,
        req: Request<GetBlobRequest>,
    ) -> Result<Response<GetBlobResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Read, BLOB_NS)?;
        let found = self
            .service
            .get_blob(&ContentHash(r.hash))
            .await
            .map_err(internal)?;
        let resp = match found {
            Some(content) => GetBlobResponse {
                found: true,
                content,
            },
            None => GetBlobResponse {
                found: false,
                content: Vec::new(),
            },
        };
        Ok(Response::new(resp))
    }

    async fn list_blobs(
        &self,
        req: Request<ListBlobsRequest>,
    ) -> Result<Response<ListBlobsResponse>, Status> {
        let (metadata, _ext, _r) = req.into_parts();
        self.authorize(&metadata, Access::Read, BLOB_NS)?;
        let hashes = self.service.list_blobs().await.map_err(internal)?;
        Ok(Response::new(ListBlobsResponse {
            hashes: hashes.into_iter().map(|h| h.0).collect(),
        }))
    }

    async fn delete_blob(
        &self,
        req: Request<DeleteBlobRequest>,
    ) -> Result<Response<DeleteBlobResponse>, Status> {
        let (metadata, _ext, r) = req.into_parts();
        self.authorize(&metadata, Access::Write, BLOB_NS)?;
        self.service
            .delete_blob(&ContentHash(r.hash))
            .await
            .map_err(internal)?;
        Ok(Response::new(DeleteBlobResponse {}))
    }
```

Raise the server decode limit in `serve_grpc` so large `PutBlob` content is
accepted (tonic defaults to 4 MiB). Read the limit off the service **before** it
is moved into the adapter:

```rust
pub async fn serve_grpc(
    listener: tokio::net::TcpListener,
    service: Service,
    auth: Arc<Auth>,
) -> Result<(), tonic::transport::Error> {
    let max_blob = service.max_blob_size();
    let adapter = GrpcAdapter::with_auth(service, auth);
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tonic::transport::Server::builder()
        .add_service(GonzaloServer::new(adapter).max_decoding_message_size(max_blob))
        .serve_with_incoming(incoming)
        .await
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p gonzalo-server --lib grpc::tests::grpc_blob grpc::tests::grpc_put_blob`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-server/src/grpc.rs
git commit -m "feat(server): gRPC blob handlers + raised decode limit (#184)"
```

---

### Task 6: `ServerStore` implements `BlobStore` (HTTP + gRPC)

**Files:**
- Modify: `crates/gonzalo-store-server/src/lib.rs`

**Interfaces:**
- Consumes: HTTP routes (Task 3), gRPC RPCs (Tasks 4/5); `gonzalo_core::{BlobStore, ContentHash}`; `gonzalo_proto::{DEFAULT_MAX_BLOB_SIZE, v1::{PutBlobRequest, GetBlobRequest, ListBlobsRequest, DeleteBlobRequest}}`; existing `Backend`, `maybe_auth`, `grpc_request`, `be`, `se`, `status`.
- Produces: `impl BlobStore for ServerStore` (both transports); a `blobs_url(base, Option<&str>)` helper; a `classify_blob_put_response(status, body)` helper. Raises the gRPC client decode limit to `DEFAULT_MAX_BLOB_SIZE`.

- [ ] **Step 1: Write the failing unit tests**

Add to the `#[cfg(test)] mod tests` in `lib.rs` (these test the pure classify + URL helpers without a live server; end-to-end conformance is Task 7):

```rust
#[test]
fn blob_put_ok_returns_the_hash() {
    let content = b"blob body";
    let hash = ContentHash::of(content);
    let result = classify_blob_put_response(StatusCode::OK, "", hash.clone()).unwrap();
    assert_eq!(result, hash);
}

#[test]
fn blob_put_413_surfaces_status_and_body() {
    let hash = ContentHash::of(b"x");
    let err = classify_blob_put_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "blob exceeds max size",
        hash,
    )
    .unwrap_err();
    match err {
        CoreError::Backend(msg) => {
            assert!(msg.contains("413"), "want status 413 in {msg:?}");
            assert!(msg.contains("blob exceeds max size"), "want body in {msg:?}");
        }
        other => panic!("expected Backend error, got {other:?}"),
    }
}

#[test]
fn blob_put_400_mismatch_surfaces_status_and_body() {
    let hash = ContentHash::of(b"x");
    let err = classify_blob_put_response(
        StatusCode::BAD_REQUEST,
        "blob content does not match the URL hash",
        hash,
    )
    .unwrap_err();
    match err {
        CoreError::Backend(msg) => {
            assert!(msg.contains("400"), "want status 400 in {msg:?}");
            assert!(msg.contains("does not match"), "want body in {msg:?}");
        }
        other => panic!("expected Backend error, got {other:?}"),
    }
}
```

Add `use gonzalo_core::ContentHash;` to the test module's imports if not already pulled in via `use super::*;` / `use gonzalo_core::...` (the module already imports several `gonzalo_core` types; extend as needed).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p gonzalo-store-server blob_put`
Expected: FAIL to compile — `classify_blob_put_response` does not exist.

- [ ] **Step 3: Implement the classify + URL helpers, decode limit, and `BlobStore` impl**

In `crates/gonzalo-store-server/src/lib.rs`:

Extend the `gonzalo_core` import to add `BlobStore` and `ContentHash`:

```rust
use gonzalo_core::{
    BlobStore, ContentHash, CoreError, DeleteResult, KeyPrefix, PutResult, Record, RecordKey,
    Result, Revision, Store, store::Conflict,
};
```

Extend the proto v1 import with the blob request types:

```rust
use gonzalo_proto::v1::{
    DeleteBlobRequest, DeleteRequest, GetBlobRequest, GetRequest, ListBlobsRequest, ListRequest,
    PutBlobRequest, PutRequest, gonzalo_client::GonzaloClient,
};
```

Raise the gRPC client decode limit in `grpc_inner` so large `GetBlob` responses
are accepted:

```rust
    async fn grpc_inner(endpoint: String, token: Option<String>) -> Result<Self> {
        let client = GonzaloClient::connect(endpoint)
            .await
            .map_err(|e| CoreError::Backend(e.to_string()))?
            .max_decoding_message_size(gonzalo_proto::DEFAULT_MAX_BLOB_SIZE);
        Ok(Self {
            backend: Backend::Grpc { client, token },
        })
    }
```

Add a `blobs_url` helper next to `records_url` (inside `impl ServerStore`):

```rust
    /// `…/v1/blobs` (list) or `…/v1/blobs/{hash}` (one blob) when `hash` is set.
    fn blobs_url(base: &reqwest::Url, hash: Option<&str>) -> Result<reqwest::Url> {
        let mut url = base.clone();
        {
            let mut seg = url
                .path_segments_mut()
                .map_err(|_| CoreError::Backend("base URL cannot be a base".into()))?;
            seg.extend(["v1", "blobs"]);
            if let Some(h) = hash {
                seg.push(h);
            }
        }
        Ok(url)
    }
```

Add the classify helper near `classify_put_response`:

```rust
/// Decide a blob `put`'s result from the HTTP response status and body text.
/// `200 OK` → the (already-known) `hash`; every other status carries a plain
/// text body surfaced verbatim as `Backend("daemon returned <status>: <body>")`
/// — notably `413` (too large), `403` (authz), and `400` (hash mismatch) — so
/// the real failure is never masked (#147).
fn classify_blob_put_response(
    status: reqwest::StatusCode,
    body: &str,
    hash: ContentHash,
) -> Result<ContentHash> {
    match status {
        reqwest::StatusCode::OK => Ok(hash),
        other => Err(CoreError::Backend(format!(
            "daemon returned {other}: {body}"
        ))),
    }
}
```

Add the `BlobStore` impl (place after `impl Store for ServerStore`):

```rust
#[async_trait]
impl BlobStore for ServerStore {
    async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
        // The blob is content-addressed: compute the hash locally to address
        // the request, exactly as the daemon will recompute and verify it.
        let hash = ContentHash::of(content);
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::blobs_url(base, Some(&hash.0))?;
                let resp = maybe_auth(client.put(url).body(content.to_vec()), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                let text = resp.text().await.map_err(be)?;
                classify_blob_put_response(status, &text, hash)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    PutBlobRequest {
                        hash: hash.0.clone(),
                        content: content.to_vec(),
                    },
                    token,
                )?;
                let resp = client.put_blob(req).await.map_err(status)?.into_inner();
                Ok(ContentHash(resp.hash))
            }
        }
    }

    async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::blobs_url(base, Some(&hash.0))?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                let resp = resp.error_for_status().map_err(be)?;
                Ok(Some(resp.bytes().await.map_err(be)?.to_vec()))
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    GetBlobRequest {
                        hash: hash.0.clone(),
                    },
                    token,
                )?;
                let resp = client.get_blob(req).await.map_err(status)?.into_inner();
                Ok(resp.found.then_some(resp.content))
            }
        }
    }

    async fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::blobs_url(base, None)?;
                let resp = maybe_auth(client.get(url), token)
                    .send()
                    .await
                    .map_err(be)?
                    .error_for_status()
                    .map_err(be)?;
                Ok(resp.json::<Vec<ContentHash>>().await.map_err(be)?)
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(ListBlobsRequest {}, token)?;
                let resp = client.list_blobs(req).await.map_err(status)?.into_inner();
                Ok(resp.hashes.into_iter().map(ContentHash).collect())
            }
        }
    }

    async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
        match &self.backend {
            Backend::Http {
                base,
                client,
                token,
            } => {
                let url = Self::blobs_url(base, Some(&hash.0))?;
                let resp = maybe_auth(client.delete(url), token)
                    .send()
                    .await
                    .map_err(be)?;
                let status = resp.status();
                if status == reqwest::StatusCode::OK {
                    return Ok(());
                }
                let text = resp.text().await.map_err(be)?;
                Err(CoreError::Backend(format!(
                    "daemon returned {status}: {text}"
                )))
            }
            Backend::Grpc { client, token } => {
                let mut client = client.clone();
                let req = grpc_request(
                    DeleteBlobRequest {
                        hash: hash.0.clone(),
                    },
                    token,
                )?;
                client.delete_blob(req).await.map_err(status)?;
                Ok(())
            }
        }
    }
}
```

Note: `status` is both a local function name (`fn status(s: tonic::Status)`) and
a common variable name in this file. In `delete_blob` above, the local `let
status = resp.status();` shadows the function within that block — that is fine
because the block does not call the `status(...)` mapper. The gRPC arms call the
`status` **function** and never bind a `status` variable, so there is no clash.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p gonzalo-store-server blob_put`
Expected: PASS (3 tests). Also run `cargo build -p gonzalo-store-server` to confirm the `BlobStore` impl compiles.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-store-server/src/lib.rs
git commit -m "feat(store-server): implement BlobStore on ServerStore (HTTP + gRPC) (#184)"
```

---

### Task 7: End-to-end blob conformance over both transports

**Files:**
- Modify: `crates/gonzalo-integration-tests/tests/server_store_conformance.rs`

**Interfaces:**
- Consumes: `gonzalo_core::conformance::run_blob_store_conformance`; `serve_http`/`serve_grpc` (existing); `ServerStore::{http, grpc}` with the new `BlobStore` impl (Task 6).
- Produces: `http_server_store_passes_blob_conformance`, `grpc_server_store_passes_blob_conformance`.

- [ ] **Step 1: Write the failing tests**

`run_blob_store_conformance` calls its factory **5×**, and `blob_list_reports_stored_hashes` asserts `list_blobs()` is empty at the start — so each factory call needs a **fresh daemon over a fresh `FsStore`**. Add to `server_store_conformance.rs`:

```rust
use gonzalo_core::conformance::run_blob_store_conformance;

/// Stand up a fresh daemon (fresh `FsStore`) over HTTP and return a
/// `ServerStore` pointing at it. Each call is an independent, empty blob store —
/// required because the blob conformance suite asserts an empty `list_blobs()`
/// at the start of one sub-test.
async fn fresh_http_blob_store() -> ServerStore {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(listener, fresh_service().await, open()));
    ServerStore::http(&format!("http://{addr}")).unwrap()
}

/// As `fresh_http_blob_store`, over gRPC (waits briefly for the server to accept).
async fn fresh_grpc_blob_store() -> ServerStore {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_grpc(listener, fresh_service().await, open()));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    ServerStore::grpc(format!("http://{addr}")).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_blob_conformance() {
    run_blob_store_conformance(fresh_http_blob_store).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_passes_blob_conformance() {
    run_blob_store_conformance(fresh_grpc_blob_store).await;
}
```

Note: `run_blob_store_conformance(factory)` accepts `F: Fn() -> Fut`. A bare
`async fn` item coerces to `Fn() -> impl Future`, so passing
`fresh_http_blob_store` directly (no closure) type-checks. `fresh_service()`,
`open()`, `serve_http`, `serve_grpc`, `ServerStore`, and `TcpListener` are
already imported in this file.

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p gonzalo-integration-tests --test server_store_conformance blob_conformance`
Expected: PASS (2 tests). If run before Task 6 landed they would fail to compile (`BlobStore` not implemented); after Task 6 they exercise the full path end to end.

- [ ] **Step 3: Commit**

```bash
git add crates/gonzalo-integration-tests/tests/server_store_conformance.rs
git commit -m "test(integration): ServerStore passes blob conformance over HTTP + gRPC (#184)"
```

---

### Task 8: Daemon wiring — `GONZALO_MAX_BLOB_SIZE`

**Files:**
- Modify: `crates/gonzalo-server/src/bin/gonzalod.rs`

**Interfaces:**
- Consumes: `Service::with_max_blob_size` (Task 2); `gonzalo_proto::DEFAULT_MAX_BLOB_SIZE`.
- Produces: the daemon applies `GONZALO_MAX_BLOB_SIZE` (bytes; default `DEFAULT_MAX_BLOB_SIZE`) to its `Service`; a malformed value is a startup error.

- [ ] **Step 1: Parse the env var and apply it**

In `crates/gonzalo-server/src/bin/gonzalod.rs`, after `let mut service = Service::new(store, blobs);` and before the `if let Some(graph_root)` block, add:

```rust
    // Optional per-blob size ceiling (bytes). Defaults to the shared constant;
    // a malformed value is a hard startup error rather than a silent fallback.
    let max_blob_size = match std::env::var("GONZALO_MAX_BLOB_SIZE") {
        Ok(v) if !v.is_empty() => v
            .parse::<usize>()
            .map_err(|e| format!("GONZALO_MAX_BLOB_SIZE must be a byte count: {e}"))?,
        _ => gonzalo_proto::DEFAULT_MAX_BLOB_SIZE,
    };
    service = service.with_max_blob_size(max_blob_size);
```

Confirm `gonzalo-proto` is a dependency of `gonzalo-server` (it is — used by the transports). The `?` requires the error type to convert into `Box<dyn std::error::Error>`; `String` does via `From`, and `main` already returns `Result<(), Box<dyn std::error::Error>>`.

Add the variable to the module-doc env table (the `//!` block at the top of the file), after the `GONZALO_GRPC_ADDR` line:

```rust
//! - `GONZALO_MAX_BLOB_SIZE` — max bytes per blob over the transports (default 64 MiB)
```

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build -p gonzalo-server --bin gonzalod`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/gonzalo-server/src/bin/gonzalod.rs
git commit -m "feat(server): GONZALO_MAX_BLOB_SIZE daemon knob (#184)"
```

---

### Task 9: Document GitStore blob exclusion

**Files:**
- Modify: `crates/gonzalo-store-git/src/lib.rs` (module doc)
- Modify: `crates/gonzalo-server/src/bin/gonzalod.rs` (module doc)

**Interfaces:** none (docs only).

- [ ] **Step 1: Add the GitStore module-doc note**

At the top of `crates/gonzalo-store-git/src/lib.rs`, in the `//!` module doc, add a sentence (adapt to the existing wording — append, don't duplicate an existing note):

```rust
//! `GitStore` implements [`Store`](gonzalo_core::Store) but **not**
//! [`BlobStore`](gonzalo_core::BlobStore): git is not a natural content-addressed
//! blob store. Blob-backed records (e.g. checkpoint pre-images, code-graph
//! slices) therefore require the `fs`, `s3`, or remote (daemon) substrates, not
//! git (gonzalo#184).
```

- [ ] **Step 2: Add the daemon-doc note**

In `crates/gonzalo-server/src/bin/gonzalod.rs` module doc, after the env table (before the `Credentials for s3` line or as a trailing paragraph), add:

```rust
//!
//! Blob endpoints (`/v1/blobs`) are served for the `fs` and `s3` substrates,
//! which implement `BlobStore`. Git is not a content-addressed blob store, so a
//! git-backed deployment does not serve blobs (gonzalo#184).
```

- [ ] **Step 3: Build docs to verify no broken intra-doc links**

Run: `cargo doc -p gonzalo-store-git --no-deps 2>&1 | tail -5`
Expected: no warnings about broken links for the new note.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-store-git/src/lib.rs crates/gonzalo-server/src/bin/gonzalod.rs
git commit -m "docs: note GitStore does not serve blobs; fs/s3/remote required (#184)"
```

---

### Task 10: Full verification gate

**Files:** none (verification only).

- [ ] **Step 1: Format**

Run: `cargo fmt --all -- --check`
Expected: clean (no diff). If it complains, run `cargo fmt --all` and re-commit the formatting.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings. Common blob-specific lints to watch: `clippy::result_large_err` on the new gRPC handlers (add `#[allow(clippy::result_large_err)]` consistent with the neighboring handlers if flagged — check how `authorize` is annotated).

- [ ] **Step 3: Build**

Run: `cargo build --workspace --all-targets`
Expected: PASS.

- [ ] **Step 4: Test**

Run: `cargo test --workspace`
Expected: PASS. Confirm the new tests ran: `blob_methods_delegate…`, `http::tests::blob_*` (4), `grpc::tests::grpc_blob*` (3), `blob_put*` (3, store-server), and the 2 integration blob-conformance tests.

- [ ] **Step 5: Commit any fmt/clippy fixups**

```bash
git add -A
git commit -m "chore: fmt/clippy cleanup for blob transport (#184)" || echo "nothing to fix up"
```

---

## Self-Review

**Spec coverage:**
- §1 reserved `_blobs` authz → Tasks 3 (HTTP), 5 (gRPC), tested in both.
- §2 `Service` blob methods + `max_blob_size` → Task 2.
- §3 HTTP routes (get/put/delete/list, 400 mismatch, 413 limit) → Task 3.
- §4 gRPC RPCs + decode limits (server + client) → Tasks 4, 5, 6.
- §5 `ServerStore` `BlobStore` impl → Task 6.
- §6 conformance fresh-daemon-per-call + targeted tests → Task 7 (conformance) + Tasks 3/5 (mismatch/limit/authz) + Task 6 (classify unit tests).
- §7 `gonzalod` env wiring → Task 8.
- §8 GitStore doc note → Task 9.
- `DEFAULT_MAX_BLOB_SIZE` shared const → Task 1.
- Verification gate → Task 10.

**Type consistency:** `ContentHash(pub String)` used as `ContentHash(s)` / `hash.0` throughout; `Service` methods named `get_blob`/`put_blob`/`list_blobs`/`delete_blob` consistently across Tasks 2/3/5; proto messages `PutBlobRequest{hash, content}` / `PutBlobResponse{hash}` / `GetBlobResponse{found, content}` / `ListBlobsResponse{hashes}` used identically in Tasks 4/5/6; `classify_blob_put_response(status, body, hash)` signature matches its call site and tests. `BLOB_NS = "_blobs"` identical in http.rs and grpc.rs.

**Placeholder scan:** no TBD/TODO; every code step carries full code.
