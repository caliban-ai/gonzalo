//! Content hashing and per-record revisions for optimistic concurrency.

use serde::{Deserialize, Serialize};

/// A content hash (blake3, hex-encoded) of a record body.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }
}

/// A record revision: a monotonic counter plus the body's content hash.
/// Two writers diverge when their `counter`/`hash` pair differs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub counter: u64,
    pub hash: ContentHash,
}

impl Revision {
    /// The first revision for a freshly created record body.
    pub fn initial(body: &[u8]) -> Self {
        Self {
            counter: 0,
            hash: ContentHash::of(body),
        }
    }

    /// The next revision after `self` for an updated body.
    pub fn next(&self, body: &[u8]) -> Self {
        Self {
            counter: self.counter + 1,
            hash: ContentHash::of(body),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_distinct() {
        assert_eq!(ContentHash::of(b"abc"), ContentHash::of(b"abc"));
        assert_ne!(ContentHash::of(b"abc"), ContentHash::of(b"abd"));
    }

    #[test]
    fn next_increments_counter_and_rehashes() {
        let r0 = Revision::initial(b"v1");
        let r1 = r0.next(b"v2");
        assert_eq!(r0.counter, 0);
        assert_eq!(r1.counter, 1);
        assert_ne!(r0.hash, r1.hash);
    }
}
