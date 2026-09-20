//! The gonzalo daemon library: a transport-agnostic [`Service`] over a
//! `Store`, served over gRPC ([`serve_grpc`]) and/or HTTP/JSON
//! ([`serve_http`]). Operators choose whichever transport they want.

mod auth;
mod config;
mod grpc;
mod http;
mod service;

pub use auth::{Access, Auth, Principal};
pub use config::{StoreConfig, ancestor_cap_from_env};
pub use grpc::{GrpcAdapter, serve_grpc};
pub use http::{SERVED_OPERATIONS, router, serve_http};
pub use service::{Service, ViewSummary};
