//! `gonzalod` — the gonzalo daemon binary. Serves a record + blob store over
//! gRPC and HTTP/JSON. Configuration via environment variables:
//!
//! - `GONZALO_STORE`     — substrate: `fs` (default) or `s3`
//! - `GONZALO_ROOT`      — fs store root directory (default `./gonzalo-data`)
//! - `GONZALO_S3_BUCKET` — s3 bucket (required when `GONZALO_STORE=s3`)
//! - `GONZALO_S3_ENDPOINT` — s3 endpoint for MinIO/Garage (optional)
//! - `GONZALO_S3_REGION` — s3 region override (optional)
//! - `GONZALO_HTTP_ADDR` — HTTP/JSON bind address (default `127.0.0.1:8080`)
//! - `GONZALO_GRPC_ADDR` — gRPC bind address (default `127.0.0.1:50051`)
//! - `GONZALO_MAX_BLOB_SIZE` — max bytes per blob over the transports (default 64 MiB)
//! - `GONZALO_AUTH_FILE` — TOML principals file for namespace-scoped auth
//! - `GONZALO_TOKEN`     — single admin token (used when no auth file is set)
//!
//! Credentials for s3 come from the standard `AWS_*` environment.

use gonzalo_core::{BlobStore, Store};
use gonzalo_server::{Auth, Service, StoreConfig, serve_grpc, serve_http};
use gonzalo_store_fs::FsStore;
use gonzalo_store_s3::S3Store;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let http_addr = std::env::var("GONZALO_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let grpc_addr = std::env::var("GONZALO_GRPC_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".into());

    // GONZALO_AUTH_FILE (scoped principals) > GONZALO_TOKEN (single admin) > open.
    let auth = Arc::new(Auth::from_env(
        |k| std::env::var(k).ok(),
        |path| std::fs::read_to_string(path).map_err(|e| e.to_string()),
    )?);
    let auth_on = !matches!(*auth, Auth::Disabled);

    // Select the storage substrate from the environment. One store backs both
    // the record store and the content-addressed blob store (each backend
    // implements both traits).
    let config = StoreConfig::from_env(|k| std::env::var(k).ok())?;
    let (store, blobs, graph_root): (
        Arc<dyn Store>,
        Arc<dyn BlobStore>,
        Option<std::path::PathBuf>,
    ) = match &config {
        StoreConfig::Fs { root } => {
            // Per-view SQLite graphs written by `gonzalo index` live under
            // `<root>/graphs` and are queried directly.
            let fs = Arc::new(FsStore::new(root));
            let graphs = std::path::Path::new(root).join("graphs");
            (fs.clone(), fs, Some(graphs))
        }
        StoreConfig::S3 {
            bucket,
            endpoint,
            region,
        } => {
            // No local SQLite graph cache under S3: views assemble from the
            // manifest + content-addressed slices (blobs) on demand.
            let s3 =
                Arc::new(S3Store::connect(bucket.clone(), endpoint.clone(), region.clone()).await);
            (s3.clone(), s3, None)
        }
    };
    let mut service = Service::new(store, blobs);
    // Optional per-blob size ceiling (bytes). Defaults to the shared constant;
    // a malformed value is a hard startup error rather than a silent fallback.
    let max_blob_size = match std::env::var("GONZALO_MAX_BLOB_SIZE") {
        Ok(v) if !v.is_empty() => v
            .parse::<usize>()
            .map_err(|e| format!("GONZALO_MAX_BLOB_SIZE must be a byte count: {e}"))?,
        _ => gonzalo_proto::DEFAULT_MAX_BLOB_SIZE,
    };
    service = service.with_max_blob_size(max_blob_size);
    if let Some(graph_root) = graph_root {
        service = service.with_graph_root(graph_root);
    }

    let substrate = match &config {
        StoreConfig::Fs { root } => format!("fs({root})"),
        StoreConfig::S3 { bucket, .. } => format!("s3({bucket})"),
    };
    let http_listener = tokio::net::TcpListener::bind(&http_addr).await?;
    let grpc_listener = tokio::net::TcpListener::bind(&grpc_addr).await?;
    eprintln!(
        "gonzalod: store {substrate}, HTTP on {http_addr}, gRPC on {grpc_addr}, auth {}",
        if auth_on { "on" } else { "off" }
    );

    let http = tokio::spawn(serve_http(http_listener, service.clone(), auth.clone()));
    let grpc = tokio::spawn(serve_grpc(grpc_listener, service, auth));

    tokio::select! {
        r = http => { r??; }
        r = grpc => { r??; }
        _ = tokio::signal::ctrl_c() => { eprintln!("gonzalod: shutting down"); }
    }
    Ok(())
}
