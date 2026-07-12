//! A set of real `gonzalod` subprocesses, all backed by the same S3
//! store — the "N stateless replicas behind a Service" of the k8s HA model.
//!
//! Each replica is a `gonzalod` process configured entirely from the environment
//! (`GONZALO_STORE=s3` + the shared bucket/endpoint/creds + distinct bind ports).
//! [`ReplicaSet::kill`] SIGKILLs one (a pod death); [`ReplicaSet::respawn`]
//! restarts it. The `gonzalod` binary is found via `GONZALO_SOAK_GONZALOD_BIN`
//! or, failing that, the workspace target dir next to the test/soak executable.

use crate::target::S3Target;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How a single replica is (re)spawned: its bind ports and full env.
struct ReplicaSpec {
    http_port: u16,
    grpc_port: u16,
    env: Vec<(String, String)>,
}

struct Replica {
    spec: ReplicaSpec,
    child: Option<Child>,
}

/// A live set of `gonzalod` replicas over a shared S3 backend.
pub struct ReplicaSet {
    gonzalod: PathBuf,
    replicas: Vec<Replica>,
}

impl ReplicaSet {
    /// Spawn `n` replicas over `target` and wait until each answers `/readyz`.
    pub async fn spawn(n: usize, target: &S3Target) -> Result<Self, String> {
        assert!(n >= 1, "need at least one replica");
        let gonzalod = locate_gonzalod()?;
        let mut set = ReplicaSet {
            gonzalod,
            replicas: Vec::with_capacity(n),
        };
        for i in 0..n {
            // Distinct localhost ports per replica; the shared bucket makes them
            // interchangeable stateless fronts over one durable store.
            let http_port = 18080 + i as u16;
            let grpc_port = 18150 + i as u16;
            let env = replica_env(target, http_port, grpc_port);
            let spec = ReplicaSpec {
                http_port,
                grpc_port,
                env,
            };
            let child = set.launch(&spec)?;
            set.replicas.push(Replica {
                spec,
                child: Some(child),
            });
        }
        for i in 0..n {
            set.wait_ready(i).await?;
        }
        Ok(set)
    }

    /// The HTTP base URLs of every replica (whether currently live or not).
    pub fn base_urls(&self) -> Vec<String> {
        self.replicas
            .iter()
            .map(|r| format!("http://127.0.0.1:{}", r.spec.http_port))
            .collect()
    }

    /// Number of replicas.
    pub fn len(&self) -> usize {
        self.replicas.len()
    }

    /// True when there are no replicas.
    pub fn is_empty(&self) -> bool {
        self.replicas.is_empty()
    }

    /// SIGKILL replica `idx` (simulates a pod death). Idempotent.
    pub fn kill(&mut self, idx: usize) {
        if let Some(mut child) = self.replicas[idx].child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Restart replica `idx` and wait for it to be ready again.
    pub async fn respawn(&mut self, idx: usize) -> Result<(), String> {
        if self.replicas[idx].child.is_none() {
            let spec = ReplicaSpec {
                http_port: self.replicas[idx].spec.http_port,
                grpc_port: self.replicas[idx].spec.grpc_port,
                env: self.replicas[idx].spec.env.clone(),
            };
            let child = self.launch(&spec)?;
            self.replicas[idx].child = Some(child);
        }
        self.wait_ready(idx).await
    }

    fn launch(&self, spec: &ReplicaSpec) -> Result<Child, String> {
        Command::new(&self.gonzalod)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            // Keep a minimal PATH so the AWS SDK / TLS can find system bits.
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("spawn gonzalod ({}): {e}", self.gonzalod.display()))
    }

    /// Poll `/readyz` on replica `idx` until 200 OK or a deadline.
    async fn wait_ready(&self, idx: usize) -> Result<(), String> {
        let url = format!(
            "http://127.0.0.1:{}/readyz",
            self.replicas[idx].spec.http_port
        );
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(resp) = client
                .get(&url)
                .timeout(Duration::from_secs(2))
                .send()
                .await
                && resp.status().is_success()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("replica {idx} never became ready at {url}"));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
}

impl Drop for ReplicaSet {
    fn drop(&mut self) {
        for r in &mut self.replicas {
            if let Some(mut child) = r.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// The env a single S3-backed `gonzalod` replica runs with.
fn replica_env(target: &S3Target, http_port: u16, grpc_port: u16) -> Vec<(String, String)> {
    let mut env = vec![
        ("GONZALO_STORE".into(), "s3".into()),
        ("GONZALO_S3_BUCKET".into(), target.bucket.clone()),
        ("GONZALO_S3_ENDPOINT".into(), target.endpoint.clone()),
        ("AWS_ACCESS_KEY_ID".into(), target.access_key.clone()),
        ("AWS_SECRET_ACCESS_KEY".into(), target.secret_key.clone()),
        ("GONZALO_HTTP_ADDR".into(), format!("127.0.0.1:{http_port}")),
        ("GONZALO_GRPC_ADDR".into(), format!("127.0.0.1:{grpc_port}")),
    ];
    env.push((
        "GONZALO_S3_REGION".into(),
        target.region.clone().unwrap_or_else(|| "us-east-1".into()),
    ));
    env
}

/// Find the `gonzalod` binary: an explicit override, or the workspace target dir
/// adjacent to the currently-running test/soak executable.
fn locate_gonzalod() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("GONZALO_SOAK_GONZALOD_BIN") {
        let p = PathBuf::from(p);
        return if p.exists() {
            Ok(p)
        } else {
            Err(format!(
                "GONZALO_SOAK_GONZALOD_BIN set but not found: {}",
                p.display()
            ))
        };
    }
    // current_exe is `.../target/<profile>/deps/<name>` or `.../target/<profile>/<name>`.
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut dir = exe.parent().ok_or("exe has no parent")?.to_path_buf();
    if dir.file_name().is_some_and(|n| n == "deps") {
        dir = dir.parent().ok_or("deps has no parent")?.to_path_buf();
    }
    let bin = dir.join("gonzalod");
    if bin.exists() {
        Ok(bin)
    } else {
        Err(format!(
            "gonzalod binary not found at {} — build it first: `cargo build --bin gonzalod` \
             (or set GONZALO_SOAK_GONZALOD_BIN)",
            bin.display()
        ))
    }
}
