//! A remote `ServerStore` must answer `has_blob` without downloading the blob
//! (gonzalo#329).
//!
//! `BlobStore::has_blob` is defaulted, and the default fetches the blob and
//! discards the bytes. `FsStore` and `S3Store` override it with a cheap
//! existence check, but `ServerStore` kept the default — so a writer confirming
//! its referenced blobs survived a commit (ADR 0028) paid a full download per
//! blob, 600 KB per shard for a vector index.
//!
//! The proof here is structural rather than a byte count: the daemon is backed
//! by a blob store that stores, lists and reports presence normally but refuses
//! to hand back bytes. Any `has_blob` implemented as a download fails against
//! it. That makes this a contract test — `has_blob` must not need `get_blob` —
//! and it holds over both transports without measuring anything.

use gonzalo_core::{BlobEntry, BlobStore, ContentHash, CoreError, DEFAULT_ANCESTOR_CAP, Result};
use gonzalo_server::{Auth, Principal, Service, serve_grpc, serve_http};
use gonzalo_store_fs::FsStore;
use gonzalo_store_server::ServerStore;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Forwards every blob operation to an `FsStore` except `get_blob`, which
/// fails. A `has_blob` that downloads surfaces as a backend error.
struct PresenceOnlyBlobs(FsStore);

#[async_trait::async_trait]
impl BlobStore for PresenceOnlyBlobs {
    async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
        self.0.put_blob(content).await
    }
    async fn get_blob(&self, _hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        Err(CoreError::Backend("has_blob must not read the blob".into()))
    }
    async fn list_blobs(&self) -> Result<Vec<BlobEntry>> {
        self.0.list_blobs().await
    }
    async fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        self.0.has_blob(hash).await
    }
    async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
        self.0.delete_blob(hash).await
    }
}

/// A daemon whose blobs are presence-only, plus the hash of one stored blob.
async fn presence_only_service() -> (Service, ContentHash) {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let fs = Arc::new(
        FsStore::new(dir.clone())
            .with_ancestor_cap(DEFAULT_ANCESTOR_CAP)
            .expect("valid ancestor cap"),
    );
    let blobs = Arc::new(PresenceOnlyBlobs(FsStore::new(dir)));
    // Written through the wrapper, so the bytes really are on disk — only
    // reading them back is refused.
    let hash = blobs
        .put_blob(b"a shard whose bytes must stay on disk")
        .await
        .expect("the fixture blob is written");
    (Service::new(fs, blobs), hash)
}

fn open() -> Arc<Auth> {
    Arc::new(Auth::Disabled)
}

async fn assert_presence_without_download(store: ServerStore, stored: &ContentHash) {
    assert!(
        store.has_blob(stored).await.expect(
            "has_blob must answer from a presence check, not a download: a download fails \
             against a presence-only store"
        ),
        "the stored blob is present"
    );

    let absent = ContentHash::of(b"never stored by this test");
    assert!(
        !store
            .has_blob(&absent)
            .await
            .expect("an absent blob is a false, not an error"),
        "an unstored blob is absent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_has_blob_does_not_download() {
    let (service, stored) = presence_only_service().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(listener, service, open()));
    let store = ServerStore::http(&format!("http://{addr}")).unwrap();
    assert_presence_without_download(store, &stored).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_has_blob_does_not_download() {
    let (service, stored) = presence_only_service().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_grpc(listener, service, open()));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let store = ServerStore::grpc(format!("http://{addr}")).await.unwrap();
    assert_presence_without_download(store, &stored).await;
}

/// A refusal is not an absence.
///
/// `has_blob` reports presence as a bool, so a denied request must surface as
/// an error rather than `false`. Reading a `403` as "absent" would tell a
/// writer the blob it just referenced had vanished — a wrong answer, where an
/// error is merely a failure.
#[tokio::test(flavor = "multi_thread")]
async fn http_server_store_has_blob_does_not_read_a_refusal_as_absence() {
    let (service, stored) = presence_only_service().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // `wtok` may read and write `memory`, and so holds no scope on `_blobs`.
    let auth = Arc::new(Auth::Enabled(std::collections::HashMap::from([(
        "wtok".to_string(),
        Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
    )])));
    tokio::spawn(serve_http(listener, service, auth));
    let store = ServerStore::http_with_token(&format!("http://{addr}"), "wtok").unwrap();

    let msg = store
        .has_blob(&stored)
        .await
        .expect_err("a principal without `_blobs` read is refused, not told `false`")
        .to_string();
    assert!(msg.contains("403"), "{msg}");
}

/// As above, over gRPC: `PermissionDenied` is an error, and must not be
/// confused with the "upgrade gonzalod" message a missing RPC earns.
#[tokio::test(flavor = "multi_thread")]
async fn grpc_server_store_has_blob_does_not_read_a_refusal_as_absence() {
    let (service, stored) = presence_only_service().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let auth = Arc::new(Auth::Enabled(std::collections::HashMap::from([(
        "wtok".to_string(),
        Principal::new("writer", vec!["memory".into()], vec!["memory".into()]),
    )])));
    tokio::spawn(serve_grpc(listener, service, auth));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let store = ServerStore::grpc_with_token(format!("http://{addr}"), "wtok")
        .await
        .unwrap();

    let msg = store
        .has_blob(&stored)
        .await
        .expect_err("a principal without `_blobs` read is refused, not told `false`")
        .to_string();
    assert!(msg.contains("PermissionDenied"), "{msg}");
    assert!(
        !msg.contains(gonzalo_store_server::DAEMON_PREDATES_BLOB_PRESENCE),
        "a refusal is not a missing RPC: {msg}"
    );
}
