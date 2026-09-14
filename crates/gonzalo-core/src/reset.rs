//! Namespace / collection reset (spec §3.7, gonzalo#203).
//!
//! Reset tombstones every live record under a prefix by issuing ordinary
//! OCC-guarded `delete` calls, so it replicates like any other delete and
//! needs only `write` on the namespace. It is **not atomic**: no substrate
//! offers multi-key transactions. It is **idempotent** instead. A second run
//! tombstones whatever the first run missed (a key that lost a race) and
//! skips everything already gone.

use crate::{CoreError, DeleteResult, Identity, KeyPrefix, RecordKey, Result, Store};

/// What a reset run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "a ResetReport may list conflicted keys that were not deleted"]
pub struct ResetReport {
    /// Keys tombstoned by this run, including any found already deleted at
    /// delete time, in `list` order.
    pub deleted: Vec<RecordKey>,
    /// Keys edited between this run's `get` and its `delete`. Left live, not
    /// retried. Run reset again to delete them.
    pub conflicts: Vec<RecordKey>,
}

/// Tombstone every live record under `prefix`. `prefix.namespace` is required.
/// Resetting every namespace in a store is not a reset, so a caller who wants
/// that must loop over namespaces explicitly.
///
/// For each live key (consumer `list`), reads its current revision and issues
/// `delete(key, Some(revision))`. A key that disappears between `list` and
/// `get` is skipped. A `Conflict` goes into [`ResetReport::conflicts`].
///
/// Stops at the first store error and returns it; keys already tombstoned
/// stay tombstoned, so a re-run is safe. Against a daemon whose backing store
/// predates tombstones, deletes are physical and do not replicate (spec
/// §8.1).
pub async fn reset(store: &dyn Store, prefix: &KeyPrefix) -> Result<ResetReport> {
    reset_as(store, prefix, None).await
}

/// As [`reset`], stamping `author` as each tombstone's deleter
/// (`Store::delete_as`). `None` keeps each record's own author, exactly as
/// [`reset`] does. Over a daemon the deleter follows spec §3.6: a non-admin
/// token is stamped as itself.
///
/// Stops at the first store error and returns it; keys already tombstoned
/// stay tombstoned, so a re-run is safe. Against a daemon whose backing store
/// predates tombstones, deletes are physical and do not replicate (spec
/// §8.1).
pub async fn reset_as(
    store: &dyn Store,
    prefix: &KeyPrefix,
    author: Option<Identity>,
) -> Result<ResetReport> {
    if prefix.namespace.is_none() {
        return Err(CoreError::Backend("reset requires a namespace".into()));
    }
    let mut report = ResetReport::default();
    for key in store.list(prefix).await? {
        // Belt-and-braces: reset is destructive, so never act on a key a
        // misbehaving store returned from outside the prefix.
        if !prefix.matches(&key) {
            continue;
        }
        let Some(current) = store.get(&key).await? else {
            continue;
        };
        match store
            .delete_as(&key, Some(current.revision), author.clone())
            .await?
        {
            DeleteResult::Deleted => report.deleted.push(key),
            DeleteResult::Conflict(_) => report.conflicts.push(key),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memstore::MemStore;
    use crate::test_support::{Hook, HookedStore, rec, seed};
    use std::sync::atomic::AtomicBool;

    const CLOCK: i64 = 1_000;

    fn ns(namespace: &str) -> KeyPrefix {
        KeyPrefix {
            namespace: Some(namespace.into()),
            collection: None,
        }
    }

    #[tokio::test]
    async fn refuses_a_prefix_without_a_namespace() {
        let store = MemStore::new().with_clock(CLOCK);
        seed(&store, rec("ns", "col", "a", "x")).await;

        let err = reset(
            &store,
            &KeyPrefix {
                namespace: None,
                collection: Some("col".into()),
            },
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&err, CoreError::Backend(m) if m == "reset requires a namespace"),
            "got {err:?}"
        );
        // Nothing was touched.
        assert!(store.raw_snapshot().values().all(|r| !r.is_tombstone()));
    }

    #[tokio::test]
    async fn tombstones_every_live_record_in_the_namespace_only() {
        let store = MemStore::new().with_clock(CLOCK);
        seed(&store, rec("ns", "a", "1", "x")).await;
        seed(&store, rec("ns", "b", "2", "y")).await;
        seed(&store, rec("other", "a", "3", "z")).await;

        let report = reset(&store, &ns("ns")).await.unwrap();

        assert_eq!(
            report.deleted,
            vec![
                RecordKey::new("ns", "a", "1"),
                RecordKey::new("ns", "b", "2")
            ]
        );
        assert!(report.conflicts.is_empty());
        for key in &report.deleted {
            assert_eq!(store.get(key).await.unwrap(), None, "{key} hidden");
            let raw = store.get_raw(key).await.unwrap().expect("tombstone kept");
            assert!(raw.is_tombstone());
            assert_eq!(raw.deleted_at, Some(CLOCK));
        }
        let other = RecordKey::new("other", "a", "3");
        assert!(
            store.get(&other).await.unwrap().is_some(),
            "sibling namespace untouched"
        );
    }

    #[tokio::test]
    async fn collection_scope_leaves_sibling_collections_alone() {
        let store = MemStore::new().with_clock(CLOCK);
        seed(&store, rec("ns", "col", "1", "x")).await;
        seed(&store, rec("ns", "sibling", "2", "y")).await;

        let report = reset(
            &store,
            &KeyPrefix {
                namespace: Some("ns".into()),
                collection: Some("col".into()),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.deleted, vec![RecordKey::new("ns", "col", "1")]);
        assert!(report.conflicts.is_empty());
        assert!(
            store
                .get(&RecordKey::new("ns", "sibling", "2"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn already_deleted_keys_are_skipped_and_their_chain_does_not_advance() {
        let store = MemStore::new().with_clock(CLOCK);
        let gone = RecordKey::new("ns", "col", "gone");
        seed(&store, rec("ns", "col", "gone", "x")).await;
        seed(&store, rec("ns", "col", "live", "y")).await;
        assert_eq!(
            store.delete(&gone, None).await.unwrap(),
            DeleteResult::Deleted
        );
        let before = store.get_raw(&gone).await.unwrap().unwrap().revision;

        let report = reset(&store, &ns("ns")).await.unwrap();

        assert_eq!(report.deleted, vec![RecordKey::new("ns", "col", "live")]);
        assert_eq!(
            store.get_raw(&gone).await.unwrap().unwrap().revision,
            before
        );
    }

    #[tokio::test]
    async fn concurrent_edit_is_a_conflict_and_rerunning_is_idempotent() {
        let raced = RecordKey::new("ns", "col", "b");
        let store = HookedStore {
            inner: MemStore::new().with_clock(CLOCK),
            hook: Hook::EditBeforeDelete(raced.clone()),
            fired: AtomicBool::new(false),
        };
        for id in ["a", "b", "c"] {
            seed(&store, rec("ns", "col", id, id)).await;
        }

        // Run 1: the edit wins the race on `b`; reset reports it, doesn't retry.
        let first = reset(&store, &ns("ns")).await.unwrap();
        assert_eq!(
            first.deleted,
            vec![
                RecordKey::new("ns", "col", "a"),
                RecordKey::new("ns", "col", "c")
            ]
        );
        assert_eq!(first.conflicts, vec![raced.clone()]);
        let survivor = store.get(&raced).await.unwrap().expect("edit survives");
        assert_eq!(survivor.body.bytes(), b"edited concurrently");

        // Run 2: tombstones exactly what run 1 missed, and conflicts on nothing.
        let second = reset(&store, &ns("ns")).await.unwrap();
        assert_eq!(second.deleted, vec![raced.clone()]);
        assert!(second.conflicts.is_empty());
        assert_eq!(store.get(&raced).await.unwrap(), None);

        // Run 3: nothing left to do.
        let third = reset(&store, &ns("ns")).await.unwrap();
        assert_eq!(third, ResetReport::default());
    }

    #[tokio::test]
    async fn reset_as_stamps_the_deleter() {
        let store = MemStore::new().with_clock(CLOCK);
        let key = RecordKey::new("ns", "col", "a");
        seed(&store, rec("ns", "col", "a", "x")).await;

        let report = reset_as(&store, &ns("ns"), Some(Identity::new("x")))
            .await
            .unwrap();

        assert_eq!(report.deleted, vec![key.clone()]);
        let raw = store.get_raw(&key).await.unwrap().expect("tombstone kept");
        assert!(raw.is_tombstone());
        assert_eq!(raw.meta.author, Identity::new("x"));
    }

    #[tokio::test]
    async fn a_key_that_vanishes_between_list_and_get_is_skipped() {
        let vanished = RecordKey::new("ns", "col", "b");
        let store = HookedStore {
            inner: MemStore::new().with_clock(CLOCK),
            hook: Hook::VanishBeforeGet(vanished),
            fired: AtomicBool::new(false),
        };
        seed(&store, rec("ns", "col", "a", "x")).await;
        seed(&store, rec("ns", "col", "b", "y")).await;

        let report = reset(&store, &ns("ns")).await.unwrap();

        assert_eq!(report.deleted, vec![RecordKey::new("ns", "col", "a")]);
        assert!(report.conflicts.is_empty());
    }
}
