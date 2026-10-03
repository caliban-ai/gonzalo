//! `undelete` over a real store with real blobs (ADR 0029).

use gonzalo_core::{
    BlobStore, Body, ContentHash, CoreError, DeleteResult, Identity, Meta, PutResult, Record,
    RecordKey, RecordKind, Store, VectorManifest, undelete,
};
use gonzalo_store_fs::FsStore;

fn fresh_store() -> FsStore {
    let dir = tempfile::tempdir().expect("tempdir");
    FsStore::new(dir.keep())
}

fn meta() -> Meta {
    Meta::new(Identity::new("tester"), "test")
}

fn manifest_key() -> RecordKey {
    VectorManifest::key("ns", "memories")
}

/// A store holding one vector index with a single shard blob.
async fn store_with_index() -> (FsStore, ContentHash) {
    let store = fresh_store();
    let shard = store.put_blob(b"shard bytes").await.unwrap();
    let mut m = VectorManifest::new("space-a", 3, 256);
    m.entries.insert(0, shard.clone());
    let record = Record::create(
        manifest_key(),
        RecordKind::VectorManifest,
        m.to_body(),
        meta(),
    );
    assert!(matches!(
        store.put(record, None).await.unwrap(),
        PutResult::Committed(_)
    ));
    (store, shard)
}

// The capability: delete an index, put it back, and it is the same record.
#[tokio::test]
async fn undelete_restores_a_deleted_manifest() {
    let (store, _shard) = store_with_index().await;
    let before = store.get(&manifest_key()).await.unwrap().unwrap();
    assert_eq!(
        store.delete(&manifest_key(), None).await.unwrap(),
        DeleteResult::Deleted
    );
    assert!(
        store.get(&manifest_key()).await.unwrap().is_none(),
        "hidden while deleted"
    );

    undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap();

    let after = store.get(&manifest_key()).await.unwrap().expect("restored");
    assert_eq!(after.kind, RecordKind::VectorManifest);
    assert_eq!(after.body, before.body, "the manifest comes back intact");
    assert_eq!(after.deleted_at, None);
    assert_eq!(after.deleted_kind, None);
}

// Provenance and lineage: `created` survives, and the restored record succeeds
// the tombstone rather than racing it.
#[tokio::test]
async fn undelete_preserves_created_and_continues_the_chain() {
    let (store, _shard) = store_with_index().await;
    let before = store.get(&manifest_key()).await.unwrap().unwrap();
    let _ = store.delete(&manifest_key(), None).await.unwrap();
    let t = store.get_raw(&manifest_key()).await.unwrap().unwrap();

    undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap();

    let after = store.get(&manifest_key()).await.unwrap().unwrap();
    assert_eq!(after.meta.created, before.meta.created, "created survives");
    assert_eq!(after.meta.updated, 9_000);
    assert_eq!(after.revision.counter, t.revision.counter + 1);
    assert_eq!(after.parent, Some(t.revision.clone()));
    assert_eq!(
        after.ancestors.first(),
        Some(&t.revision),
        "put_raw folds the tombstone into the chain"
    );
}

// An explicit author overrides the tombstone's, for a daemon-stamped restore.
#[tokio::test]
async fn undelete_stamps_an_explicit_author() {
    let (store, _shard) = store_with_index().await;
    let _ = store.delete(&manifest_key(), None).await.unwrap();

    let operator = Identity::new("operator");
    undelete(&store, &manifest_key(), 9_000, Some(&operator))
        .await
        .unwrap();

    let after = store.get(&manifest_key()).await.unwrap().unwrap();
    assert_eq!(after.meta.author, operator);
}

#[tokio::test]
async fn undelete_refuses_when_there_is_no_tombstone() {
    let store = fresh_store();
    let err = undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn undelete_refuses_when_the_record_is_live() {
    let (store, _shard) = store_with_index().await;
    let err = undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Invalid(_)), "got {err:?}");
}

// A kind whose body was never retained cannot be restored. This is the stated
// asymmetry, and the message has to explain it rather than just failing.
#[tokio::test]
async fn undelete_refuses_a_tombstone_whose_body_was_not_retained() {
    let store = fresh_store();
    let key = RecordKey::new("ns", "coll", "topic");
    let record = Record::create(
        key.clone(),
        RecordKind::Topic,
        Body::Inline(b"{}".to_vec()),
        meta(),
    );
    let _ = store.put(record, None).await.unwrap();
    let _ = store.delete(&key, None).await.unwrap();

    let err = undelete(&store, &key, 9_000, None).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not retained"),
        "the message must explain why, got {msg}"
    );
}

// Creating a record for an index that cannot be opened is worse than refusing,
// so the blobs are checked before anything is written.
#[tokio::test]
async fn undelete_refuses_when_a_named_blob_is_missing() {
    let (store, shard) = store_with_index().await;
    let _ = store.delete(&manifest_key(), None).await.unwrap();
    store.delete_blob(&shard).await.unwrap();

    let err = undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains(&shard.0),
        "the message names the missing hash: {err}"
    );
    assert!(
        store.get(&manifest_key()).await.unwrap().is_none(),
        "nothing was written"
    );
}

// Someone recreated the key while the operator was deciding. The live-record
// guard is what fires here: `undelete` reads the key and finds a non-tombstone.
// (`put_raw` returning Conflict is the narrower race where a recreate lands
// between that read and the write; a single-threaded test cannot reach it, so
// nothing below asserts on it.)
#[tokio::test]
async fn undelete_refuses_when_the_key_was_recreated() {
    let (store, _shard) = store_with_index().await;
    let _ = store.delete(&manifest_key(), None).await.unwrap();

    let replacement = Record::create(
        manifest_key(),
        RecordKind::Topic,
        Body::Inline(b"{}".to_vec()),
        meta(),
    );
    let _ = store.put(replacement, None).await.unwrap();

    let err = undelete(&store, &manifest_key(), 9_000, None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Invalid(_)), "got {err:?}");
    let live = store.get(&manifest_key()).await.unwrap().unwrap();
    assert_eq!(
        live.kind,
        RecordKind::Topic,
        "the live record was not clobbered"
    );
}
