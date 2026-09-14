//! Tombstone collection (spec §3.8, gonzalo#203).
//!
//! A tombstone is what stops a deleted record from resurrecting on the next
//! sync, so purging one is only safe once every peer has synced past the
//! delete. Only the operator knows how long that takes. Collection is
//! therefore explicit, never automatic, and takes the horizon as a required
//! argument with no default. Not to be confused with [`gc_blobs`](crate::gc_blobs),
//! which sweeps orphaned code-graph slices.

use crate::{DeleteResult, KeyPrefix, RecordKey, Result, Store};
use std::time::Duration;

/// What a collection run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "a CollectReport may list conflicts (keys recreated during collection)"]
pub struct CollectReport {
    /// Tombstones purged by this run, including any found already gone when
    /// purged.
    pub purged: Vec<RecordKey>,
    /// Tombstones kept because they have no `deleted_at`.
    pub unstamped: usize,
    /// Purge lost an OCC race (the key was recreated during collection).
    pub conflicts: Vec<RecordKey>,
}

/// Purge tombstones under `prefix` whose `deleted_at` is at least `horizon` old.
///
/// Live records are never touched. A tombstone with no `deleted_at` is kept
/// and counted in [`CollectReport::unstamped`]. A future-dated `deleted_at`
/// (clock skew) gives a negative age and is kept, so skew can delay collection
/// but can't trigger it early. A horizon longer than any representable age
/// purges nothing. `now_ms` is a parameter so tests control time; the CLI
/// passes the system clock.
///
/// Stops at the first store error and returns it; tombstones already purged
/// stay purged, so a re-run is safe. Over a daemon, purge needs an admin
/// token: a non-admin token fails on the first eligible tombstone, before
/// anything is purged. A daemon predating replication fails on `list_raw`
/// with `DAEMON_PREDATES_REPLICATION`.
pub async fn collect(
    store: &dyn Store,
    prefix: &KeyPrefix,
    horizon: Duration,
    now_ms: i64,
) -> Result<CollectReport> {
    let horizon_ms = i128::try_from(horizon.as_millis()).unwrap_or(i128::MAX);
    let mut report = CollectReport::default();
    for key in store.list_raw(prefix).await? {
        if !prefix.matches(&key) {
            continue;
        }
        let Some(record) = store.get_raw(&key).await? else {
            continue;
        };
        if !record.is_tombstone() {
            continue;
        }
        let Some(deleted_at) = record.deleted_at else {
            report.unstamped += 1;
            continue;
        };
        if i128::from(now_ms) - i128::from(deleted_at) < horizon_ms {
            continue;
        }
        match store.purge(&key, record.revision).await? {
            DeleteResult::Deleted => report.purged.push(key),
            DeleteResult::Conflict(_) => report.conflicts.push(key),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PutResult;
    use crate::memstore::MemStore;
    use crate::test_support::{Hook, HookedStore, rec, seed};
    use crate::tombstone::{DEFAULT_ANCESTOR_CAP, tombstone_of};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    const DAY_MS: i64 = 86_400_000;
    const NOW: i64 = 100 * DAY_MS;
    const THIRTY_DAYS: Duration = Duration::from_secs(30 * 86_400);

    /// Store a tombstone for `ns/col/id` directly, with an arbitrary
    /// `deleted_at`, the way sync would replicate one into an empty store.
    async fn seed_tombstone(
        store: &dyn Store,
        ns: &str,
        col: &str,
        id: &str,
        deleted_at: Option<i64>,
    ) {
        let live = rec(ns, col, id, "was live");
        let mut tomb = tombstone_of(&live, 0, DEFAULT_ANCESTOR_CAP, None);
        tomb.deleted_at = deleted_at;
        // `put_raw` is the replication write: stores the tombstone unchanged.
        assert!(matches!(
            store.put_raw(tomb, None).await.unwrap(),
            PutResult::Committed(_)
        ));
    }

    #[tokio::test]
    async fn purges_only_stamped_tombstones_at_least_horizon_old() {
        let store = MemStore::new();
        seed_tombstone(&store, "ns", "col", "old", Some(NOW - 31 * DAY_MS)).await;
        seed_tombstone(&store, "ns", "col", "exact", Some(NOW - 30 * DAY_MS)).await;
        seed_tombstone(&store, "ns", "col", "young", Some(NOW - DAY_MS)).await;
        seed_tombstone(&store, "ns", "col", "future", Some(NOW + DAY_MS)).await;
        seed_tombstone(&store, "ns", "col", "unstamped", None).await;
        seed(&store, rec("ns", "col", "live", "content")).await;
        let live_key = RecordKey::new("ns", "col", "live");
        let live_before = store.raw_snapshot()[&live_key].clone();

        let report = collect(&store, &KeyPrefix::default(), THIRTY_DAYS, NOW)
            .await
            .unwrap();

        assert_eq!(
            report.purged,
            vec![
                RecordKey::new("ns", "col", "exact"),
                RecordKey::new("ns", "col", "old"),
            ]
        );
        assert_eq!(report.unstamped, 1);
        assert!(report.conflicts.is_empty());

        let after = store.raw_snapshot();
        assert!(!after.contains_key(&RecordKey::new("ns", "col", "old")));
        assert!(!after.contains_key(&RecordKey::new("ns", "col", "exact")));
        for kept in ["young", "future", "unstamped"] {
            let key = RecordKey::new("ns", "col", kept);
            assert!(after[&key].is_tombstone(), "{kept} must be kept");
        }
        assert_eq!(after[&live_key], live_before, "live record untouched");
    }

    #[tokio::test]
    async fn respects_the_prefix() {
        let store = MemStore::new();
        let old = Some(NOW - 60 * DAY_MS);
        seed_tombstone(&store, "ns", "col", "in", old).await;
        seed_tombstone(&store, "ns", "sibling", "out1", old).await;
        seed_tombstone(&store, "other", "col", "out2", old).await;

        let report = collect(
            &store,
            &KeyPrefix {
                namespace: Some("ns".into()),
                collection: Some("col".into()),
            },
            THIRTY_DAYS,
            NOW,
        )
        .await
        .unwrap();

        assert_eq!(report.purged, vec![RecordKey::new("ns", "col", "in")]);
        let after = store.raw_snapshot();
        assert!(after.contains_key(&RecordKey::new("ns", "sibling", "out1")));
        assert!(after.contains_key(&RecordKey::new("other", "col", "out2")));
    }

    #[tokio::test]
    async fn purges_a_tombstone_written_by_delete() {
        let store = MemStore::new().with_clock(NOW - 40 * DAY_MS);
        let key = RecordKey::new("ns", "col", "k");
        seed(&store, rec("ns", "col", "k", "x")).await;
        assert_eq!(
            store.delete(&key, None).await.unwrap(),
            DeleteResult::Deleted
        );

        let report = collect(&store, &KeyPrefix::default(), THIRTY_DAYS, NOW)
            .await
            .unwrap();

        assert_eq!(report.purged, vec![key.clone()]);
        assert_eq!(store.get_raw(&key).await.unwrap(), None);
        assert!(
            store
                .list_raw(&KeyPrefix::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_horizon_too_large_for_i64_millis_purges_nothing() {
        let store = MemStore::new();
        seed_tombstone(&store, "ns", "col", "ancient", Some(i64::MIN / 2)).await;
        seed_tombstone(&store, "ns", "col", "floor", Some(i64::MIN)).await;

        let report = collect(&store, &KeyPrefix::default(), Duration::MAX, NOW)
            .await
            .unwrap();

        assert!(report.purged.is_empty());
        assert_eq!(store.raw_snapshot().len(), 2);
    }

    #[tokio::test]
    async fn recreation_during_collection_is_a_conflict_and_the_live_record_survives() {
        let key = RecordKey::new("ns", "col", "k");
        let store = HookedStore {
            inner: MemStore::new(),
            hook: Hook::RecreateBeforePurge(key.clone()),
            fired: AtomicBool::new(false),
        };
        seed_tombstone(&store, "ns", "col", "k", Some(NOW - 60 * DAY_MS)).await;
        let tomb_rev = store.get_raw(&key).await.unwrap().unwrap().revision;

        let report = collect(&store, &KeyPrefix::default(), THIRTY_DAYS, NOW)
            .await
            .unwrap();

        assert!(report.purged.is_empty());
        assert_eq!(report.conflicts, vec![key.clone()]);
        let live = store
            .get(&key)
            .await
            .unwrap()
            .expect("recreated record survives");
        assert_eq!(live.body.bytes(), b"recreated");
        assert_eq!(live.revision.counter, tomb_rev.counter + 1);
    }
}
