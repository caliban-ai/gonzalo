# Gonzalo M3 — Daemon (gRPC + HTTP/JSON) Design & Build Notes

- **Date:** 2026-06-05
- **Status:** Implemented on `main`
- Implements design spec §3 (proto/server/server-client crates) and §9 (daemon, identity & auth).

## What shipped

Three crates plus a facade feature:

- **`gonzalo-proto`** — the shared wire schema. `proto/gonzalo.proto` defines a
  tiny `Gonzalo` gRPC service (`Get`/`Put`/`List`); a `build.rs` generates the
  stubs using `tonic-build` with a **vendored `protoc`** (`protoc-bin-vendored`)
  so no system protobuf install is required. A `http` module holds the JSON
  DTOs (`PutBody`, `PutOutcome`) used by the HTTP transport. Crucially, both
  transports carry **JSON-encoded `gonzalo-core` types as bytes** — the proto
  messages just wrap `bytes record_json` etc. — so the schema stays tiny and the
  two transports share one serialization (`gonzalo-core::Record` already derives
  serde; `Conflict` gained serde in this milestone).
- **`gonzalo-server`** — the daemon. A transport-agnostic `Service` wraps an
  `Arc<dyn Store>`; the gRPC (`tonic`) and HTTP/JSON (`axum`) transports are
  thin adapters over it. `serve_grpc` / `serve_http` each take an already-bound
  `TcpListener` and an `Option<String>` auth token. A `gonzalod` binary serves a
  filesystem-backed store over both transports (configured via `GONZALO_ROOT` /
  `GONZALO_HTTP_ADDR` / `GONZALO_GRPC_ADDR` / `GONZALO_TOKEN`).
- **`gonzalo-store-server`** — the client substrate. `ServerStore` implements
  the generic `Store` over a remote daemon, choosing transport at construction:
  `http` / `http_with_token` (reqwest) or `grpc` / `grpc_with_token` (tonic).

The facade gains a `remote` feature re-exporting `ServerStore`.

## Auth (§9)

Token-based bearer auth, opt-in. When the daemon is started with a token, the
HTTP transport enforces it via an axum middleware (`Authorization: Bearer …`)
and the gRPC transport via a tonic interceptor (same header in metadata). The
client sends it when constructed with `*_with_token`. **Namespace-scoped
permissions are deferred** (spec §9 explicitly designs for this to slot in
later); the interceptor/middleware is the obvious seam.

## Verification

- An end-to-end integration test stands up a daemon backed by a temp `FsStore`
  and runs the shared **conformance suite** against a remote `ServerStore` over
  **both** HTTP and gRPC — proving the client substrate is a conformant `Store`.
- An auth test confirms missing/wrong tokens are rejected and the correct token
  is accepted.
- `cargo clippy --workspace -D warnings` and `cargo fmt --check` clean.

## Decisions / notes

- **JSON-over-gRPC payloads** (rather than mirroring the full `Record` graph in
  protobuf) was chosen for DRY and to keep both transports byte-identical on the
  wire. If a future consumer needs language-native protobuf messages, the schema
  can be expanded without changing the service shape.
- **Vendored protoc** keeps the build self-contained (no system dependency).
  `gonzalo-proto` uses a local `unsafe_code = "deny"` (not the workspace
  `forbid`) solely so its build script can set the `PROTOC` env var; library
  code remains unsafe-free.
- gRPC `serve_grpc` carries `#[allow(clippy::result_large_err)]` because the
  interceptor's `Result<_, tonic::Status>` type is fixed by tonic's API.
