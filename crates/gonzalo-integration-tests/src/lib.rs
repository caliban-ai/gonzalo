//! Cross-crate integration tests for the gonzalo workspace.
//!
//! This crate ships no library code — it exists only as a home for integration
//! tests that wire several gonzalo crates together (e.g. the remote
//! [`ServerStore`](gonzalo_store_server::ServerStore) client exercised against a
//! real `gonzalod` over HTTP/gRPC, and code-graph queries over HTTP).
//!
//! Keeping them here — rather than in any single crate's `tests/` — means no
//! *published* library dev-depends on the `gonzalo-server` binary or the heavy
//! `gonzalo-graph` grammar set. That keeps each crate's crates.io publish order
//! tied to its real runtime dependencies (see gonzalo#190 / #187), and keeps
//! per-crate test builds light. The crate is `publish = false`.
