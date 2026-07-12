//! `ParserPool` behavior: parses on isolated workers (multi-language), reuses
//! long-lived workers, and recovers from a worker crash or hang (ticket E).

use gonzalo_graph::Language;
use gonzalo_parse::{ParseError, ParserPool};
use std::path::PathBuf;
use std::time::Duration;

fn worker_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gonzalo-parse-worker"))
}

fn pool(size: usize) -> ParserPool {
    ParserPool::new(worker_bin(), size, Duration::from_secs(10))
}

#[tokio::test]
async fn parses_rust_into_a_graph() {
    let p = pool(2);
    let graph = p
        .parse(Language::Rust, "fn helper() {}\nfn main() { helper(); }")
        .await
        .unwrap();
    assert!(graph.symbols.iter().any(|s| s.name == "helper"));
    assert!(graph.references.iter().any(|r| r.name == "helper"));
}

#[tokio::test]
async fn parses_python_on_the_same_worker() {
    // The worker dispatches by the request's language, so one pool parses any
    // supported language.
    let p = pool(1);
    let graph = p
        .parse(Language::Python, "def py_fn():\n    pass\n")
        .await
        .unwrap();
    assert!(graph.symbols.iter().any(|s| s.name == "py_fn"));
}

#[tokio::test]
async fn reuses_a_worker_across_many_parses() {
    // One slot, several parses: the worker is long-lived, not respawned.
    let p = pool(1);
    for i in 0..5 {
        let src = format!("fn f{i}() {{}}");
        let g = p.parse(Language::Rust, &src).await.unwrap();
        assert!(g.symbols.iter().any(|s| s.name == format!("f{i}")));
    }
}

#[tokio::test]
async fn handles_concurrent_parses() {
    let p = std::sync::Arc::new(pool(4));
    let mut handles = Vec::new();
    for i in 0..12 {
        let p = p.clone();
        handles.push(tokio::spawn(async move {
            p.parse(Language::Rust, &format!("fn c{i}() {{}}"))
                .await
                .unwrap()
        }));
    }
    for (i, h) in handles.into_iter().enumerate() {
        let g = h.await.unwrap();
        assert!(g.symbols.iter().any(|s| s.name == format!("c{i}")));
    }
}

#[tokio::test]
async fn respawns_and_recovers_after_a_worker_crash() {
    // The worker aborts when the request's source equals the crash token.
    let p = ParserPool::new(worker_bin(), 1, Duration::from_secs(10)).with_worker_env(vec![(
        "GONZALO_PARSE_CRASH_TOKEN".into(),
        "__BOOM__".into(),
    )]);

    // Both attempts crash the worker -> WorkerDied (proves the crash is contained
    // rather than propagating to the parent).
    let err = p.parse(Language::Rust, "__BOOM__").await.unwrap_err();
    assert!(matches!(err, ParseError::WorkerDied), "got {err:?}");

    // The pool respawned a worker; a normal parse now succeeds on it.
    let g = p.parse(Language::Rust, "fn survives() {}").await.unwrap();
    assert!(g.symbols.iter().any(|s| s.name == "survives"));
}

#[tokio::test]
async fn times_out_a_hung_worker() {
    // The timeout is a liveness deadline (catch a worker that never responds),
    // NOT a latency SLA. A hung worker never returns, so a generous budget still
    // detects it; a tight budget (was 300ms) instead false-times-out the healthy
    // recovery parse below when the machine is under heavy build load, which is
    // the flake this guards against. 5s is comfortably above a loaded
    // spawn+parse yet still bounded.
    let p = ParserPool::new(worker_bin(), 1, Duration::from_secs(5))
        .with_worker_env(vec![("GONZALO_PARSE_HANG_TOKEN".into(), "__HANG__".into())]);
    let err = p.parse(Language::Rust, "__HANG__").await.unwrap_err();
    assert!(matches!(err, ParseError::Timeout(_)), "got {err:?}");

    // After the hung worker is dropped, the pool recovers.
    let g = p.parse(Language::Rust, "fn after_hang() {}").await.unwrap();
    assert!(g.symbols.iter().any(|s| s.name == "after_hang"));
}
