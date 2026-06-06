use gonzalo_core::conformance::run_store_conformance;
use gonzalo_store_s3::S3Store;

#[tokio::test]
async fn s3_store_passes_conformance_when_endpoint_configured() {
    let (Ok(endpoint), Ok(bucket)) = (
        std::env::var("GONZALO_S3_TEST_ENDPOINT"),
        std::env::var("GONZALO_S3_TEST_BUCKET"),
    ) else {
        eprintln!("skipping: set GONZALO_S3_TEST_ENDPOINT and GONZALO_S3_TEST_BUCKET to run");
        return;
    };
    run_store_conformance(|| async {
        S3Store::connect(bucket.clone(), Some(endpoint.clone())).await
    })
    .await;
}
