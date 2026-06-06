//! Shared, filesystem/object-key path mapping used by every storage
//! substrate so a record lands at the same logical location regardless of
//! backend.

use crate::RecordKey;

/// Encode one key component as a single safe path/key segment. Only
/// `[A-Za-z0-9_-]` survive; everything else (including `.`, `/`, and dot
/// lookalikes) maps to `_`, so `..` and path separators cannot escape.
pub fn segment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

/// The three sanitized path components for a record:
/// `(namespace_dir, collection_dir, "<id>.json")`. Backends join these with
/// their own separator (`PathBuf` for fs/git, `/` for object keys).
pub fn record_components(key: &RecordKey) -> (String, String, String) {
    (
        segment(&key.namespace),
        segment(&key.collection),
        format!("{}.json", segment(&key.id)),
    )
}

/// The object-key form `namespace/collection/id.json` for object stores.
pub fn object_key(key: &RecordKey) -> String {
    let (ns, col, file) = record_components(key);
    format!("{ns}/{col}/{file}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_neutralizes_traversal() {
        assert_eq!(segment(".."), "__");
        assert_eq!(segment("../etc"), "___etc");
        assert_eq!(segment("a.b"), "a_b");
    }

    #[test]
    fn object_key_is_slash_joined_json() {
        let k = RecordKey::new("caliban", "topics", "rust");
        assert_eq!(object_key(&k), "caliban/topics/rust.json");
    }
}
