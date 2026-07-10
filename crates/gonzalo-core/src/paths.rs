//! Shared, filesystem/object-key path mapping used by every storage
//! substrate so a record lands at the same logical location regardless of
//! backend.

use crate::RecordKey;

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Encode one key component as a single safe path/key segment.
///
/// This is a **reversible** percent-style encoding: the unreserved set
/// `[A-Za-z0-9_-]` survives verbatim, and every other byte is escaped as
/// `%XX` (uppercase hex of the UTF-8 byte). Because `.` and `/` are escaped,
/// `..` and path separators cannot escape a component; because the mapping is
/// injective ([`decode_segment`] is its exact inverse), two distinct keys can
/// never collide onto one path/object key — closing the silent cross-key
/// overwrite and OCC-bypass that the old lossy `_`-collapse allowed.
///
/// Well-formed keys (only `[A-Za-z0-9_-]`) encode to themselves, so existing
/// stores need no migration.
pub fn segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => out.push(b as char),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Decode a segment produced by [`segment`] back to the original component.
/// Exact inverse of [`segment`]; on our own output the round-trip is lossless.
/// A stray `%` not followed by two hex digits is passed through literally so
/// decoding never panics on unexpected input.
pub fn decode_segment(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
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
        // `.` and `/` are escaped, so no component can contain `..` or a
        // separator after encoding.
        assert_eq!(segment(".."), "%2E%2E");
        assert_eq!(segment("../etc"), "%2E%2E%2Fetc");
        assert_eq!(segment("a.b"), "a%2Eb");
        assert!(!segment("../../x").contains(".."));
        assert!(!segment("a/b").contains('/'));
    }

    #[test]
    fn segment_leaves_wellformed_keys_untouched() {
        // No migration for clean keys: they encode to themselves.
        for s in ["rust", "caliban", "topics", "a_b-c9", "UPPER"] {
            assert_eq!(segment(s), s);
        }
    }

    #[test]
    fn segment_roundtrips_and_is_injective() {
        // decode ∘ segment == identity, so segment is injective: distinct keys
        // never share an encoding (the core anti-collision property).
        let cases = [
            "",
            "..",
            "v1.0",
            "v1_0",
            "a/b",
            "a.b",
            "50% off",
            "spaces here",
            "café/x",
            "emoji🚀",
            "%2E", // a literal percent must round-trip too
            "a%2Fb",
        ];
        let mut encoded = std::collections::BTreeSet::new();
        for s in cases {
            let enc = segment(s);
            assert_eq!(decode_segment(&enc), s, "round-trip failed for {s:?}");
            assert!(encoded.insert(enc), "encoding collision at {s:?}");
        }
        // The classic collision pair now maps to distinct segments.
        assert_ne!(segment("v1.0"), segment("v1_0"));
        assert_ne!(segment("a/b"), segment("a_b"));
    }

    #[test]
    fn object_key_is_slash_joined_json() {
        let k = RecordKey::new("caliban", "topics", "rust");
        assert_eq!(object_key(&k), "caliban/topics/rust.json");
    }
}
