use gonzalo_core::conformance::run_store_conformance;
use gonzalo_store_git::GitStore;

#[tokio::test]
async fn git_store_passes_conformance() {
    run_store_conformance(|| async {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.keep();
        GitStore::open(path).expect("open git store")
    })
    .await;
}
