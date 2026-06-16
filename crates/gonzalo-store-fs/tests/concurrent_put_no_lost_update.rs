//! The fs store must not lose updates when writers race the same key. A
//! conditional `put` is read-modify-write; without serialization two writers
//! can both read the base revision before either renames, and both "commit",
//! silently clobbering one update. Advisory locking must serialize the
//! critical section so OCC admits exactly one winner.

use gonzalo_core::{
    Body, Identity, Meta, PutResult, Record, RecordKey, RecordKind, Revision, Store,
};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;
use std::sync::Arc;

fn rec(key: RecordKey, payload: &str) -> Record {
    let body = Body::Inline(payload.as_bytes().to_vec());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_with_same_expected_admit_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsStore::new(dir.path()));
    let key = RecordKey::new("ns", "col", "k");

    // Seed the base revision every writer will race against.
    let base_rev = match store.put(rec(key.clone(), "base"), None).await.unwrap() {
        PutResult::Committed(r) => r,
        PutResult::Conflict(_) => panic!("seed should commit"),
    };

    // N writers each conditionally put against the SAME base revision. Each
    // task classifies its own outcome so an unsynchronized critical section's
    // failure modes — a lost update (extra commit) or a temp-file/rename race
    // (error) — both surface instead of one masking the other.
    let n = 24usize;
    let mut handles = Vec::new();
    for i in 0..n {
        let store = Arc::clone(&store);
        let key = key.clone();
        let expected = base_rev.clone();
        handles.push(tokio::spawn(async move {
            match store
                .put(rec(key, &format!("writer-{i}")), Some(expected))
                .await
            {
                Ok(PutResult::Committed(_)) => "committed",
                Ok(PutResult::Conflict(_)) => "conflict",
                Err(_) => "error",
            }
        }));
    }

    let (mut committed, mut conflicts, mut errors) = (0usize, 0usize, 0usize);
    for h in handles {
        match h.await.unwrap() {
            "committed" => committed += 1,
            "conflict" => conflicts += 1,
            _ => errors += 1,
        }
    }

    // Exactly one writer may win; the rest must observe the winner's revision
    // and conflict — with no errors. More than one commit is a lost update; an
    // error is the shared temp/rename race. Locking must rule out both.
    assert_eq!(
        (committed, conflicts, errors),
        (1, n - 1, 0),
        "expected exactly one winner; got {committed} commits, {conflicts} conflicts, {errors} errors"
    );
}
