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
        serde_json::from_slice(body.bytes()).map_err(|e| CoreError::Serde(e.to_string()))
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
        let d = Demo { n: 7, s: "x".into() };
        let body = d.to_body().unwrap();
        assert_eq!(Demo::from_body(&body).unwrap(), d);
    }
}
