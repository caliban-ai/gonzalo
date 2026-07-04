//! `gonzalo-mcp` — the code-graph MCP server (stdio). An agent spawns this
//! binary and calls tools that answer from a local gonzalo store.
//!
//! - `GONZALO_ROOT` — store root directory (default `./gonzalo-data`)
//!
//! rmcp owns stdout (JSON-RPC framing); diagnostics go to stderr only.

use gonzalo_mcp::GonzaloMcp;
use gonzalo_server::Service;
use gonzalo_store_fs::FsStore;
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let root = std::env::var("GONZALO_ROOT").unwrap_or_else(|_| "./gonzalo-data".into());

    // One FsStore backs both the record store and the blob store.
    // Per-view SQLite graphs written by `gonzalo index` live under `<root>/graphs`.
    let graph_root = std::path::Path::new(&root).join("graphs");
    let fs = Arc::new(FsStore::new(&root));
    let server = GonzaloMcp::new(
        Service::new(fs.clone(), fs).with_graph_root(graph_root),
        root,
    );

    eprintln!("gonzalo-mcp: serving on stdio");
    let (stdin, stdout) = stdio();
    let running = server
        .serve((stdin, stdout))
        .await
        .map_err(|e| std::io::Error::other(format!("gonzalo-mcp: {e}")))?;
    let _quit = running.waiting().await;
    Ok(())
}
