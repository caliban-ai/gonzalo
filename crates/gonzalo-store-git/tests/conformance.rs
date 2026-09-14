use gonzalo_core::DEFAULT_ANCESTOR_CAP;
use gonzalo_core::conformance::{run_store_conformance, run_tombstone_conformance};
use gonzalo_store_git::GitStore;

/// A small cap makes `ancestors_capped_and_ordered` and
/// `put_raw_truncates_ancestors_and_excludes_own_revision` exercise truncation
/// after a handful of writes instead of 32+.
const SMALL_CAP: usize = 3;

/// A freshly initialized git store in a leaked TempDir (it must outlive the
/// factory invocation).
fn fresh_store() -> GitStore {
    let path = tempfile::tempdir().expect("tempdir").keep();
    GitStore::open(path).expect("open git store")
}

#[tokio::test]
async fn git_store_passes_conformance() {
    run_store_conformance(|| async { fresh_store() }).await;
}

#[tokio::test]
async fn git_store_passes_tombstone_conformance_default_cap() {
    run_tombstone_conformance(|| async { fresh_store() }, DEFAULT_ANCESTOR_CAP).await;
}

#[tokio::test]
async fn git_store_passes_tombstone_conformance_small_cap() {
    run_tombstone_conformance(
        || async {
            fresh_store()
                .with_ancestor_cap(SMALL_CAP)
                .expect("cap 3 is valid")
        },
        SMALL_CAP,
    )
    .await;
}
