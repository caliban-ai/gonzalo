//! `gonzalo-soak` — the deep/parameterized HA soak driver (manual/nightly).
//!
//! Spawns N `gonzalod` replicas over a shared S3 backend and runs repeated
//! workload rounds, each with a kill+recover cycle on a rotating replica, then
//! reports per-round invariant results. Exits non-zero if any round violated an
//! invariant. Requires a S3 target in the environment (unlike the bounded
//! gate, this driver is run explicitly, so a missing target is an error):
//!
//!   GONZALO_S3_TEST_ENDPOINT, GONZALO_S3_TEST_BUCKET,
//!   AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY   (see scripts/rustfs-up.sh)
//!
//! Usage:
//!   gonzalo-soak [--replicas N] [--rounds N] [--writers N] [--shared-keys N]
//!                [--ops-per-writer N] [--unique-per-writer N] [--retries N]

use gonzalo_soak::harness::run_rounds;
use gonzalo_soak::target::S3Target;
use gonzalo_soak::workload::WorkloadConfig;

struct Args {
    replicas: usize,
    rounds: usize,
    cfg: WorkloadConfig,
}

fn parse_args() -> Result<Args, String> {
    let mut replicas = 3usize;
    let mut rounds = 20usize;
    let mut cfg = WorkloadConfig {
        writers: 16,
        shared_keys: 6,
        ops_per_writer: 50,
        unique_per_writer: 4,
        max_conflict_retries: 100,
        ..Default::default()
    };

    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || {
            it.next()
                .ok_or_else(|| format!("{flag} needs a value"))?
                .parse::<usize>()
                .map_err(|e| format!("{flag}: {e}"))
        };
        match flag.as_str() {
            "--replicas" => replicas = val()?,
            "--rounds" => rounds = val()?,
            "--writers" => cfg.writers = val()?,
            "--shared-keys" => cfg.shared_keys = val()?,
            "--ops-per-writer" => cfg.ops_per_writer = val()?,
            "--unique-per-writer" => cfg.unique_per_writer = val()?,
            "--retries" => cfg.max_conflict_retries = val()?,
            "-h" | "--help" => return Err("help".into()),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(Args {
        replicas,
        rounds,
        cfg,
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("gonzalo-soak: {e}\n{}", usage());
            std::process::exit(if e == "help" { 0 } else { 2 });
        }
    };

    let Some(target) = S3Target::from_process_env() else {
        eprintln!(
            "gonzalo-soak: no S3 target — set GONZALO_S3_TEST_ENDPOINT, \
             GONZALO_S3_TEST_BUCKET, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY \
             (see scripts/rustfs-up.sh)"
        );
        std::process::exit(2);
    };

    eprintln!(
        "gonzalo-soak: {} replicas, {} rounds, {} writers, {} shared keys",
        args.replicas, args.rounds, args.cfg.writers, args.cfg.shared_keys
    );

    let outcomes = match run_rounds(&target, args.cfg, args.replicas, args.rounds).await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gonzalo-soak: run failed: {e}");
            std::process::exit(1);
        }
    };

    let mut failed = 0;
    for (r, o) in outcomes.iter().enumerate() {
        let committed = o
            .stats
            .ops
            .iter()
            .filter(|op| op.result == gonzalo_soak::oracle::OpResult::Committed)
            .count();
        if o.passed() {
            eprintln!(
                "round {r}: PASS  committed={committed} conflicts={} writers={}/{}",
                o.stats.conflicts_observed, o.stats.writers_completed, o.stats.writers_total
            );
        } else {
            failed += 1;
            eprintln!("round {r}: FAIL  violations={:?}", o.violations);
        }
    }

    if failed == 0 {
        eprintln!("gonzalo-soak: all {} rounds passed", outcomes.len());
    } else {
        eprintln!("gonzalo-soak: {failed}/{} rounds FAILED", outcomes.len());
        std::process::exit(1);
    }
}

fn usage() -> &'static str {
    "usage: gonzalo-soak [--replicas N] [--rounds N] [--writers N] \
     [--shared-keys N] [--ops-per-writer N] [--unique-per-writer N] [--retries N]"
}
