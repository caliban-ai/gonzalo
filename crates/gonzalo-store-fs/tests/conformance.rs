use gonzalo_core::conformance::run_store_conformance;
use gonzalo_store_fs::FsStore;

#[tokio::test]
async fn fs_store_passes_conformance() {
    run_store_conformance(|| async {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir so the directory survives for the store's lifetime
        // within a single factory invocation; the OS reclaims /tmp on reboot.
        let path = dir.keep();
        FsStore::new(path)
    })
    .await;
}
