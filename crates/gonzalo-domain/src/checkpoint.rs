//! Checkpoint view (opaque snapshot blob + label).

use crate::codec::RecordCodec;
use gonzalo_core::RecordKind;
use serde::{Deserialize, Serialize};

/// A checkpoint: a labeled, opaque snapshot payload (base64 or raw text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub label: String,
    pub payload: String,
}
impl RecordCodec for Checkpoint {}
impl Checkpoint {
    pub const KIND: RecordKind = RecordKind::Checkpoint;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::RecordCodec;

    #[test]
    fn checkpoint_roundtrips() {
        let c = Checkpoint {
            label: "before-refactor".into(),
            payload: "blob".into(),
        };
        assert_eq!(Checkpoint::from_body(&c.to_body().unwrap()).unwrap(), c);
        assert_eq!(Checkpoint::KIND, RecordKind::Checkpoint);
    }
}
