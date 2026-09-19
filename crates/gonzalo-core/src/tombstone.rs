//! Tombstone semantics shared by every [`Store`](crate::Store) implementation
//! (ADR 0021). Stores read the current record inside their own OCC critical
//! section, ask a planner here what to do, and carry out the returned plan, so
//! every substrate reaches identical decisions by construction.

use crate::{
    Body, ContentHash, CoreError, Identity, Meta, Record, RecordKind, Result, Revision,
    store::Conflict,
};

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
    let mut folded: Vec<Revision> = incoming.iter().chain(from_current).cloned().collect();
    // `incoming` is untrusted and this runs inside a store's critical section,
    // so dedup by sorting first (equal revisions land adjacent) rather than
    // `Vec::contains`, which is O(n²).
    folded.sort_by(|a, b| b.counter.cmp(&a.counter).then_with(|| b.hash.cmp(&a.hash)));
    folded.dedup();
    folded.retain(|r| r != stored);
    folded.truncate(cap);
    folded
}

/// The tombstone that deleting the live record `current` produces. `author`,
/// when given, is the deleter and replaces `meta.author`; otherwise the
/// tombstone keeps the last live writer's metadata.
///
/// A delete is a write, so `meta.updated` becomes the delete's time while
/// `meta.created` stays the deleted record's (gonzalo#293). `deleted_at` is the
/// same instant, and is what collection reads.
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
    meta.updated = now_ms;
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

/// What a store must do for a `put`. Decided with the store's current record
/// in hand, inside its OCC critical section.
#[derive(Clone, Debug, PartialEq, Eq)]
// The plan is a short-lived return value, never stored in a collection, so the
// size difference between `Write(Record)` and the other variants doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum PutPlan {
    /// Persist exactly this record, then return
    /// `PutResult::Committed(record.revision)`. On recreation the revision was
    /// re-stamped; ancestors are always already folded.
    Write(Record),
    /// Persist nothing; return `PutResult::Conflict`.
    Conflict(Box<Conflict>),
    /// Persist nothing; return `Err(CoreError::NotFound(key))`.
    NotFound,
    /// The record may not be written through this path; return
    /// `Err(CoreError::Backend(reason.to_string()))`. Only consumer `plan_put`
    /// produces this, for a `RecordKind::Tombstone` record: deletes go through
    /// `Store::delete_as`, and replication writes tombstones through `put_raw`.
    Rejected(&'static str),
}

/// The reason a consumer `put` of a `RecordKind::Tombstone` record is
/// rejected (see [`PutPlan::Rejected`]).
pub const CONSUMER_TOMBSTONE_REJECTED: &str =
    "consumer put cannot write a tombstone; use delete_as";

/// Decide a consumer `put` (spec §3.2). A `RecordKind::Tombstone` record is
/// always `Rejected`: deletes go through `Store::delete_as`, and replication
/// writes tombstones through `put_raw`. Otherwise, a tombstone counts as
/// absent: a create (`expected == None`) recreates the key past the
/// tombstone, and any `Some(_)` is `NotFound`. Replication writes use
/// [`plan_put_raw`].
///
/// The store stamps `meta.created` and `meta.updated` on every consumer write,
/// inside the same critical section that decides the write, exactly as it
/// stamps a tombstone's `deleted_at` (gonzalo#293). A client's values are
/// overwritten: times a caller can set are times a caller can lie about, and
/// two substrates would disagree about what a record's history means.
///
/// `updated` is always the write's time. `created` is the live record's
/// `created` when one is being replaced, so it survives every edit, and the
/// write's time otherwise — including a recreation over a tombstone, which is a
/// new record at an old key, not a continuation of the deleted one.
pub fn plan_put(
    current: Option<&Record>,
    mut record: Record,
    expected: Option<Revision>,
    now_ms: i64,
    cap: usize,
) -> PutPlan {
    if record.is_tombstone() {
        return PutPlan::Rejected(CONSUMER_TOMBSTONE_REJECTED);
    }
    match current {
        None => {
            if expected.is_some() {
                return PutPlan::NotFound;
            }
            record.meta.created = now_ms;
            record.meta.updated = now_ms;
            record.ancestors = fold_ancestors(&record.revision, &record.ancestors, None, cap);
            PutPlan::Write(record)
        }
        Some(t) if t.is_tombstone() => match expected {
            None => {
                // Recreation: continue the chain past the tombstone so the new
                // record is never ordered before the delete it follows. The
                // incoming record is never a tombstone here: that case is
                // rejected above, before `current` is even consulted.
                record.revision = Revision {
                    counter: t.revision.counter + 1,
                    hash: ContentHash::of(record.body.bytes()),
                };
                record.parent = Some(t.revision.clone());
                record.deleted_at = None;
                // A recreation starts a new life at this key: the deleted
                // record's `created` does not carry over.
                record.meta.created = now_ms;
                record.meta.updated = now_ms;
                record.ancestors =
                    fold_ancestors(&record.revision, &record.ancestors, Some(t), cap);
                PutPlan::Write(record)
            }
            // Consumers never learn a tombstone's revision, so any `Some` here
            // is stale; replication writes over tombstones use `plan_put_raw`.
            Some(_) => PutPlan::NotFound,
        },
        Some(c) => {
            if expected.as_ref() == Some(&c.revision) {
                record.meta.created = c.meta.created;
                record.meta.updated = now_ms;
                record.ancestors =
                    fold_ancestors(&record.revision, &record.ancestors, Some(c), cap);
                PutPlan::Write(record)
            } else {
                PutPlan::Conflict(Box::new(Conflict {
                    key: record.key.clone(),
                    expected,
                    current: c.clone(),
                }))
            }
        }
    }
}

/// Decide a replication write (`Store::put_raw`, spec §3.2). Never re-stamps:
/// a tombstone is an ordinary record here. A create that finds anything stored
/// (live or tombstone) is a `Conflict` carrying it, so sync re-reads instead of
/// turning a copy into a recreation that would resurrect a deleted record.
///
/// `now_ms` is deliberately unused: a replication write copies a record that
/// already happened elsewhere, so it keeps the source's `created` and `updated`
/// (gonzalo#293). Stamping them here would make every sync look like an edit
/// and lose when the record was really written. The parameter is kept so both
/// planners share one signature, which is what lets a store hand either to its
/// locked read→plan→write path.
pub fn plan_put_raw(
    current: Option<&Record>,
    mut record: Record,
    expected: Option<Revision>,
    _now_ms: i64,
    cap: usize,
) -> PutPlan {
    match (current, expected) {
        (None, None) => {
            record.ancestors = fold_ancestors(&record.revision, &record.ancestors, None, cap);
            PutPlan::Write(record)
        }
        (None, Some(_)) => PutPlan::NotFound,
        (Some(c), Some(e)) if e == c.revision => {
            record.ancestors = fold_ancestors(&record.revision, &record.ancestors, Some(c), cap);
            PutPlan::Write(record)
        }
        (Some(c), expected) => PutPlan::Conflict(Box::new(Conflict {
            key: record.key.clone(),
            expected,
            current: c.clone(),
        })),
    }
}

/// What a store must do for a `delete`.
#[derive(Clone, Debug, PartialEq, Eq)]
// The plan is a short-lived return value, never stored in a collection, so the
// size difference between `Write(Record)` and the other variants doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum DeletePlan {
    /// Persist this tombstone, then return `DeleteResult::Deleted`.
    Write(Record),
    /// Persist nothing; return `DeleteResult::Deleted`.
    Noop,
    /// Persist nothing; return `DeleteResult::Conflict`.
    Conflict(Box<Conflict>),
}

/// Decide a conditional `delete` (spec §3.2). Deleting an absent key writes
/// nothing: there is no revision to order a tombstone against a peer's copy.
/// Deleting a tombstone writes nothing: the chain does not advance.
pub fn plan_delete(
    current: Option<&Record>,
    expected: Option<Revision>,
    now_ms: i64,
    cap: usize,
    author: Option<&Identity>,
) -> DeletePlan {
    match current {
        None => DeletePlan::Noop,
        Some(c) if c.is_tombstone() => DeletePlan::Noop,
        Some(c) if expected.is_none() || expected.as_ref() == Some(&c.revision) => {
            DeletePlan::Write(tombstone_of(c, now_ms, cap, author))
        }
        Some(c) => DeletePlan::Conflict(Box::new(Conflict {
            key: c.key.clone(),
            expected,
            current: c.clone(),
        })),
    }
}

/// What a store must do for a `purge`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PurgePlan {
    /// Physically remove the stored record; return `DeleteResult::Deleted`.
    Remove,
    /// Nothing is stored; return `DeleteResult::Deleted`.
    Noop,
    /// Remove nothing; return `DeleteResult::Conflict`.
    Conflict(Box<Conflict>),
}

/// Decide a `purge`: remove only the exact revision named, so a key recreated
/// after its tombstone was listed survives collection.
pub fn plan_purge(current: Option<&Record>, expected: &Revision) -> PurgePlan {
    match current {
        None => PurgePlan::Noop,
        Some(c) if &c.revision == expected => PurgePlan::Remove,
        Some(c) => PurgePlan::Conflict(Box::new(Conflict {
            key: c.key.clone(),
            expected: Some(expected.clone()),
            current: c.clone(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Body, Identity, Meta, RecordKey, RecordKind};
    use std::collections::BTreeMap;

    /// A fixed clock for the tests that are about a plan's shape rather than
    /// its stamps (gonzalo#293).
    const NOW: i64 = 1_700_000_000_000;

    /// [`super::plan_put`] at [`NOW`]. The stamping itself is covered by the
    /// `records_are_stamped_*` tests below, which call the planner directly.
    fn plan_put(
        current: Option<&Record>,
        record: Record,
        expected: Option<Revision>,
        cap: usize,
    ) -> PutPlan {
        super::plan_put(current, record, expected, NOW, cap)
    }

    /// [`super::plan_put_raw`] at [`NOW`], which it ignores.
    fn plan_put_raw(
        current: Option<&Record>,
        record: Record,
        expected: Option<Revision>,
        cap: usize,
    ) -> PutPlan {
        super::plan_put_raw(current, record, expected, NOW, cap)
    }

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

    // ---- record times (#293) ----

    /// The `PutPlan::Write` a plan produced, or a panic naming what it was.
    fn written(plan: PutPlan) -> Record {
        match plan {
            PutPlan::Write(r) => r,
            other => panic!("expected a write, got {other:?}"),
        }
    }

    #[test]
    fn a_create_is_stamped_with_the_write_time() {
        let mut incoming = live(0, b"v0", vec![]);
        // A client's values carry no authority; the store decides.
        incoming.meta.created = 999;
        incoming.meta.updated = 999;

        let stored = written(super::plan_put(None, incoming, None, NOW, 32));

        assert_eq!(stored.meta.created, NOW);
        assert_eq!(stored.meta.updated, NOW);
    }

    #[test]
    fn an_update_keeps_created_and_advances_updated() {
        let mut current = live(0, b"v0", vec![]);
        current.meta.created = NOW;
        current.meta.updated = NOW;
        let later = NOW + 5_000;

        let stored = written(super::plan_put(
            Some(&current),
            live(1, b"v1", vec![]),
            Some(current.revision.clone()),
            later,
            32,
        ));

        assert_eq!(stored.meta.created, NOW, "created survives every edit");
        assert_eq!(stored.meta.updated, later);
    }

    #[test]
    fn a_recreation_over_a_tombstone_starts_a_new_created() {
        // The key is reused, but the record is a new one: carrying the deleted
        // record's `created` across would claim a history it doesn't have.
        let mut deleted = live(0, b"v0", vec![]);
        deleted.meta.created = NOW;
        let tomb = tombstone_of(&deleted, NOW + 1_000, 32, None);
        let later = NOW + 9_000;

        let stored = written(super::plan_put(
            Some(&tomb),
            live(0, b"fresh", vec![]),
            None,
            later,
            32,
        ));

        assert_eq!(stored.meta.created, later);
        assert_eq!(stored.meta.updated, later);
    }

    #[test]
    fn a_tombstone_records_the_delete_as_its_update() {
        let mut current = live(0, b"v0", vec![]);
        current.meta.created = NOW;
        current.meta.updated = NOW;

        let tomb = tombstone_of(&current, NOW + 250, 32, None);

        assert_eq!(tomb.meta.created, NOW, "the record was created when it was");
        assert_eq!(tomb.meta.updated, NOW + 250);
        assert_eq!(tomb.deleted_at, Some(NOW + 250));
    }

    #[test]
    fn a_replication_write_keeps_the_source_times() {
        // What sync copies happened elsewhere, at the time the source says.
        let mut incoming = live(0, b"v0", vec![]);
        incoming.meta.created = 111;
        incoming.meta.updated = 222;

        let stored = written(super::plan_put_raw(None, incoming, None, NOW, 32));

        assert_eq!((stored.meta.created, stored.meta.updated), (111, 222));
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
        assert_eq!(t.meta.author, cur.meta.author);
        assert_eq!(t.meta.origin_system, cur.meta.origin_system);
        assert_eq!(t.meta.created, cur.meta.created);
        // The delete is the record's last update (#293).
        assert_eq!(t.meta.updated, 1_234);
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

    fn tomb(counter: u64) -> Record {
        let prior = live(counter - 1, b"was", vec![]);
        tombstone_of(&prior, 42, 32, None)
    }

    // ---- plan_put ----

    #[test]
    fn put_create_on_absent_writes() {
        let rec = live(0, b"new", vec![]);
        let mut stamped = rec.clone();
        // The only thing the planner changes is the times (#293).
        stamped.meta.created = NOW;
        stamped.meta.updated = NOW;
        assert_eq!(plan_put(None, rec, None, 32), PutPlan::Write(stamped));
    }

    #[test]
    fn put_expected_on_absent_is_not_found() {
        let rec = live(0, b"new", vec![]);
        assert_eq!(
            plan_put(None, rec, Some(rev(0, b"x")), 32),
            PutPlan::NotFound
        );
    }

    #[test]
    fn put_update_with_matching_expected_folds_ancestors() {
        let cur = live(0, b"v0", vec![]);
        let mut next = live(1, b"v1", vec![]);
        next.parent = Some(cur.revision.clone());
        let PutPlan::Write(stored) =
            plan_put(Some(&cur), next.clone(), Some(cur.revision.clone()), 32)
        else {
            panic!("expected Write");
        };
        assert_eq!(stored.revision, next.revision);
        assert_eq!(stored.ancestors, vec![cur.revision.clone()]);
    }

    #[test]
    fn put_over_live_with_wrong_or_no_expected_conflicts() {
        let cur = live(0, b"v0", vec![]);
        for expected in [None, Some(rev(9, b"nope"))] {
            match plan_put(Some(&cur), live(1, b"v1", vec![]), expected.clone(), 32) {
                PutPlan::Conflict(c) => {
                    assert_eq!(c.current, cur);
                    assert_eq!(c.expected, expected);
                }
                other => panic!("expected Conflict, got {other:?}"),
            }
        }
    }

    #[test]
    fn put_none_over_tombstone_recreates_continuing_the_chain() {
        let t = tomb(3);
        let fresh = live(0, b"again", vec![]);
        let PutPlan::Write(stored) = plan_put(Some(&t), fresh, None, 32) else {
            panic!("expected Write");
        };
        assert_eq!(stored.revision, rev(t.revision.counter + 1, b"again"));
        assert_eq!(stored.parent, Some(t.revision.clone()));
        assert_eq!(stored.deleted_at, None);
        assert_eq!(stored.ancestors.first(), Some(&t.revision));
        assert_eq!(stored.kind, RecordKind::Topic);
    }

    #[test]
    fn consumer_put_of_a_tombstone_is_rejected() {
        let tombstone_record = tomb(1);

        // Absent key.
        assert_eq!(
            plan_put(None, tombstone_record.clone(), None, 32),
            PutPlan::Rejected(CONSUMER_TOMBSTONE_REJECTED)
        );

        // Live key, matching expected.
        let cur = live(0, b"v", vec![]);
        assert_eq!(
            plan_put(
                Some(&cur),
                tombstone_record.clone(),
                Some(cur.revision.clone()),
                32
            ),
            PutPlan::Rejected(CONSUMER_TOMBSTONE_REJECTED)
        );

        // Tombstoned key.
        let t = tomb(3);
        assert_eq!(
            plan_put(Some(&t), tombstone_record, None, 32),
            PutPlan::Rejected(CONSUMER_TOMBSTONE_REJECTED)
        );
    }

    #[test]
    fn consumer_put_with_any_expected_over_tombstone_is_not_found() {
        let t = tomb(3);
        // Even the tombstone's own revision: replication uses plan_put_raw.
        for expected in [rev(1, b"stale"), t.revision.clone()] {
            assert_eq!(
                plan_put(Some(&t), live(0, b"x", vec![]), Some(expected), 32),
                PutPlan::NotFound
            );
        }
    }

    // ---- plan_put_raw ----

    #[test]
    fn put_raw_create_on_absent_writes_verbatim_including_tombstones() {
        let t = tomb(3);
        assert_eq!(plan_put_raw(None, t.clone(), None, 32), PutPlan::Write(t));
    }

    #[test]
    fn put_raw_expected_on_absent_is_not_found() {
        assert_eq!(
            plan_put_raw(None, live(0, b"x", vec![]), Some(rev(0, b"x")), 32),
            PutPlan::NotFound
        );
    }

    #[test]
    fn put_raw_create_over_tombstone_conflicts_carrying_it() {
        let t = tomb(3);
        match plan_put_raw(Some(&t), live(0, b"copy", vec![]), None, 32) {
            PutPlan::Conflict(c) => {
                assert!(c.current.is_tombstone());
                assert_eq!(c.current, t);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn put_raw_overwrite_of_tombstone_never_restamps() {
        let t = tomb(3);
        let mut incoming = live(9, b"peer", vec![t.revision.clone()]);
        incoming.parent = Some(t.revision.clone());
        let PutPlan::Write(stored) =
            plan_put_raw(Some(&t), incoming.clone(), Some(t.revision.clone()), 32)
        else {
            panic!("expected Write");
        };
        assert_eq!(
            stored.revision, incoming.revision,
            "revision stored unchanged"
        );
        assert_eq!(stored.ancestors.first(), Some(&t.revision));
    }

    #[test]
    fn put_raw_over_live_with_wrong_expected_conflicts() {
        let cur = live(0, b"v0", vec![]);
        assert!(matches!(
            plan_put_raw(Some(&cur), live(1, b"v1", vec![]), Some(rev(7, b"no")), 32),
            PutPlan::Conflict(_)
        ));
    }

    // ---- plan_delete ----

    #[test]
    fn delete_absent_is_noop() {
        assert_eq!(plan_delete(None, None, 1, 32, None), DeletePlan::Noop);
        assert_eq!(
            plan_delete(None, Some(rev(0, b"x")), 1, 32, None),
            DeletePlan::Noop
        );
    }

    #[test]
    fn delete_tombstone_is_noop() {
        let t = tomb(2);
        assert_eq!(plan_delete(Some(&t), None, 1, 32, None), DeletePlan::Noop);
        assert_eq!(
            plan_delete(Some(&t), Some(rev(7, b"x")), 1, 32, None),
            DeletePlan::Noop
        );
    }

    #[test]
    fn delete_live_writes_its_tombstone() {
        let cur = live(4, b"v", vec![]);
        let expected_tomb = tombstone_of(&cur, 77, 32, None);
        assert_eq!(
            plan_delete(Some(&cur), None, 77, 32, None),
            DeletePlan::Write(expected_tomb.clone())
        );
        assert_eq!(
            plan_delete(Some(&cur), Some(cur.revision.clone()), 77, 32, None),
            DeletePlan::Write(expected_tomb)
        );
    }

    #[test]
    fn delete_live_with_stale_expected_conflicts() {
        let cur = live(4, b"v", vec![]);
        match plan_delete(Some(&cur), Some(rev(1, b"old")), 77, 32, None) {
            DeletePlan::Conflict(c) => assert_eq!(c.current, cur),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    // ---- plan_purge ----

    #[test]
    fn purge_absent_is_noop() {
        assert_eq!(plan_purge(None, &rev(0, b"x")), PurgePlan::Noop);
    }

    #[test]
    fn purge_matching_revision_removes() {
        let t = tomb(2);
        assert_eq!(plan_purge(Some(&t), &t.revision), PurgePlan::Remove);
    }

    #[test]
    fn purge_after_recreation_conflicts() {
        let t = tomb(2);
        let PutPlan::Write(recreated) = plan_put(Some(&t), live(0, b"back", vec![]), None, 32)
        else {
            panic!("expected Write");
        };
        match plan_purge(Some(&recreated), &t.revision) {
            PurgePlan::Conflict(c) => {
                assert_eq!(c.current, recreated);
                assert_eq!(c.expected, Some(t.revision.clone()));
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
}

/// Ancestors for a record that reconciles two diverged records `a` and `b`: a
/// sync or pull merge result, or a tombstone winner. Both revisions and both
/// ancestor lists are folded (spec §3.4), minus `stored`, sorted by
/// `(counter desc, hash desc)` and truncated to `cap`.
///
/// Sync passes a lossless cap (`a.ancestors.len() + b.ancestors.len() + 2`)
/// and lets each destination store's `plan_put_raw` truncate to its own cap on
/// write. Git pull writes the index directly and passes the store's cap.
pub fn reconciled_ancestors(
    stored: &Revision,
    a: &Record,
    b: &Record,
    cap: usize,
) -> Vec<Revision> {
    let mut incoming = Vec::with_capacity(a.ancestors.len() + b.ancestors.len() + 2);
    incoming.push(a.revision.clone());
    incoming.extend(a.ancestors.iter().cloned());
    incoming.push(b.revision.clone());
    incoming.extend(b.ancestors.iter().cloned());
    fold_ancestors(stored, &incoming, None, cap)
}

/// Resolve two diverged tombstones for one key (spec §3.4, §3.5): the one with
/// the higher `(counter, hash)` wins and carries ancestors reconciled from both.
///
/// Symmetric whenever `a.revision != b.revision`. Callers skip equal revisions
/// before reaching this, because independent deletes of the same revision are
/// already in sync. Both arguments must be tombstones.
pub fn tombstone_winner(a: &Record, b: &Record, cap: usize) -> Record {
    debug_assert!(
        a.is_tombstone() && b.is_tombstone(),
        "tombstone_winner needs two tombstones"
    );
    let a_wins = (a.revision.counter, &a.revision.hash) >= (b.revision.counter, &b.revision.hash);
    let mut winner = if a_wins { a.clone() } else { b.clone() };
    winner.ancestors = reconciled_ancestors(&winner.revision, a, b, cap);
    winner
}

/// The record reconciling two diverged live records `a` and `b` into merged
/// `body` (spec §3.4, §3.5): revision `max(counter) + 1` over `body`, parent =
/// the higher-counter side, labels and links unioned, both chains folded. On a
/// label-key collision `b` wins, and on a counter tie the parent is `a`. Sync
/// calls this with `(a, b)`; pull calls it with `(local, remote)`.
pub fn reconciled_record(
    a: &Record,
    b: &Record,
    body: Body,
    author: Identity,
    origin_system: &str,
    cap: usize,
) -> Record {
    let revision = Revision {
        counter: a.revision.counter.max(b.revision.counter) + 1,
        hash: ContentHash::of(body.bytes()),
    };
    let ancestors = reconciled_ancestors(&revision, a, b, cap);
    let mut labels = a.meta.labels.clone();
    labels.extend(b.meta.labels.clone());
    let mut links = a.links.clone();
    for l in &b.links {
        if !links.contains(l) {
            links.push(l.clone());
        }
    }
    let parent = if a.revision.counter >= b.revision.counter {
        a.revision.clone()
    } else {
        b.revision.clone()
    };
    Record {
        key: a.key.clone(),
        kind: a.kind,
        revision,
        parent: Some(parent),
        body,
        meta: Meta {
            author,
            origin_system: origin_system.into(),
            created: a.meta.created.min(b.meta.created),
            updated: a.meta.updated.max(b.meta.updated),
            labels,
        },
        links,
        ancestors,
        deleted_at: None,
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use crate::RecordKey;
    use std::collections::BTreeMap;

    fn rev(counter: u64, tag: &str) -> Revision {
        Revision {
            counter,
            hash: ContentHash::of(tag.as_bytes()),
        }
    }

    fn record(kind: RecordKind, revision: Revision, ancestors: Vec<Revision>) -> Record {
        Record {
            key: RecordKey::new("ns", "col", "k"),
            kind,
            revision,
            parent: None,
            body: Body::Inline(Vec::new()),
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

    fn tomb(counter: u64, ancestors: Vec<Revision>) -> Record {
        let mut r = record(
            RecordKind::Tombstone,
            Revision {
                counter,
                hash: tombstone_hash(),
            },
            ancestors,
        );
        r.deleted_at = Some(1_000);
        r
    }

    #[test]
    fn reconciled_ancestors_unions_both_revisions_and_both_chains() {
        let base = rev(0, "base");
        let ra = rev(1, "a");
        let rb = rev(1, "b");
        let a = record(RecordKind::Topic, ra.clone(), vec![base.clone()]);
        let b = record(RecordKind::Topic, rb.clone(), vec![base.clone()]);
        let merged = rev(2, "merged");

        let got = reconciled_ancestors(&merged, &a, &b, DEFAULT_ANCESTOR_CAP);

        // Same result as folding the four inputs by hand: same order rule.
        let expected = fold_ancestors(
            &merged,
            &[ra.clone(), base.clone(), rb.clone(), base.clone()],
            None,
            DEFAULT_ANCESTOR_CAP,
        );
        assert_eq!(got, expected);
        assert_eq!(got.len(), 3, "base is deduplicated");
        assert!(got.contains(&ra) && got.contains(&rb) && got.contains(&base));
    }

    #[test]
    fn reconciled_ancestors_excludes_stored_and_truncates_to_cap() {
        let a = record(
            RecordKind::Topic,
            rev(3, "a3"),
            vec![rev(2, "a2"), rev(1, "a1")],
        );
        let b = record(RecordKind::Topic, rev(3, "b3"), vec![rev(2, "b2")]);

        // Stored revision == a's revision (a sync writing a's winner back onto a).
        let got = reconciled_ancestors(&a.revision, &a, &b, 2);

        assert_eq!(got.len(), 2);
        assert!(
            !got.contains(&a.revision),
            "own revision is never an ancestor"
        );
        assert_eq!(got[0], b.revision, "newest remaining entry comes first");
    }

    #[test]
    fn tombstone_winner_takes_higher_counter_and_folds_both_chains() {
        let base = rev(0, "base");
        let r1 = rev(1, "r1");
        let low = tomb(1, vec![base.clone()]);
        let high = tomb(2, vec![r1.clone(), base.clone()]);

        let w = tombstone_winner(&low, &high, DEFAULT_ANCESTOR_CAP);

        assert!(w.is_tombstone());
        assert_eq!(w.revision, high.revision);
        assert_eq!(w.deleted_at, high.deleted_at);
        assert_eq!(w.ancestors.len(), 3);
        assert!(w.ancestors.contains(&low.revision));
        assert!(w.ancestors.contains(&r1));
        assert!(w.ancestors.contains(&base));
    }

    #[test]
    fn tombstone_winner_is_symmetric() {
        let base = rev(0, "base");
        let low = tomb(1, vec![base.clone()]);
        let high = tomb(2, vec![rev(1, "r1"), base]);
        assert_eq!(
            tombstone_winner(&low, &high, DEFAULT_ANCESTOR_CAP),
            tombstone_winner(&high, &low, DEFAULT_ANCESTOR_CAP)
        );
    }

    #[test]
    fn reconciled_record_folds_both_chains_and_picks_parent() {
        let mut a = record(RecordKind::Topic, rev(3, "a3"), vec![rev(2, "a2")]);
        a.meta.labels.insert("color".into(), "red".into());
        let mut b = record(RecordKind::Topic, rev(5, "b5"), vec![rev(4, "b4")]);
        b.meta.labels.insert("size".into(), "large".into());

        let merged = reconciled_record(
            &a,
            &b,
            Body::Inline(b"merged-body".to_vec()),
            Identity::new("merger"),
            "test-origin",
            DEFAULT_ANCESTOR_CAP,
        );

        // Revision counter is max(3, 5) + 1.
        assert_eq!(merged.revision.counter, 6);
        // b has the higher counter, so b's revision is the parent.
        assert_eq!(merged.parent, Some(b.revision.clone()));
        // Both chains are folded: a's and b's revisions and ancestors all appear.
        assert!(merged.ancestors.contains(&a.revision));
        assert!(merged.ancestors.contains(&rev(2, "a2")));
        assert!(merged.ancestors.contains(&b.revision));
        assert!(merged.ancestors.contains(&rev(4, "b4")));
        // Labels from both sides are unioned.
        assert_eq!(merged.meta.labels.get("color"), Some(&"red".to_string()));
        assert_eq!(merged.meta.labels.get("size"), Some(&"large".to_string()));
        assert_eq!(merged.meta.author, Identity::new("merger"));
        assert_eq!(merged.meta.origin_system, "test-origin");
        assert_eq!(merged.deleted_at, None);
    }
}
