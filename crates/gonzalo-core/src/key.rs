//! Stable addressing for records.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The stable address of a record: `namespace/collection/id`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordKey {
    pub namespace: String,
    pub collection: String,
    pub id: String,
}

impl RecordKey {
    pub fn new(
        namespace: impl Into<String>,
        collection: impl Into<String>,
        id: impl Into<String>,
    ) -> Self {
        Self { namespace: namespace.into(), collection: collection.into(), id: id.into() }
    }
}

impl fmt::Display for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.namespace, self.collection, self.id)
    }
}

/// A prefix used to list records. `None` fields match anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyPrefix {
    pub namespace: Option<String>,
    pub collection: Option<String>,
}

impl KeyPrefix {
    pub fn matches(&self, key: &RecordKey) -> bool {
        self.namespace.as_ref().is_none_or(|n| n == &key.namespace)
            && self.collection.as_ref().is_none_or(|c| c == &key.collection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_slash_joined() {
        let k = RecordKey::new("caliban", "topics", "rust-tips");
        assert_eq!(k.to_string(), "caliban/topics/rust-tips");
    }

    #[test]
    fn prefix_matches_on_set_fields_only() {
        let k = RecordKey::new("caliban", "topics", "x");
        assert!(KeyPrefix { namespace: Some("caliban".into()), collection: None }.matches(&k));
        assert!(!KeyPrefix { namespace: Some("other".into()), collection: None }.matches(&k));
        assert!(KeyPrefix::default().matches(&k));
    }
}
