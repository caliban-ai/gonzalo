//! HA soak harness for gonzalo (#52): drive N stateless `gonzalod` replicas
//! over a shared S3 backend (RustFS) under replica-kill chaos, and assert
//! gonzalo's core invariant — concurrent edits are never silently lost.
//!
//! The crate is split into pure-logic pieces (unit-tested without any backend)
//! and integration pieces that need a live S3 backend + `gonzalod` binary:
//!
//! - [`oracle`] — the safety/liveness invariant checker (pure).
//! - [`dispatch`] — round-robin + failover across live replicas (pure, mockable).
//! - [`target`] — the S3 endpoint from the env, or skip (pure).
//!
//! Integration pieces (`replica`, `workload`) are added on top and exercised by
//! `tests/ha_soak.rs` (the bounded per-PR gate) and the `gonzalo-soak` binary
//! (the deep soak), both of which skip/error unless a S3 target is set.

pub mod dispatch;
pub mod harness;
pub mod oracle;
pub mod replica;
pub mod target;
pub mod workload;
