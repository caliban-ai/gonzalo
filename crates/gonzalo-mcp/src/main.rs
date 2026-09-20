//! `gonzalo-mcp` — the gonzalo MCP server (stdio). An agent spawns this binary
//! and calls tools that answer from a gonzalo store: the code graph, and the
//! records themselves.
//!
//! - `GONZALO_ROOT` — store root directory (default `./gonzalo-data`)
//! - `GONZALO_DAEMON` — a running `gonzalod`'s base URL. When set, records are
//!   read from and written to that daemon instead of a local directory, so the
//!   server can share a store with other clients (ADR 0007).
//! - `GONZALO_TOKEN` — bearer token sent to that daemon, when it requires one
//! - `GONZALO_MCP_ALLOW_WRITES` — set to `1` to expose `record_put` and
//!   `record_delete`. Off by default.
//!
//! `--daemon <url>` and `--allow-writes` do the same as the last two, for a
//! client that passes arguments rather than environment.
//!
//! rmcp owns stdout (JSON-RPC framing); diagnostics go to stderr only.

use gonzalo_core::{BlobStore, Store};
use gonzalo_mcp::GonzaloMcp;
use gonzalo_server::Service;
use gonzalo_store_fs::{FsStore, expand_tilde};
use gonzalo_store_server::ServerStore;
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Writes are opt-in: reading a store is one thing, mutating it is another,
    // and that is the operator's call rather than the agent's (#197).
    let allow_writes = flag(&args, "--allow-writes")
        || std::env::var("GONZALO_MCP_ALLOW_WRITES").as_deref() == Ok("1");

    let daemon = value(&args, "--daemon").or_else(|| std::env::var("GONZALO_DAEMON").ok());

    let (service, root) = match daemon {
        Some(url) => {
            // One ServerStore backs both records and blobs, so the graph tools
            // assemble their slices through the daemon too.
            let store = match std::env::var("GONZALO_TOKEN") {
                Ok(token) if !token.is_empty() => ServerStore::http_with_token(&url, token),
                _ => ServerStore::http(&url),
            }
            .map_err(|e| std::io::Error::other(format!("gonzalo-mcp: {url}: {e}")))?;
            let store = Arc::new(store);
            let records: Arc<dyn Store> = store.clone();
            let blobs: Arc<dyn BlobStore> = store;
            // No local graph root: views are assembled from slices fetched over
            // the daemon rather than from a SQLite db on this machine.
            (Service::new(records, blobs), url)
        }
        None => {
            // Expand a leading `~`: an MCP client hands this value to the
            // process directly, with no shell to do it, so `GONZALO_ROOT=~/.gonzalo`
            // would otherwise create a directory literally named `~` and answer
            // every query from the wrong store (#211).
            let root = expand_tilde(
                std::env::var("GONZALO_ROOT").unwrap_or_else(|_| "./gonzalo-data".into()),
            );
            // Per-view SQLite graphs written by `gonzalo index` live under
            // `<root>/graphs`.
            let graph_root = root.join("graphs");
            let fs = Arc::new(FsStore::new(&root));
            (
                Service::new(fs.clone(), fs).with_graph_root(graph_root),
                // `status` reports the expanded path, so what the server
                // actually uses is verifiable from the client rather than
                // echoed back as written.
                root.display().to_string(),
            )
        }
    };

    let server = GonzaloMcp::new(service, root).with_writes(allow_writes);

    eprintln!(
        "gonzalo-mcp: serving on stdio ({} records)",
        if allow_writes {
            "read-write"
        } else {
            "read-only"
        }
    );
    let (stdin, stdout) = stdio();
    let running = server
        .serve((stdin, stdout))
        .await
        .map_err(|e| std::io::Error::other(format!("gonzalo-mcp: {e}")))?;
    let _quit = running.waiting().await;
    Ok(())
}

/// Whether a bare flag was passed.
fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// The value of `--name <value>`, or of `--name=<value>`.
fn value(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == name {
            return it.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(name)
            && let Some(v) = rest.strip_prefix('=')
        {
            return Some(v.to_string());
        }
    }
    None
}
