//! Shard assignment and the on-blob format for a durable vector index.
//!
//! Vectors are bucketed by a hash of their key so a write touches one shard
//! rather than the whole index, and a shard's bytes are deterministic so an
//! unchanged shard content-addresses to the blob already stored.

use gonzalo_core::{ContentHash, CoreError, RecordKey, Result};
use std::num::NonZeroU16;

/// Default number of shards for a new index. At ~100k chunks of 384 f32s this
/// is ~600 KB per shard: 256 reads to open, one ~600 KB rewrite per upsert.
pub const DEFAULT_SHARDS: NonZeroU16 = match NonZeroU16::new(256) {
    Some(n) => n,
    None => panic!("256 is nonzero"),
};

const MAGIC: &[u8; 4] = b"GZVS";
const VERSION: u8 = 1;

/// Which shard `key` belongs to.
///
/// Uses blake3 via [`ContentHash`] rather than [`std::hash::DefaultHasher`],
/// whose output is explicitly not stable across releases — a drift there would
/// silently strand every vector in every existing index.
pub fn shard_of(key: &RecordKey, shards: NonZeroU16) -> u16 {
    let s = format!("{}/{}/{}", key.namespace, key.collection, key.id);
    let hex = ContentHash::of(s.as_bytes()).0;
    let bits = u16::from_str_radix(&hex[..4], 16).expect("blake3 hex is 64 hex digits");
    bits % shards.get()
}

/// Encode one shard. Entries are sorted by key, so identical contents always
/// produce identical bytes.
///
/// `pub(crate)`, not `pub`: this freezes the `GZVS` v1 on-disk format as a
/// public contract, and nothing outside this crate has a reason to encode a
/// shard directly — a caller works through [`crate::VectorIndex`].
pub(crate) fn encode_shard(dim: usize, entries: &[(RecordKey, Vec<f32>)]) -> Vec<u8> {
    let mut sorted: Vec<&(RecordKey, Vec<f32>)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&(dim as u32).to_le_bytes());
    out.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    for (key, vector) in sorted {
        put_str16(&mut out, &key.namespace);
        put_str16(&mut out, &key.collection);
        out.extend_from_slice(&(key.id.len() as u32).to_le_bytes());
        out.extend_from_slice(key.id.as_bytes());
        for f in vector {
            out.extend_from_slice(&f.to_le_bytes());
        }
    }
    out
}

/// Decode one shard, returning its dimension and entries.
///
/// Rejects corrupted or truncated blobs as `CoreError::Backend`, including those
/// with unbounded length fields that could cause allocation failures.
///
/// `pub(crate)` for the same reason as [`encode_shard`]: the format is not a
/// public contract, and every caller of this crate goes through
/// [`crate::VectorIndex`] instead.
#[allow(clippy::type_complexity)]
pub(crate) fn decode_shard(bytes: &[u8]) -> Result<(usize, Vec<(RecordKey, Vec<f32>)>)> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(4)? != MAGIC {
        return Err(CoreError::Backend("vector shard: bad magic".into()));
    }
    let version = r.take(1)?[0];
    if version != VERSION {
        return Err(CoreError::Backend(format!(
            "vector shard: unsupported version {version}, expected {VERSION}"
        )));
    }
    let dim = r.u32()? as usize;
    let count = r.u32()? as usize;

    // Cap the allocation by what the remaining input could actually contain.
    // Each entry needs at least 8 bytes (two u16-length fields for namespace and
    // collection) + 4 bytes (id length) + dim * 4 bytes (floats).
    let remaining = bytes.len().saturating_sub(r.at);
    let min_entry_size = 8_usize.saturating_add(dim.saturating_mul(4));
    let max_count = remaining.checked_div(min_entry_size).unwrap_or(usize::MAX);
    if count > max_count {
        return Err(CoreError::Backend(
            "vector shard: entry count exceeds remaining data".into(),
        ));
    }

    let mut entries = Vec::new();
    for _ in 0..count {
        let namespace = r.str16()?;
        let collection = r.str16()?;
        let id_len = r.u32()? as usize;
        let id = String::from_utf8(r.take(id_len)?.to_vec())
            .map_err(|e| CoreError::Backend(format!("vector shard: bad utf8 in id: {e}")))?;
        let mut vector = Vec::new();
        for _ in 0..dim {
            let b: [u8; 4] = r.take(4)?.try_into().expect("took exactly 4 bytes");
            vector.push(f32::from_le_bytes(b));
        }
        entries.push((RecordKey::new(namespace, collection, id), vector));
    }
    Ok((dim, entries))
}

fn put_str16(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(overflow)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| CoreError::Backend("vector shard: truncated".into()))?;
        self.at = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32> {
        let b: [u8; 4] = self.take(4)?.try_into().expect("took exactly 4 bytes");
        Ok(u32::from_le_bytes(b))
    }

    fn str16(&mut self) -> Result<String> {
        let b: [u8; 2] = self.take(2)?.try_into().expect("took exactly 2 bytes");
        let len = u16::from_le_bytes(b) as usize;
        String::from_utf8(self.take(len)?.to_vec())
            .map_err(|e| CoreError::Backend(format!("vector shard: bad utf8: {e}")))
    }
}

fn overflow() -> CoreError {
    CoreError::Backend("vector shard: length overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> RecordKey {
        RecordKey::new("ns", "coll", id)
    }

    #[test]
    fn shard_assignment_is_stable_across_calls() {
        let k = key("a");
        let shards = NonZeroU16::new(256).unwrap();
        assert_eq!(shard_of(&k, shards), shard_of(&k, shards));
    }

    // The assignment must not drift between releases, or every existing index
    // silently loses track of where its vectors live. Pinning one literal makes
    // an accidental change to the hashing fail loudly here.
    #[test]
    fn shard_assignment_is_pinned_to_a_known_value() {
        assert_eq!(shard_of(&key("a"), NonZeroU16::new(256).unwrap()), 121);
    }

    #[test]
    fn shard_assignment_respects_the_shard_count() {
        let shards = NonZeroU16::new(4).unwrap();
        for i in 0..100 {
            assert!(shard_of(&key(&i.to_string()), shards) < 4);
        }
    }

    #[test]
    fn round_trips_entries_and_dim() {
        let entries = vec![
            (key("a"), vec![1.0, 2.0, 3.0]),
            (key("b"), vec![-1.5, 0.0, 2.5]),
        ];
        let bytes = encode_shard(3, &entries);
        let (dim, decoded) = decode_shard(&bytes).unwrap();
        assert_eq!(dim, 3);
        assert_eq!(decoded, entries);
    }

    #[test]
    fn round_trips_an_empty_shard() {
        let bytes = encode_shard(4, &[]);
        let (dim, decoded) = decode_shard(&bytes).unwrap();
        assert_eq!(dim, 4);
        assert!(decoded.is_empty());
    }

    // Identical contents must produce identical bytes, so an untouched shard
    // hashes to the blob already stored and costs nothing to "rewrite".
    #[test]
    fn encoding_is_deterministic_regardless_of_input_order() {
        let a = vec![(key("b"), vec![1.0]), (key("a"), vec![2.0])];
        let b = vec![(key("a"), vec![2.0]), (key("b"), vec![1.0])];
        assert_eq!(encode_shard(1, &a), encode_shard(1, &b));
    }

    #[test]
    fn decode_rejects_a_bad_magic() {
        let mut bytes = encode_shard(1, &[(key("a"), vec![1.0])]);
        bytes[0] = b'X';
        assert!(matches!(decode_shard(&bytes), Err(CoreError::Backend(_))));
    }

    #[test]
    fn decode_rejects_an_unknown_version() {
        let mut bytes = encode_shard(1, &[(key("a"), vec![1.0])]);
        bytes[4] = 99;
        assert!(matches!(decode_shard(&bytes), Err(CoreError::Backend(_))));
    }

    #[test]
    fn decode_rejects_a_truncated_shard() {
        let bytes = encode_shard(2, &[(key("a"), vec![1.0, 2.0])]);
        let truncated = &bytes[..bytes.len() - 3];
        assert!(matches!(
            decode_shard(truncated),
            Err(CoreError::Backend(_))
        ));
    }

    #[test]
    fn decode_rejects_a_huge_entry_count() {
        // A shard with count field set to u32::MAX but only a few actual bytes.
        // Should reject this without attempting a multi-gigabyte allocation.
        let mut bytes = vec![];
        bytes.extend_from_slice(b"GZVS");
        bytes.push(1); // version
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // dim = 1
        bytes.extend_from_slice(&(u32::MAX).to_le_bytes()); // count = u32::MAX
        assert!(matches!(decode_shard(&bytes), Err(CoreError::Backend(_))));
    }

    #[test]
    fn decode_rejects_a_huge_dimension() {
        // A shard with dim field set to u32::MAX but only a few actual bytes.
        // Should reject this without attempting a multi-gigabyte allocation.
        let mut bytes = vec![];
        bytes.extend_from_slice(b"GZVS");
        bytes.push(1); // version
        bytes.extend_from_slice(&(u32::MAX).to_le_bytes()); // dim = u32::MAX
        bytes.extend_from_slice(&(1u32).to_le_bytes()); // count = 1
        assert!(matches!(decode_shard(&bytes), Err(CoreError::Backend(_))));
    }
}
