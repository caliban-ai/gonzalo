//! Ties the pieces together: spawn `gonzalod` replicas over the S3 backend, build a
//! [`Dispatcher`] over them, run the [`workload`] while injecting replica-kill
//! chaos, and check the [`oracle`]. Used by the bounded gate (`tests/ha_soak.rs`)
//! and, with a longer chaos loop, by the `gonzalo-soak` binary.
//!
//! [`workload`]: crate::workload
//! [`oracle`]: crate::oracle

use crate::dispatch::Dispatcher;
use crate::oracle::{self, SoakStats, Violation};
use crate::replica::ReplicaSet;
use crate::target::S3Target;
use crate::workload::{self, WorkloadConfig};
use gonzalo_core::Store;
use gonzalo_store_server::ServerStore;
use std::sync::Arc;
use std::time::Duration;

/// The result of a soak run: the collected stats and any invariant violations.
pub struct SoakOutcome {
    pub stats: SoakStats,
    pub violations: Vec<Violation>,
}

impl SoakOutcome {
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Build a dispatcher (the "Service") over the replicas' HTTP endpoints.
pub fn dispatcher_over(base_urls: &[String]) -> Result<Arc<Dispatcher>, String> {
    let mut replicas: Vec<Arc<dyn Store>> = Vec::with_capacity(base_urls.len());
    for url in base_urls {
        let store = ServerStore::http(url).map_err(|e| format!("connect {url}: {e}"))?;
        replicas.push(Arc::new(store));
    }
    Ok(Arc::new(Dispatcher::new(replicas)))
}

/// Run `rounds` soak rounds against `replicas` freshly-spawned `gonzalod`
/// processes over `target`. Each round runs the workload while performing one
/// kill+recover cycle on a (rotating) replica, then checks the oracle. Each round
/// uses a distinct collection so op-ids never collide across rounds.
///
/// `rounds == 1` is exactly the bounded per-PR gate; the deep soak passes N > 1.
pub async fn run_rounds(
    target: &S3Target,
    base_cfg: WorkloadConfig,
    replicas: usize,
    rounds: usize,
) -> Result<Vec<SoakOutcome>, String> {
    let mut set = ReplicaSet::spawn(replicas, target).await?;
    let dispatcher = dispatcher_over(&set.base_urls())?;
    let mut outcomes = Vec::with_capacity(rounds);

    for r in 0..rounds {
        let mut cfg = base_cfg.clone();
        cfg.collection = format!("{}-r{r}", base_cfg.collection);

        let workload_task = {
            let d = dispatcher.clone();
            tokio::spawn(async move { workload::run(d, cfg).await })
        };

        if replicas > 1 {
            // Warm up, kill a rotating replica mid-load (a pod death), hold,
            // then recover it — the failover path a k8s Service masks.
            let victim = 1 + (r % (replicas - 1));
            tokio::time::sleep(Duration::from_millis(200)).await;
            set.kill(victim);
            tokio::time::sleep(Duration::from_millis(450)).await;
            set.respawn(victim).await?;
        }

        let stats = workload_task
            .await
            .map_err(|e| format!("workload task panicked: {e}"))?;
        let violations = oracle::check(&stats);
        outcomes.push(SoakOutcome { stats, violations });
    }

    Ok(outcomes)
}
