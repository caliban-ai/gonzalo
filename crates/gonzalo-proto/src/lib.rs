//! Shared gRPC schema for the gonzalo daemon. The generated module mirrors
//! the `gonzalo.v1` package; payloads are JSON-encoded `gonzalo-core` types.

pub mod http;

pub mod v1 {
    tonic::include_proto!("gonzalo.v1");
}

pub use v1::{
    DeleteRequest, DeleteResponse, GetRequest, GetResponse, GraphLocatedResponse,
    GraphNamesResponse, GraphQueryRequest, ListRequest, ListResponse, PutRequest, PutResponse,
    gonzalo_client::GonzaloClient,
    gonzalo_server::{Gonzalo, GonzaloServer},
};

/// Default ceiling for a single blob transferred over the daemon, in bytes
/// (64 MiB). Shared by the server (HTTP body limit + gRPC decode limit) and the
/// client (gRPC decode limit) so both agree on the supported blob size. The
/// daemon may raise its own limit via `GONZALO_MAX_BLOB_SIZE`, but a client
/// still caps decoding at this constant (see gonzalo#184 design §4).
pub const DEFAULT_MAX_BLOB_SIZE: usize = 64 * 1024 * 1024;

#[cfg(test)]
mod tests {
    #[test]
    fn default_max_blob_size_is_64_mib() {
        assert_eq!(super::DEFAULT_MAX_BLOB_SIZE, 67_108_864);
    }
}
