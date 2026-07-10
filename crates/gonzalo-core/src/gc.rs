//! Mark-sweep garbage collection for content-addressed slices (ADR 0012).
//!
//! A slice blob is *live* iff some live manifest references its hash. GC marks
//! the union of every live manifest's hashes, then sweeps any stored blob
//! outside that set. Liveness is derived from the manifests themselves rather
//! than a maintained refcount, so it is self-correcting: a missed event can
//! leave a slice briefly un-swept, never wrongly deleted, and never leaked
//! forever the way a drifted refcount would.

use crate::{BlobStore, ContentHash, Manifest, Result};
use std::collections::BTreeSet;

/// What a GC sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Hashes of blobs deleted because no live manifest referenced them.
    pub freed: Vec<ContentHash>,
    /// Count of blobs kept because they are still referenced.
    pub retained: usize,
}

/// The mark set: every slice hash referenced by any of the `manifests`.
pub fn live_slice_hashes<'a>(
    manifests: impl IntoIterator<Item = &'a Manifest>,
) -> BTreeSet<ContentHash> {
    manifests
        .into_iter()
        .flat_map(|m| m.entries.values().cloned())
        .collect()
}

/// The sweep set: hashes present in `all` but referenced by no live manifest,
/// returned sorted (`all - live`).
pub fn unreferenced_slices(all: &[ContentHash], live: &BTreeSet<ContentHash>) -> Vec<ContentHash> {
    let mut garbage: Vec<ContentHash> =
        all.iter().filter(|h| !live.contains(*h)).cloned().collect();
    garbage.sort();
    garbage.dedup();
    garbage
}

/// Sweep `blobs`: delete every stored slice no `live_manifests` entry
/// references, and report what was freed vs. retained. Mark-sweep — see the
/// module docs for why this is preferred over refcounting.
pub async fn gc_blobs<B: BlobStore>(blobs: &B, live_manifests: &[Manifest]) -> Result<GcReport> {
    let all = blobs.list_blobs().await?;
    let live = live_slice_hashes(live_manifests);
    let freed = unreferenced_slices(&all, &live);
    for hash in &freed {
        blobs.delete_blob(hash).await?;
    }
    // `all` may list a hash more than once (unspecified order, no dedup
    // guarantee), while `freed` is deduplicated. Count retained from a
    // deduplicated set of all hashes so a repeated hash doesn't inflate the
    // total and skew `retained`.
    let distinct = all.iter().cloned().collect::<BTreeSet<_>>().len();
    let retained = distinct - freed.len();
    Ok(GcReport { freed, retained })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> ContentHash {
        ContentHash::of(s.as_bytes())
    }

    #[test]
    fn live_set_unions_all_manifest_references() {
        let mut a = Manifest::new();
        a.insert("x.rs", h("1"));
        a.insert("y.rs", h("2"));
        let mut b = Manifest::new();
        b.insert("z.rs", h("2")); // shared slice, counted once
        b.insert("w.rs", h("3"));

        let live = live_slice_hashes([&a, &b]);
        assert_eq!(live, BTreeSet::from([h("1"), h("2"), h("3")]));
    }

    #[test]
    fn live_set_of_no_manifests_is_empty() {
        assert!(live_slice_hashes([]).is_empty());
    }

    #[test]
    fn unreferenced_is_all_minus_live_sorted() {
        let all = vec![h("keep"), h("drop"), h("keep2")];
        let live = BTreeSet::from([h("keep"), h("keep2")]);
        let garbage = unreferenced_slices(&all, &live);
        let mut want = vec![h("drop")];
        want.sort();
        assert_eq!(garbage, want);
    }

    #[test]
    fn unreferenced_dedups_repeated_input_hashes() {
        let all = vec![h("dup"), h("dup"), h("live")];
        let live = BTreeSet::from([h("live")]);
        assert_eq!(unreferenced_slices(&all, &live), vec![h("dup")]);
    }

    #[test]
    fn nothing_unreferenced_when_all_are_live() {
        let all = vec![h("a"), h("b")];
        let live = BTreeSet::from([h("a"), h("b")]);
        assert!(unreferenced_slices(&all, &live).is_empty());
    }

    /// A `BlobStore` whose `list_blobs` returns a fixed, possibly-duplicated
    /// list of hashes and records which hashes `delete_blob` was called on.
    #[derive(Default)]
    struct FakeBlobs {
        listed: Vec<ContentHash>,
        deleted: std::sync::Mutex<Vec<ContentHash>>,
    }

    #[async_trait::async_trait]
    impl BlobStore for FakeBlobs {
        async fn put_blob(&self, content: &[u8]) -> Result<ContentHash> {
            Ok(ContentHash::of(content))
        }
        async fn get_blob(&self, _hash: &ContentHash) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn list_blobs(&self) -> Result<Vec<ContentHash>> {
            Ok(self.listed.clone())
        }
        async fn delete_blob(&self, hash: &ContentHash) -> Result<()> {
            self.deleted.lock().unwrap().push(hash.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn retained_counts_distinct_blobs_despite_duplicate_listing() {
        // `list_blobs` reports `keep` twice and one unreferenced `drop`. There
        // are two distinct blobs; one is freed, so exactly one is retained — the
        // duplicate listing must not inflate `retained` to 2 (regression: #156).
        let blobs = FakeBlobs {
            listed: vec![h("keep"), h("keep"), h("drop")],
            deleted: Default::default(),
        };
        let mut m = Manifest::new();
        m.insert("f.rs", h("keep"));

        let report = gc_blobs(&blobs, &[m]).await.unwrap();

        assert_eq!(report.freed, vec![h("drop")]);
        assert_eq!(report.retained, 1);
        assert_eq!(*blobs.deleted.lock().unwrap(), vec![h("drop")]);
    }
}
