//! Parse worker: a subprocess that reads source and writes the parsed
//! [`CodeGraph`], one request per line. It exists to contain tree-sitter — the
//! grammars are C and can `abort()` on malformed input, which is uncatchable in
//! Rust; running parsing here means such a crash kills only this worker, not the
//! daemon (see [`gonzalo_parse::ParserPool`]).
//!
//! ## Protocol (newline-delimited JSON, one exchange per line)
//! - **Request**: a JSON string — the file's source. JSON escapes newlines, so a
//!   whole file is exactly one line.
//! - **Response**: a JSON [`CodeGraph`] on one line.
//!
//! ## Fault injection (tests only)
//! Two env-gated sentinels give deterministic stand-ins for grammar
//! pathologies, used to test pool respawn/timeout. Unset in production, so
//! neither is a live code path:
//! - `GONZALO_PARSE_CRASH_TOKEN` — a request equal to it `abort()`s the worker
//!   (a grammar crash).
//! - `GONZALO_PARSE_HANG_TOKEN` — a request equal to it blocks forever (a
//!   grammar hang).

use gonzalo_graph::build_rust;
use std::io::{BufRead, Write};

fn main() {
    let crash_token = std::env::var("GONZALO_PARSE_CRASH_TOKEN").ok();
    let hang_token = std::env::var("GONZALO_PARSE_HANG_TOKEN").ok();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        let Ok(source) = serde_json::from_str::<String>(&line) else {
            // A malformed request frame is a protocol error; skip it rather than
            // die (the pool would otherwise see a spurious worker death).
            continue;
        };

        if crash_token.as_deref() == Some(source.as_str()) {
            std::process::abort();
        }
        if hang_token.as_deref() == Some(source.as_str()) {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }

        let graph = build_rust(&source);
        let encoded = serde_json::to_string(&graph).expect("CodeGraph serializes");
        if writeln!(stdout, "{encoded}").is_err() || stdout.flush().is_err() {
            break; // parent closed the pipe
        }
    }
}
