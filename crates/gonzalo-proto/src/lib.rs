//! Shared gRPC schema for the gonzalo daemon. The generated module mirrors
//! the `gonzalo.v1` package; payloads are JSON-encoded `gonzalo-core` types.

pub mod http;

pub mod v1 {
    tonic::include_proto!("gonzalo.v1");
}

pub use v1::{
    GetRequest, GetResponse, ListRequest, ListResponse, PutRequest, PutResponse,
    gonzalo_client::GonzaloClient,
    gonzalo_server::{Gonzalo, GonzaloServer},
};
