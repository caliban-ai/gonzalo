//! `gonzalod` — the gonzalo daemon binary. Serves a filesystem-backed store
//! over gRPC and HTTP/JSON. Configuration via environment variables:
//!
//! - `GONZALO_ROOT`      — store root directory (default `./gonzalo-data`)
//! - `GONZALO_HTTP_ADDR` — HTTP/JSON bind address (default `127.0.0.1:8080`)
//! - `GONZALO_GRPC_ADDR` — gRPC bind address (default `127.0.0.1:50051`)
//! - `GONZALO_TOKEN`     — if set, require `Authorization: Bearer <token>`

use gonzalo_server::{Service, serve_grpc, serve_http};
use gonzalo_store_fs::FsStore;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::var("GONZALO_ROOT").unwrap_or_else(|_| "./gonzalo-data".into());
    let http_addr = std::env::var("GONZALO_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let grpc_addr = std::env::var("GONZALO_GRPC_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".into());

    let token = std::env::var("GONZALO_TOKEN").ok();

    let store = Arc::new(FsStore::new(root));
    let service = Service::new(store);

    let http_listener = tokio::net::TcpListener::bind(&http_addr).await?;
    let grpc_listener = tokio::net::TcpListener::bind(&grpc_addr).await?;
    eprintln!(
        "gonzalod: HTTP on {http_addr}, gRPC on {grpc_addr}, auth {}",
        if token.is_some() { "on" } else { "off" }
    );

    let http = tokio::spawn(serve_http(http_listener, service.clone(), token.clone()));
    let grpc = tokio::spawn(serve_grpc(grpc_listener, service, token));

    tokio::select! {
        r = http => { r??; }
        r = grpc => { r??; }
        _ = tokio::signal::ctrl_c() => { eprintln!("gonzalod: shutting down"); }
    }
    Ok(())
}
