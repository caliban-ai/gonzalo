//! Memory-tier and auto-memory topic views.

use crate::codec::RecordCodec;
use gonzalo_core::RecordKind;
use serde::{Deserialize, Serialize};

/// A CLAUDE.md-style memory tier file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryTier {
    pub name: String,
    pub content: String,
}
impl RecordCodec for MemoryTier {}
impl MemoryTier {
    pub const KIND: RecordKind = RecordKind::MemoryTier;
}

/// An auto-memory topic: a slug plus append-only bullet lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topic {
    pub slug: String,
    pub bullets: Vec<String>,
}
impl RecordCodec for Topic {}
impl Topic {
    pub const KIND: RecordKind = RecordKind::Topic;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::RecordCodec;

    #[test]
    fn tier_roundtrips() {
        let t = MemoryTier { name: "global".into(), content: "be concise".into() };
        assert_eq!(MemoryTier::from_body(&t.to_body().unwrap()).unwrap(), t);
        assert_eq!(MemoryTier::KIND, RecordKind::MemoryTier);
    }

    #[test]
    fn topic_roundtrips() {
        let t = Topic { slug: "rust".into(), bullets: vec!["use clippy".into()] };
        assert_eq!(Topic::from_body(&t.to_body().unwrap()).unwrap(), t);
        assert_eq!(Topic::KIND, RecordKind::Topic);
    }
}
