//! End-to-end: the daemon's HTTP/JSON transport answers code-graph queries
//! (EPIC C, C3). A view is seeded directly into the filesystem store (blobs +
//! manifest record), the daemon is served, and queries go over real HTTP.

use gonzalo_core::{
    BlobStore, Identity, Manifest, Meta, PutResult, Record, RecordKind, Revision, Store,
};
use gonzalo_graph::{Located, Symbol, build_rust};
use gonzalo_server::{Service, serve_http};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Seed view `r`/`main` with two slices + its manifest record.
async fn seed(fs: &FsStore) {
    let mut manifest = Manifest::new();
    for (path, src) in [
        ("lib.rs", "fn helper() {}"),
        ("main.rs", "fn main() { helper(); }"),
    ] {
        let hash = fs
            .put_blob(&build_rust(src).to_slice_bytes())
            .await
            .unwrap();
        manifest.insert(path, hash);
    }
    let body = manifest.to_body();
    let record = Record {
        revision: Revision::initial(body.bytes()),
        parent: None,
        body,
        kind: RecordKind::GraphManifest,
        meta: Meta {
            author: Identity::new("tester"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        key: Manifest::key("r", "main"),
    };
    assert!(matches!(
        fs.put(record, None).await.unwrap(),
        PutResult::Committed(_)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_serves_graph_queries() {
    let fs = Arc::new(FsStore::new(tempfile::tempdir().unwrap().keep()));
    seed(&fs).await;
    let service = Service::new(fs.clone(), fs);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(listener, service, None));
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // definitions -> located results
    let defs: Vec<Located<Symbol>> = client
        .get(format!("{base}/v1/graph/definitions"))
        .query(&[("repo", "r"), ("view", "main"), ("name", "helper")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].path, "lib.rs");

    // impact -> name list (transitive callers)
    let impact: Vec<String> = client
        .get(format!("{base}/v1/graph/impact"))
        .query(&[("repo", "r"), ("view", "main"), ("name", "helper")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(impact, vec!["main".to_string()]);

    // callees -> name list
    let callees: Vec<String> = client
        .get(format!("{base}/v1/graph/callees"))
        .query(&[("repo", "r"), ("view", "main"), ("name", "main")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(callees, vec!["helper".to_string()]);
}
