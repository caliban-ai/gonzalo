//! Tombstone semantics shared by every [`Store`](crate::Store) implementation
//! (ADR 0021). Stores read the current record inside their own OCC critical
//! section, ask a planner here what to do, and carry out the returned plan, so
//! every substrate reaches identical decisions by construction.

use crate::{Body, ContentHash, CoreError, Identity, Record, RecordKind, Result, Revision};

/// Ancestor list length used when a store is not configured otherwise.
/// About 90 bytes per serialized entry: ~2.9 KB at the cap.
pub const DEFAULT_ANCESTOR_CAP: usize = 32;

/// Domain string hashed to form every tombstone's revision hash. Not the empty
/// body's hash: that would let a tombstone collide with a live record edited to
/// an empty body at the same counter, and sync would miss the divergence.
pub const TOMBSTONE_DOMAIN: &[u8] = b"gonzalo:tombstone:v1";

/// The revision hash shared by every tombstone.
pub fn tombstone_hash() -> ContentHash {
    ContentHash::of(TOMBSTONE_DOMAIN)
}

/// Wall-clock milliseconds since the Unix epoch. A clock set before the epoch
/// yields `i64::MAX`: collection treats a future stamp as too young, so a
/// broken clock can only delay purging a tombstone, never make it early
/// (spec §3.8).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(i64::MAX)
}

/// Reject an ancestor cap of zero: with no ancestors, sync could never
/// fast-forward and every divergence would surface as a conflict.
pub fn validate_ancestor_cap(cap: usize) -> Result<usize> {
    if cap == 0 {
        return Err(CoreError::Backend("ancestor cap must be at least 1".into()));
    }
    Ok(cap)
}

/// The ancestor list to persist for a record whose revision is `stored`,
/// replacing `current` (if any), given the ancestors the writer supplied.
///
/// `dedup(incoming ∪ {current.revision} ∪ current.ancestors) − {stored}`,
/// sorted by `(counter desc, hash desc)` and truncated to `cap`. The order is
/// total, so every substrate persists byte-identical lists.
pub fn fold_ancestors(
    stored: &Revision,
    incoming: &[Revision],
    current: Option<&Record>,
    cap: usize,
) -> Vec<Revision> {
    let from_current = current
        .into_iter()
        .flat_map(|c| std::iter::once(&c.revision).chain(c.ancestors.iter()));
    let mut folded: Vec<Revision> = Vec::new();
    for r in incoming.iter().chain(from_current) {
        if r != stored && !folded.contains(r) {
            folded.push(r.clone());
        }
    }
    folded.sort_by(|a, b| b.counter.cmp(&a.counter).then_with(|| b.hash.cmp(&a.hash)));
    folded.truncate(cap);
    folded
}

/// The tombstone that deleting the live record `current` produces. `author`,
/// when given, is the deleter and replaces `meta.author`; otherwise the
/// tombstone keeps the last live writer's metadata.
pub fn tombstone_of(
    current: &Record,
    now_ms: i64,
    cap: usize,
    author: Option<&Identity>,
) -> Record {
    let revision = Revision {
        counter: current.revision.counter + 1,
        hash: tombstone_hash(),
    };
    let ancestors = fold_ancestors(&revision, &[], Some(current), cap);
    let mut meta = current.meta.clone();
    if let Some(author) = author {
        meta.author = author.clone();
    }
    Record {
        key: current.key.clone(),
        kind: RecordKind::Tombstone,
        revision,
        parent: Some(current.revision.clone()),
        body: Body::Inline(Vec::new()),
        meta,
        links: Vec::new(),
        ancestors,
        deleted_at: Some(now_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Body, Identity, Meta, RecordKey, RecordKind};
    use std::collections::BTreeMap;

    fn rev(counter: u64, body: &[u8]) -> Revision {
        Revision {
            counter,
            hash: ContentHash::of(body),
        }
    }

    fn live(counter: u64, body: &[u8], ancestors: Vec<Revision>) -> Record {
        Record {
            key: RecordKey::new("ns", "col", "id"),
            kind: RecordKind::Topic,
            revision: rev(counter, body),
            parent: None,
            body: Body::Inline(body.to_vec()),
            meta: Meta {
                author: Identity::new("t"),
                origin_system: "test".into(),
                created: 0,
                updated: 0,
                labels: BTreeMap::new(),
            },
            links: Vec::new(),
            ancestors,
            deleted_at: None,
        }
    }

    #[test]
    fn tombstone_hash_is_domain_separated() {
        assert_eq!(tombstone_hash(), ContentHash::of(b"gonzalo:tombstone:v1"));
        assert_ne!(tombstone_hash(), ContentHash::of(b""));
    }

    #[test]
    fn now_ms_is_after_2020() {
        assert!(now_ms() > 1_577_836_800_000);
    }

    #[test]
    fn zero_cap_is_rejected() {
        assert!(validate_ancestor_cap(0).is_err());
        assert_eq!(validate_ancestor_cap(1).unwrap(), 1);
        assert_eq!(validate_ancestor_cap(DEFAULT_ANCESTOR_CAP).unwrap(), 32);
    }

    #[test]
    fn fold_with_nothing_is_empty() {
        assert!(fold_ancestors(&rev(0, b"a"), &[], None, 32).is_empty());
    }

    #[test]
    fn fold_adds_current_and_its_ancestors_newest_first() {
        let cur = live(2, b"c", vec![rev(1, b"b"), rev(0, b"a")]);
        let got = fold_ancestors(&rev(3, b"d"), &[], Some(&cur), 32);
        assert_eq!(got, vec![rev(2, b"c"), rev(1, b"b"), rev(0, b"a")]);
    }

    #[test]
    fn fold_dedups_incoming_and_current() {
        let cur = live(1, b"b", vec![rev(0, b"a")]);
        let incoming = [rev(1, b"b"), rev(0, b"a")];
        let got = fold_ancestors(&rev(2, b"c"), &incoming, Some(&cur), 32);
        assert_eq!(got, vec![rev(1, b"b"), rev(0, b"a")]);
    }

    #[test]
    fn fold_excludes_the_stored_revision() {
        let cur = live(4, b"t", vec![rev(3, b"x")]);
        // Writing the same revision back (sync winner onto the side holding it).
        let got = fold_ancestors(&rev(4, b"t"), &[rev(3, b"y")], Some(&cur), 32);
        assert!(!got.contains(&rev(4, b"t")));
        let mut expected = vec![rev(3, b"x"), rev(3, b"y")];
        expected.sort_by(|a, b| b.hash.cmp(&a.hash));
        assert_eq!(got, expected);
    }

    #[test]
    fn fold_truncates_to_cap_keeping_newest() {
        let ancestors: Vec<Revision> = (0..10).rev().map(|i| rev(i, &[i as u8])).collect();
        let cur = live(10, b"z", ancestors);
        let got = fold_ancestors(&rev(11, b"n"), &[], Some(&cur), 3);
        assert_eq!(got, vec![rev(10, b"z"), rev(9, &[9]), rev(8, &[8])]);
    }

    #[test]
    fn tombstone_of_follows_the_spec_table() {
        let cur = live(5, b"body", vec![rev(4, b"prev")]);
        let t = tombstone_of(&cur, 1_234, 32, None);
        assert_eq!(t.key, cur.key);
        assert_eq!(t.kind, RecordKind::Tombstone);
        assert_eq!(
            t.revision,
            Revision {
                counter: 6,
                hash: tombstone_hash()
            }
        );
        assert_eq!(t.parent, Some(cur.revision.clone()));
        assert_eq!(t.body, Body::Inline(Vec::new()));
        assert_eq!(t.ancestors, vec![cur.revision.clone(), rev(4, b"prev")]);
        assert_eq!(t.deleted_at, Some(1_234));
        assert_eq!(t.meta, cur.meta);
        assert!(t.links.is_empty());
    }

    #[test]
    fn tombstone_of_stamps_the_deleter_when_given() {
        let cur = live(5, b"body", vec![]);
        let deleter = Identity::new("deleter");
        let t = tombstone_of(&cur, 1, 32, Some(&deleter));
        assert_eq!(t.meta.author, deleter);
        assert_eq!(t.meta.origin_system, cur.meta.origin_system);
    }

    #[test]
    fn independent_tombstones_of_the_same_revision_are_identical() {
        let cur = live(5, b"body", vec![]);
        assert_eq!(
            tombstone_of(&cur, 1, 32, None).revision,
            tombstone_of(&cur, 999, 32, None).revision
        );
    }

    #[test]
    fn tombstone_never_equals_an_empty_body_edit() {
        let cur = live(5, b"body", vec![]);
        assert_ne!(
            tombstone_of(&cur, 1, 32, None).revision,
            cur.revision.next(b"")
        );
    }
}
