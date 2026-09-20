//! Working-tree writes are atomic (gonzalo#283). Consumer `get` and `list` read
//! the working tree without the repo lock, so a write must replace a record
//! file in one step: a reader sees the old record or the new one, never an
//! empty or half-written file. A crash mid-write may leave a temp file behind;
//! it must never be listed as a record or committed.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use gonzalo_core::{
    Body, Identity, KeyPrefix, Meta, PutResult, Record, RecordKey, RecordKind, Revision, Store,
    record_components,
};
use gonzalo_store_git::GitStore;

fn rec(key: &RecordKey, payload: &[u8], revision: Revision, parent: Option<Revision>) -> Record {
    Record {
        key: key.clone(),
        kind: RecordKind::Topic,
        revision,
        parent,
        body: Body::Inline(payload.to_vec()),
        meta: Meta {
            author: Identity::new("t"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        ancestors: Vec::new(),
        deleted_at: None,
        deleted_blob: None,
    }
}

fn committed(result: PutResult) -> Revision {
    match result {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    }
}

/// Every file under `root` (outside `.git`) whose name ends in `.tmp`.
fn temp_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "tmp") {
                out.push(path.display().to_string());
            }
        }
    }
    out
}

/// The record file for `key` under the store root at `root`.
fn record_file(root: &Path, key: &RecordKey) -> std::path::PathBuf {
    let (ns, col, file) = record_components(key);
    root.join(ns).join(col).join(file)
}

/// Everything readable from `f`, from the start.
fn read_all(mut f: std::fs::File) -> String {
    let mut buf = String::new();
    f.read_to_string(&mut buf).unwrap();
    buf
}

// Whether a write is atomic can't be tested reliably by racing readers against
// it: the torn window is too short to hit on a fast disk. What makes a write
// atomic is that it *replaces* the file (write a temp file, rename it over the
// record) instead of truncating and rewriting the file a reader may have open.
// That is observable deterministically: a handle opened before the write still
// reads the old record afterwards, because it points at the file the rename
// displaced. An in-place rewrite changes the very file the handle is reading.

#[tokio::test]
async fn a_put_replaces_the_record_file_instead_of_rewriting_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).expect("open git store");
    let key = RecordKey::new("ns", "col", "x");
    let rev = committed(
        store
            .put(rec(&key, b"v0", Revision::initial(b"v0"), None), None)
            .await
            .unwrap(),
    );
    let path = record_file(dir.path(), &key);
    let before = std::fs::read_to_string(&path).unwrap();
    let reader = std::fs::File::open(&path).unwrap();

    let next = rev.next(b"v1");
    committed(
        store
            .put(rec(&key, b"v1", next, Some(rev.clone())), Some(rev))
            .await
            .unwrap(),
    );

    assert_eq!(
        read_all(reader),
        before,
        "an open reader must keep seeing the old record, not a rewritten file"
    );
    assert_ne!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "the new record landed"
    );
}

#[tokio::test]
async fn a_delete_replaces_the_record_file_instead_of_rewriting_it() {
    // Since tombstones (#203) a delete writes a file too, so it needs the same
    // guarantee a put has.
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).expect("open git store");
    let key = RecordKey::new("ns", "col", "x");
    let rev = committed(
        store
            .put(rec(&key, b"v0", Revision::initial(b"v0"), None), None)
            .await
            .unwrap(),
    );
    let path = record_file(dir.path(), &key);
    let before = std::fs::read_to_string(&path).unwrap();
    let reader = std::fs::File::open(&path).unwrap();

    let _ = store.delete(&key, Some(rev)).await.unwrap();

    assert_eq!(
        read_all(reader),
        before,
        "an open reader must keep seeing the live record, not a half-written tombstone"
    );
    let after = store
        .get_raw(&key)
        .await
        .unwrap()
        .expect("tombstone stored");
    assert!(after.is_tombstone());
}

#[tokio::test]
async fn puts_and_deletes_leave_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).expect("open git store");
    let key = RecordKey::new("ns", "col", "x");

    let rev = committed(
        store
            .put(rec(&key, b"v0", Revision::initial(b"v0"), None), None)
            .await
            .unwrap(),
    );
    let next = rev.next(b"v1");
    let rev = committed(
        store
            .put(rec(&key, b"v1", next, Some(rev.clone())), Some(rev))
            .await
            .unwrap(),
    );
    let _ = store.delete(&key, Some(rev)).await.unwrap();

    assert!(
        temp_files(dir.path()).is_empty(),
        "stray temp files: {:?}",
        temp_files(dir.path())
    );
}

#[tokio::test]
async fn a_stray_temp_file_is_never_listed_or_committed() {
    // What a crash between writing the temp file and renaming it leaves behind.
    let dir = tempfile::tempdir().unwrap();
    let store = GitStore::open(dir.path()).expect("open git store");
    let live = RecordKey::new("ns", "col", "live");
    committed(
        store
            .put(rec(&live, b"v0", Revision::initial(b"v0"), None), None)
            .await
            .unwrap(),
    );

    let (ns, col, file) = record_components(&RecordKey::new("ns", "col", "crashed"));
    let stray = dir.path().join(&ns).join(&col).join(format!("{file}.tmp"));
    std::fs::write(&stray, b"{ half a record").unwrap();

    let listed = store.list(&KeyPrefix::default()).await.unwrap();
    assert_eq!(
        listed,
        vec![live.clone()],
        "the temp file must not be a key"
    );
    let raw = store.list_raw(&KeyPrefix::default()).await.unwrap();
    assert_eq!(raw, vec![live.clone()], "nor on the replication surface");

    // A later write commits only its own record file.
    let other = RecordKey::new("ns", "col", "other");
    committed(
        store
            .put(rec(&other, b"v0", Revision::initial(b"v0"), None), None)
            .await
            .unwrap(),
    );
    let repo = git2::Repository::open(dir.path()).unwrap();
    let tree = repo.head().unwrap().peel_to_tree().unwrap();
    let rel = format!("{ns}/{col}/{file}.tmp");
    assert!(
        tree.get_path(Path::new(&rel)).is_err(),
        "the temp file must never be committed"
    );
}
