//! Mapping between typed domain structs and generic record bodies.

use gonzalo_core::{Body, CoreError, Result};
use serde::{Serialize, de::DeserializeOwned};

/// A typed value that can be stored in a record body as JSON.
pub trait RecordCodec: Serialize + DeserializeOwned {
    fn to_body(&self) -> Result<Body> {
        let bytes = serde_json::to_vec(self).map_err(|e| CoreError::Serde(e.to_string()))?;
        Ok(Body::Inline(bytes))
    }

    fn from_body(body: &Body) -> Result<Self> {
        match body {
            Body::Inline(bytes) => {
                serde_json::from_slice(bytes).map_err(|e| CoreError::Serde(e.to_string()))
            }
            // A `Body::Blob` carries only the content hash; the referenced JSON
            // lives out-of-line and must be fetched via `BlobStore::get_blob`.
            // `from_body` has no `BlobStore`, so decoding a blob here is
            // impossible — fail explicitly rather than misparse the hash bytes
            // as JSON (which yields a misleading serde error).
            Body::Blob { .. } => Err(CoreError::Backend(
                "cannot decode a blob-backed body without a BlobStore".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Demo {
        n: u32,
        s: String,
    }
    impl RecordCodec for Demo {}

    #[test]
    fn roundtrips_through_body() {
        let d = Demo {
            n: 7,
            s: "x".into(),
        };
        let body = d.to_body().unwrap();
        assert_eq!(Demo::from_body(&body).unwrap(), d);
    }

    #[test]
    fn from_body_rejects_a_blob_body_instead_of_misparsing_its_hash() {
        // A `Body::Blob` carries only the content hash, not the referenced JSON.
        // `from_body` has no `BlobStore`, so it must fail explicitly rather than
        // try to parse the hash string as JSON (a misleading serde error).
        let body = Body::blob(br#"{"n":7,"s":"x"}"#);
        let err = Demo::from_body(&body).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("blob"),
            "error should mention a blob body, got: {msg}"
        );
    }
}
