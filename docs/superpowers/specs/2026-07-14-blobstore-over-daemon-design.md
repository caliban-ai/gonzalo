# Expose `BlobStore` over the daemon + remote client

- **Ticket:** gonzalo#184
- **Date:** 2026-07-14
- **Status:** Accepted
- **Refs:** `crates/gonzalo-core/src/store.rs:66-89` (`BlobStore`),
  `crates/gonzalo-server/src/{service,http,grpc}.rs`,
  `crates/gonzalo-store-server/src/lib.rs`,
  `crates/gonzalo-core/src/conformance.rs::run_blob_store_conformance`,
  ADR 0007 (dual-transport daemon), ADR 0012 (content-addressed slices),
  ADR 0015 (namespace-scoped auth). Driver: gonzalo#1 (checkpoint blobs).

## Problem

`BlobStore` (content-addressed out-of-line blobs: `put_blob`/`get_blob`/
`list_blobs`/`delete_blob`) is implemented by `FsStore` and `S3Store` but **not**
by the remote client `ServerStore`, and the daemon (`gonzalo-server`) exposes
**no blob endpoints** — its HTTP/gRPC surface is records only. So a remote-backed
consumer gets `Store` but not `BlobStore`.

The caliban integration (gonzalo#1) stores checkpoint pre-image data as
content-addressed binary blobs (dedup by hash). Mapped onto gonzalo these are
`BlobStore` calls, which work on fs/s3 but mean **checkpoints cannot use the
remote (daemon) substrate** until blobs are transportable over the daemon.

## Design

### 1. Authz — reserved `_blobs` namespace

Blobs are namespace-agnostic, but the daemon's authz model (ADR 0015) is
namespace-scoped. We reuse it without a schema change by authorizing blob ops
against a **reserved namespace `_blobs`**:

- blob reads (`GET` one, list) → `Access::Read` on `_blobs`;
- blob writes (`PUT`, `DELETE`) → `Access::Write` on `_blobs`.

Admins (`read`/`write` on `"*"`) and open mode (`Auth::Disabled`) cover it with
no config. An operator grants a scoped principal blob access by adding `_blobs`
to its `read`/`write` lists in the TOML principals file. Follows the existing
reserved-name convention (`_gonzalo`/`_health` in `Service::ready`).

### 2. `Service` carries the blob surface + size limit

`Service` already wraps `Arc<dyn Store>` + `Arc<dyn BlobStore>`. Add:

```rust
pub struct Service { /* store, blobs, graph_root, */ max_blob_size: usize }

impl Service {
    // Service::new(..) defaults max_blob_size to DEFAULT_MAX_BLOB_SIZE.
    pub fn with_max_blob_size(mut self, n: usize) -> Self { self.max_blob_size = n; self }
    pub fn max_blob_size(&self) -> usize { self.max_blob_size }

    pub async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> { self.blobs.get_blob(hash).await }
    pub async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> { self.blobs.put_blob(content).await }
    pub async fn list_blobs(&self) -> Result<Vec<ContentHash>> { self.blobs.list_blobs().await }
    pub async fn delete_blob(&self, hash: &ContentHash) -> Result<()> { self.blobs.delete_blob(hash).await }
}
```

Both transports read `service.max_blob_size()`, so `serve_http`/`serve_grpc`
signatures are unchanged. `DEFAULT_MAX_BLOB_SIZE: usize = 64 * 1024 * 1024`
(64 MiB) lives in `gonzalo-proto` (both `gonzalo-server` and
`gonzalo-store-server` already depend on it), so client and server share one
constant.

### 3. HTTP transport (`http.rs`)

New routes (raw bytes in/out; the shared JSON convention doesn't apply to blob
bodies):

| Route | Handler | Authz | Success | Notes |
|-------|---------|-------|---------|-------|
| `GET /v1/blobs/{hash}` | `get_blob` | Read `_blobs` | `200` `application/octet-stream` raw body, or `404` | — |
| `PUT /v1/blobs/{hash}` | `put_blob` | Write `_blobs` | `200` (empty body) | recompute `ContentHash::of(&body)`; **`400`** on mismatch with `{hash}` |
| `DELETE /v1/blobs/{hash}` | `delete_blob` | Write `_blobs` | `200` | idempotent |
| `GET /v1/blobs` | `list_blobs` | Read `_blobs` | `200` `Json(Vec<ContentHash>)` | `ContentHash` serializes as its hex string |

- Body is extracted with `axum::body::Bytes`. `ContentHash::of(&body)` is
  recomputed server-side and compared to the `{hash}` path segment; a mismatch
  is `400` **before** the write, so the URL hash is authoritative and integrity
  is checked end to end.
- The blob routes carry a `DefaultBodyLimit::max(service.max_blob_size())` layer
  so an oversized `PUT` yields **`413`**. Record routes keep axum's 2 MiB
  default (applied only to the blob sub-router, not globally).
- The auth middleware and `server_error` opaque-500 mapping are unchanged;
  `is_probe_path` is untouched (blob routes are authenticated like records).

### 4. gRPC transport (`gonzalo.proto` + `grpc.rs`)

Add four RPCs and their messages:

```proto
rpc PutBlob(PutBlobRequest) returns (PutBlobResponse);
rpc GetBlob(GetBlobRequest) returns (GetBlobResponse);
rpc ListBlobs(ListBlobsRequest) returns (ListBlobsResponse);
rpc DeleteBlob(DeleteBlobRequest) returns (DeleteBlobResponse);

message PutBlobRequest  { string hash = 1; bytes content = 2; } // hash = expected ContentHash
message PutBlobResponse { string hash = 1; }                    // echo committed hash
message GetBlobRequest  { string hash = 1; }
message GetBlobResponse { bool found = 1; bytes content = 2; }
message ListBlobsRequest  {}
message ListBlobsResponse { repeated string hashes = 1; }
message DeleteBlobRequest  { string hash = 1; }
message DeleteBlobResponse {}
```

- Handlers authorize `_blobs` per call (Read for get/list, Write for
  put/delete), keeping authenticate-before-work.
- `PutBlob` verifies `ContentHash::of(&content).0 == req.hash`, returning
  `Status::invalid_argument` on mismatch — the same end-to-end integrity check
  as HTTP.
- Backend failures map through `internal` to an opaque `Status::internal`
  (#148), consistent with the record handlers.
- **Message size:** raise `max_decoding_message_size(service.max_blob_size())`
  on `GonzaloServer` in `serve_grpc` (server decodes large `PutBlob` content),
  and on the client's `GonzaloClient` to `DEFAULT_MAX_BLOB_SIZE` (client decodes
  large `GetBlob` responses) — tonic's default 4 MiB otherwise rejects 64 MiB
  blobs. Documented limitation: a daemon configured with
  `GONZALO_MAX_BLOB_SIZE` **above** `DEFAULT_MAX_BLOB_SIZE` needs a
  correspondingly-raised client; the client constant is the supported ceiling.

### 5. Remote client — `impl BlobStore for ServerStore`

Mirrors the existing `Store` impl (HTTP + gRPC arms):

- `blobs_url(base, hash?)` helper builds `…/v1/blobs[/{hash}]` (mirrors
  `records_url`).
- `put_blob(content)`: compute `hash = ContentHash::of(content)` locally to
  address the request.
  - HTTP: `PUT /v1/blobs/{hash}` with raw `content` body; classify via a
    `classify_blob_put_response(status)` that returns `Ok(hash)` on `200` and
    surfaces `403`/`413`/`400`/other as `Backend("daemon returned <status>:
    <body>")` (reuses the #147 status+body-text pattern, not a masked decode).
  - gRPC: `PutBlob { hash, content }` → `Ok(hash)`.
- `get_blob(hash)`: HTTP `GET` → `200` bytes / `404` `None`; gRPC `GetBlob` →
  `found ? Some(content) : None`.
- `list_blobs()`: HTTP `GET /v1/blobs` → `Vec<ContentHash>`; gRPC `ListBlobs` →
  map `Vec<String>` to `ContentHash`.
- `delete_blob(hash)`: HTTP `DELETE` → `Ok(())`; gRPC `DeleteBlob`.

`ServerStore` thus offers the full `Store` + `BlobStore` surface.

### 6. Conformance (`gonzalo-integration-tests`)

Add `http_server_store_passes_blob_conformance` and
`grpc_server_store_passes_blob_conformance`, each driving
`run_blob_store_conformance` against a daemon-backed `ServerStore`.

**Fresh-daemon-per-factory-call.** `run_blob_store_conformance` invokes the
factory 5× and one sub-test (`blob_list_reports_stored_hashes`) asserts
`list_blobs()` is empty at start. The record test shares one daemon across
sub-tests because record keys are namespaced per sub-test, but blobs share a
single global keyspace — so each factory call must stand up a **fresh daemon
over a fresh `FsStore`** (new bound listener + spawned server). A helper builds
one per call (gRPC waits briefly for the server to accept).

Targeted transport tests (in the same crate or the transport crates' unit
tests):

- HTTP `PUT /v1/blobs/{hash}` with content not hashing to `{hash}` → `400`.
- HTTP `PUT` of a body over `max_blob_size` → `413`.
- gRPC `PutBlob` with mismatched `hash` → `InvalidArgument`.
- `_blobs` authz: a principal without `_blobs` scope is denied; an admin and a
  `_blobs`-scoped principal succeed.

### 7. `gonzalod` binary wiring

`bin/gonzalod.rs` reads `GONZALO_MAX_BLOB_SIZE` (bytes; default
`DEFAULT_MAX_BLOB_SIZE`) and applies it with
`Service::…with_max_blob_size(n)`. A parse failure is a startup error. The
module doc's env table gains the new variable.

### 8. GitStore — documented out of scope

`GitStore` still does not implement `BlobStore` (git is not a natural
content-addressed blob store). Add a short doc note to the `gonzalo-store-git`
module and the `gonzalod` docs: **blob-backed records require the fs, s3, or
remote substrates, not git.** No code change to `GitStore`.

## Acceptance criteria

- The daemon serves blob put/get/list/delete over HTTP and gRPC; `ServerStore`
  implements `BlobStore` and passes `run_blob_store_conformance` over both
  transports.
- Blob-backed records (e.g. checkpoints) work end-to-end against a remote
  Gonzalo daemon, not just fs/s3.
- Hash-mismatch → `400`/`InvalidArgument`; oversized `PUT` → `413`; `_blobs`
  authz enforced; oversized blobs (up to 64 MiB) transit both transports.

## Non-goals

- `GitStore` blob support (documented out of scope).
- A configurable client-side blob ceiling (fixed at `DEFAULT_MAX_BLOB_SIZE`).
- Streaming/chunked blob transfer (whole-body transfer within the size limit).
