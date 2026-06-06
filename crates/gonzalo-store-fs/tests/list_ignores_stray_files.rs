use gonzalo_core::{
    Body, Identity, KeyPrefix, Meta, Record, RecordKey, RecordKind, Revision, Store,
};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;

fn sample(key: RecordKey) -> Record {
    let body = Body::Inline(b"x".to_vec());
    Record {
        revision: Revision::initial(body.bytes()),
        parent: None,
        body,
        kind: RecordKind::Topic,
        meta: Meta {
            author: Identity::new("t"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        key,
    }
}

#[tokio::test]
async fn list_skips_stray_non_directory_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());

    let key = RecordKey::new("ns", "col", "real");
    let _ = store.put(sample(key.clone()), None).await.unwrap();

    // Stray files at the root level and inside the namespace dir.
    tokio::fs::write(dir.path().join(".DS_Store"), b"junk")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("ns").join(".DS_Store"), b"junk")
        .await
        .unwrap();

    let keys = store.list(&KeyPrefix::default()).await.unwrap();
    assert_eq!(keys, vec![key]);
}
