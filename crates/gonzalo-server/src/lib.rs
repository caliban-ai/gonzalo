//! The gonzalo daemon library: a transport-agnostic [`Service`] over a
//! `Store`, served over gRPC ([`serve_grpc`]) and/or HTTP/JSON
//! ([`serve_http`]). Operators choose whichever transport they want.

mod grpc;
mod http;
mod service;

pub use grpc::{GrpcAdapter, serve_grpc};
pub use http::{router, serve_http};
pub use service::Service;
