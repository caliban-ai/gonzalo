//! End-to-end: a daemon backed by a filesystem store must serve a remote
//! `ServerStore` that passes the shared conformance suites — over BOTH the
//! HTTP/JSON and gRPC transports. Every factory call stands up a fresh daemon
//! over a fresh `FsStore`, because the suites require a fresh, empty store per
//! invocation.

use gonzalo_core::DEFAULT_ANCESTOR_CAP;
use gonzalo_core::conformance::{
    run_blob_store_conformance, run_store_conformance, run_tombstone_conformance,
};
use gonzalo_server::{Auth, Principal, Service, serve_grpc, serve_http};
use gonzalo_store_fs::FsStore;
use gonzalo_store_server::ServerStore;
use std::sync::Arc;
use tokio::net::TcpListener;

/// A small cap so `ancestors_capped_and_ordered` exercises truncation over the
/// wire in a handful of writes.
const SMALL_CAP: usize = 3;

fn service_with_cap(cap: usize) -> Service {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let fs = Arc::new(
        FsStore::new(dir)
            .with_ancestor_cap(cap)
            .expect("valid ancestor cap"),
    );
    Service::new(fs.clone(), fs)
}

fn open() -> Arc<Auth> {
    Arc::new(Auth::Disabled)
}

/// A fresh open daemon over HTTP whose backing store uses `cap`.
async fn fresh_http_store(cap: usize) -> ServerStore {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(listener, service_with_cap(cap), open()));
    ServerStore::http(&format!("http://{addr}")).unwrap()
}

/// As `fresh_http_store`, over gRPC (waits briefly for the server to accept).
async fn fresh_grpc_store(cap: usize) -> ServerStore {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_grpc(listener, service_with_cap(cap), open()));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    ServerStore::grpc(format!("http://{addr}")).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_conformance() {
    run_store_conformance(|| fresh_http_store(DEFAULT_ANCESTOR_CAP)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_passes_conformance() {
    run_store_conformance(|| fresh_grpc_store(DEFAULT_ANCESTOR_CAP)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_tombstone_conformance_small_cap() {
    run_tombstone_conformance(|| fresh_http_store(SMALL_CAP), SMALL_CAP).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_passes_tombstone_conformance_small_cap() {
    run_tombstone_conformance(|| fresh_grpc_store(SMALL_CAP), SMALL_CAP).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_passes_blob_conformance() {
    run_blob_store_conformance(|| fresh_http_store(DEFAULT_ANCESTOR_CAP)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_passes_blob_conformance() {
    run_blob_store_conformance(|| fresh_grpc_store(DEFAULT_ANCESTOR_CAP)).await;
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
    tokio::spawn(serve_http(
        listener,
        service_with_cap(DEFAULT_ANCESTOR_CAP),
        auth,
    ));
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

/// Purge over a real daemon is admin-only end to end: a namespace writer's
/// `ServerStore::purge` fails with the daemon's 403, not the upgrade error.
#[tokio::test(flavor = "multi_thread")]
async fn http_purge_by_non_admin_is_forbidden_end_to_end() {
    use gonzalo_core::{Revision, Store};
    use gonzalo_store_server::DAEMON_PREDATES_REPLICATION;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let auth = Arc::new(Auth::Enabled(std::collections::HashMap::from([(
        "wtok".to_string(),
        Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
    )])));
    tokio::spawn(serve_http(
        listener,
        service_with_cap(DEFAULT_ANCESTOR_CAP),
        auth,
    ));
    let writer = ServerStore::http_with_token(&format!("http://{addr}"), "wtok").unwrap();
    let key = gonzalo_core::RecordKey::new("memory", "col", "x");
    let msg = writer
        .purge(&key, Revision::initial(b"x"))
        .await
        .unwrap_err()
        .to_string();
    assert!(msg.contains("403"), "{msg}");
    assert!(!msg.contains(DAEMON_PREDATES_REPLICATION), "{msg}");
}

/// Sync against a daemon peer replicates a deletion: the tombstone crosses the
/// wire through the raw routes (`list_raw`, `get_raw`, `put_raw`), the peer hides
/// the key, and the pair is then in sync (gonzalo#203 §3.4).
#[tokio::test(flavor = "multi_thread")]
async fn sync_replicates_a_tombstone_to_a_daemon_peer() {
    use gonzalo_core::{
        Body, DeleteResult, Identity, Meta, PutResult, Record, RecordKey, RecordKind, Revision,
        Store, SyncReport, sync,
    };
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().expect("tempdir");
    let a = FsStore::new(dir.path());
    let b = fresh_http_store(DEFAULT_ANCESTOR_CAP).await;

    let key = RecordKey::new("testns", "testcol", "d");
    let body = Body::Inline(b"v0\n".to_vec());
    let record = Record {
        revision: Revision::initial(body.bytes()),
        parent: None,
        body,
        kind: RecordKind::Topic,
        meta: Meta {
            author: Identity::new("tester"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        key: key.clone(),
        ancestors: Vec::new(),
        deleted_at: None,
    };
    assert!(matches!(
        a.put(record, None).await.unwrap(),
        PutResult::Committed(_)
    ));

    let _ = sync(&a, &b).await.unwrap();
    assert!(b.get(&key).await.unwrap().is_some());

    assert!(matches!(
        a.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    ));

    let report = sync(&a, &b).await.unwrap();
    assert_eq!(report.fast_forwarded_to_b, vec![key.clone()]);
    assert!(b.get(&key).await.unwrap().is_none());
    let ta = a.get_raw(&key).await.unwrap().unwrap();
    let tb = b.get_raw(&key).await.unwrap().unwrap();
    assert!(tb.is_tombstone());
    assert_eq!(tb.revision, ta.revision);

    assert_eq!(sync(&a, &b).await.unwrap(), SyncReport::default());
}
