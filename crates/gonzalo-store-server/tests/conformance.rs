//! End-to-end: a daemon backed by a filesystem store must serve a remote
//! `ServerStore` that passes the shared conformance suite — over BOTH the
//! HTTP/JSON and gRPC transports.

use gonzalo_core::conformance::run_store_conformance;
use gonzalo_server::{Service, serve_grpc, serve_http};
use gonzalo_store_fs::FsStore;
use gonzalo_store_server::ServerStore;
use std::sync::Arc;
use tokio::net::TcpListener;

async fn fresh_service() -> Service {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    Service::new(Arc::new(FsStore::new(dir)))
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_conformance() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(listener, fresh_service().await, None));
    let base = format!("http://{addr}");

    run_store_conformance(|| {
        let base = base.clone();
        async move { ServerStore::http(&base).unwrap() }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_passes_conformance() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_grpc(listener, fresh_service().await, None));
    // Give the spawned server a moment to start accepting.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let endpoint = format!("http://{addr}");

    run_store_conformance(|| {
        let endpoint = endpoint.clone();
        async move { ServerStore::grpc(endpoint).await.unwrap() }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_auth_rejects_wrong_token_and_accepts_correct() {
    use gonzalo_core::{KeyPrefix, Store};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(
        listener,
        fresh_service().await,
        Some("s3cret".to_string()),
    ));
    let base = format!("http://{addr}");

    // No token / wrong token -> error (401 surfaced as a backend error).
    let anon = ServerStore::http(&base).unwrap();
    assert!(anon.list(&KeyPrefix::default()).await.is_err());
    let wrong = ServerStore::http_with_token(&base, "nope").unwrap();
    assert!(wrong.list(&KeyPrefix::default()).await.is_err());

    // Correct token -> ok.
    let ok = ServerStore::http_with_token(&base, "s3cret").unwrap();
    assert!(ok.list(&KeyPrefix::default()).await.is_ok());
}
