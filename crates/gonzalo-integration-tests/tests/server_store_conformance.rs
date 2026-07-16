//! End-to-end: a daemon backed by a filesystem store must serve a remote
//! `ServerStore` that passes the shared conformance suite — over BOTH the
//! HTTP/JSON and gRPC transports.

use gonzalo_core::conformance::{run_blob_store_conformance, run_store_conformance};
use gonzalo_server::{Auth, Principal, Service, serve_grpc, serve_http};
use gonzalo_store_fs::FsStore;
use gonzalo_store_server::ServerStore;
use std::sync::Arc;
use tokio::net::TcpListener;

async fn fresh_service() -> Service {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let fs = Arc::new(FsStore::new(dir));
    Service::new(fs.clone(), fs)
}

fn open() -> Arc<Auth> {
    Arc::new(Auth::Disabled)
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_conformance() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(listener, fresh_service().await, open()));
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
    tokio::spawn(serve_grpc(listener, fresh_service().await, open()));
    // Give the spawned server a moment to start accepting.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let endpoint = format!("http://{addr}");

    run_store_conformance(|| {
        let endpoint = endpoint.clone();
        async move { ServerStore::grpc(endpoint).await.unwrap() }
    })
    .await;
}

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

#[tokio::test(flavor = "multi_thread")]
async fn http_auth_rejects_wrong_token_and_accepts_correct() {
    use gonzalo_core::{KeyPrefix, Store};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let auth = Arc::new(Auth::Enabled(std::collections::HashMap::from([(
        "s3cret".to_string(),
        Principal::admin("admin"),
    )])));
    tokio::spawn(serve_http(listener, fresh_service().await, auth));
    let base = format!("http://{addr}");

    // No token / wrong token -> error (401 surfaced as a backend error).
    let anon = ServerStore::http(&base).unwrap();
    assert!(anon.list(&KeyPrefix::default()).await.is_err());
    let wrong = ServerStore::http_with_token(&base, "nope").unwrap();
    assert!(wrong.list(&KeyPrefix::default()).await.is_err());

    // Correct admin token -> ok (admin may list across all namespaces).
    let ok = ServerStore::http_with_token(&base, "s3cret").unwrap();
    assert!(ok.list(&KeyPrefix::default()).await.is_ok());
}
