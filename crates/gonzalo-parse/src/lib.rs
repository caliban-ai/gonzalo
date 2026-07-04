//! Crash-isolated code parsing (ticket E).
//!
//! tree-sitter grammars are C and can `abort()`/segfault on malformed input,
//! which Rust cannot catch. [`ParserPool`] runs [`build_rust`](gonzalo_graph::build_rust)
//! in a pool of `gonzalo-parse-worker` subprocesses, so such a crash kills only
//! a worker; the pool respawns it and the graph store/query layer (in the parent)
//! is never taken down. This is the isolation required before parsing arbitrary
//! target-repo input at scale.
//!
//! The pool keeps `size` long-lived workers and dispatches parses round-robin,
//! one in flight per worker. A worker that dies (crash) or hangs past the
//! per-parse `timeout` is dropped and lazily respawned on next use; a death is
//! retried once on a fresh worker so an unlucky crash doesn't fail a good parse.

use gonzalo_graph::{CodeGraph, Language};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// One parse request sent to a worker: the source and the language to parse it
/// as. Serialized as one JSON line (JSON escapes newlines, so a whole file is a
/// single line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseRequest {
    pub language: Language,
    pub source: String,
}

/// Why a parse did not return a graph. All variants are recoverable — the pool
/// has already dropped the offending worker.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("failed to spawn parse worker: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("parse worker died (crashed or closed its pipe)")]
    WorkerDied,
    #[error("parse worker exceeded the {0:?} timeout")]
    Timeout(Duration),
    #[error("parse worker sent a malformed response: {0}")]
    Protocol(String),
}

/// A pool of parse-worker subprocesses.
pub struct ParserPool {
    worker_bin: PathBuf,
    worker_env: Vec<(String, String)>,
    slots: Vec<Mutex<Option<Worker>>>,
    next: AtomicUsize,
    timeout: Duration,
}

impl ParserPool {
    /// Create a pool of `size` workers running `worker_bin` (the
    /// `gonzalo-parse-worker` binary), each parse bounded by `timeout`. Workers
    /// spawn lazily on first use.
    pub fn new(worker_bin: impl Into<PathBuf>, size: usize, timeout: Duration) -> Self {
        let size = size.max(1);
        let slots = (0..size).map(|_| Mutex::new(None)).collect();
        Self {
            worker_bin: worker_bin.into(),
            worker_env: Vec::new(),
            slots,
            next: AtomicUsize::new(0),
            timeout,
        }
    }

    /// Set extra environment variables handed to every spawned worker (on top of
    /// the inherited environment).
    pub fn with_worker_env(mut self, vars: Vec<(String, String)>) -> Self {
        self.worker_env = vars;
        self
    }

    /// The number of worker slots.
    pub fn size(&self) -> usize {
        self.slots.len()
    }

    /// Parse `source` as `language` into a [`CodeGraph`] on an isolated worker.
    /// A worker crash is retried once on a fresh worker; a hang past the timeout
    /// is not retried (it may be pathological input that always hangs).
    pub async fn parse(&self, language: Language, source: &str) -> Result<CodeGraph, ParseError> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        let mut slot = self.slots[idx].lock().await;

        let mut last_err = ParseError::WorkerDied;
        for _ in 0..2 {
            if slot.is_none() {
                *slot = Some(
                    Worker::spawn(&self.worker_bin, &self.worker_env).map_err(ParseError::Spawn)?,
                );
            }
            let worker = slot.as_mut().expect("worker present");
            match tokio::time::timeout(self.timeout, worker.roundtrip(language, source)).await {
                Ok(Ok(graph)) => return Ok(graph),
                Ok(Err(e)) => {
                    // Dead worker: drop it (kill_on_drop) and retry on a fresh one.
                    *slot = None;
                    last_err = e;
                }
                Err(_) => {
                    // Hung worker: drop it and give up (don't retry a hang).
                    *slot = None;
                    return Err(ParseError::Timeout(self.timeout));
                }
            }
        }
        Err(last_err)
    }
}

/// One live worker subprocess and its pipes.
struct Worker {
    // Held so `kill_on_drop` tears the child down when the worker is dropped.
    _child: tokio::process::Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    fn spawn(bin: &Path, env: &[(String, String)]) -> std::io::Result<Self> {
        let mut child = Command::new(bin)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        Ok(Self {
            _child: child,
            stdin,
            stdout,
        })
    }

    /// Send one request and read one response. A broken pipe or EOF means the
    /// worker died.
    async fn roundtrip(
        &mut self,
        language: Language,
        source: &str,
    ) -> Result<CodeGraph, ParseError> {
        let request = ParseRequest {
            language,
            source: source.to_string(),
        };
        let mut req = serde_json::to_string(&request).expect("ParseRequest serializes");
        req.push('\n');
        self.stdin
            .write_all(req.as_bytes())
            .await
            .map_err(|_| ParseError::WorkerDied)?;
        self.stdin
            .flush()
            .await
            .map_err(|_| ParseError::WorkerDied)?;

        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .await
            .map_err(|_| ParseError::WorkerDied)?;
        if n == 0 {
            return Err(ParseError::WorkerDied); // EOF: the worker exited
        }
        serde_json::from_str(&line).map_err(|e| ParseError::Protocol(e.to_string()))
    }
}
