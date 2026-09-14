use gonzalo_core::DEFAULT_ANCESTOR_CAP;
use gonzalo_core::conformance::{run_store_conformance, run_tombstone_conformance};
use gonzalo_store_fs::FsStore;

/// A small cap makes `ancestors_capped_and_ordered` and
/// `put_raw_truncates_ancestors_and_excludes_own_revision` exercise truncation
/// after a handful of writes instead of 32+.
const SMALL_CAP: usize = 3;

/// A fresh, empty store root. The TempDir is leaked so the directory survives
/// for the store's lifetime within a single factory invocation; the OS
/// reclaims /tmp on reboot.
fn fresh_root() -> std::path::PathBuf {
    tempfile::tempdir().expect("tempdir").keep()
}

#[tokio::test]
async fn fs_store_passes_conformance() {
    run_store_conformance(|| async { FsStore::new(fresh_root()) }).await;
}

#[tokio::test]
async fn fs_store_passes_tombstone_conformance_default_cap() {
    run_tombstone_conformance(
        || async { FsStore::new(fresh_root()) },
        DEFAULT_ANCESTOR_CAP,
    )
    .await;
}

#[tokio::test]
async fn fs_store_passes_tombstone_conformance_small_cap() {
    run_tombstone_conformance(
        || async {
            FsStore::new(fresh_root())
                .with_ancestor_cap(SMALL_CAP)
                .expect("cap 3 is valid")
        },
        SMALL_CAP,
    )
    .await;
}
