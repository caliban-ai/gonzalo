//! The bounded per-PR HA soak gate.
//!
//! Spawns 3 real `gonzalod` replicas over a shared S3 backend, runs a
//! contended + unique-key workload while killing and recovering one replica, and
//! asserts gonzalo's invariants (no lost update, conflicts surface, durability,
//! liveness). **Skips** (does not fail) unless a S3 target is configured via
//! `GONZALO_S3_TEST_ENDPOINT` / `GONZALO_S3_TEST_BUCKET` / `AWS_*` — mirroring the
//! existing S3 integration test. See `scripts/rustfs-up.sh` and the `ha-soak` CI
//! job for provisioning; requires the `gonzalod` binary to be built.

use gonzalo_soak::harness::run_rounds;
use gonzalo_soak::oracle::OpResult;
use gonzalo_soak::target::S3Target;
use gonzalo_soak::workload::WorkloadConfig;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ha_soak_bounded() {
    let Some(target) = S3Target::from_process_env() else {
        eprintln!(
            "skipping ha_soak_bounded: set GONZALO_S3_TEST_ENDPOINT, GONZALO_S3_TEST_BUCKET, \
             AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (see scripts/rustfs-up.sh) to run"
        );
        return;
    };

    let cfg = WorkloadConfig {
        writers: 8,
        shared_keys: 4,
        ops_per_writer: 25,
        unique_per_writer: 3,
        max_conflict_retries: 50,
        ..Default::default()
    };

    let outcomes = run_rounds(&target, cfg, 3, 1)
        .await
        .expect("bounded soak ran");
    let outcome = &outcomes[0];
    let committed = outcome
        .stats
        .ops
        .iter()
        .filter(|o| o.result == OpResult::Committed)
        .count();

    assert!(
        outcome.passed(),
        "HA soak invariant violations: {:?}\n\
         committed={committed} conflicts={} writers={}/{}",
        outcome.violations,
        outcome.stats.conflicts_observed,
        outcome.stats.writers_completed,
        outcome.stats.writers_total,
    );
}
