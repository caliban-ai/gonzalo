//! A reusable conformance suite every `Store` impl must pass. Substrate
//! crates call `run_store_conformance(factory)` from their integration
//! tests. The factory returns a fresh, empty store per invocation, built at
//! the default ancestor cap. `run_store_conformance` includes every tombstone
//! case (gonzalo#203) at that cap. Stores built with a smaller cap also call
//! `run_tombstone_conformance(factory, cap)` directly.

use crate::{
    BlobStore, Body, ContentHash, CoreError, DeleteResult, Identity, KeyPrefix, Meta, PutResult,
    Record, RecordKey, RecordKind, Revision, Store, tombstone_hash,
};
use std::collections::BTreeMap;

fn sample(key: RecordKey, payload: &[u8]) -> Record {
    let body = Body::Inline(payload.to_vec());
    Record {
        revision: Revision::initial(body.bytes()),
        parent: None,
        body,
        kind: RecordKind::Topic,
        meta: Meta {
            author: Identity::new("tester"),
            origin_system: "test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        key,
        ancestors: Vec::new(),
        deleted_at: None,
    }
}

/// Run the full suite against a store produced by `factory`, including the
/// tombstone cases at [`DEFAULT_ANCESTOR_CAP`](crate::DEFAULT_ANCESTOR_CAP).
/// `factory` must build fresh, empty stores at the default cap.
pub async fn run_store_conformance<S, F, Fut>(factory: F)
where
    S: Store,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = S>,
{
    get_absent_returns_none(&factory().await).await;
    put_then_get_roundtrips(&factory().await).await;
    stale_expected_returns_conflict(&factory().await).await;
    update_commits_then_stale_update_conflicts(&factory().await).await;
    list_filters_by_prefix(&factory().await).await;
    special_char_keys_dont_collide(&factory().await).await;
    delete_unconditional_removes(&factory().await).await;
    delete_absent_is_idempotent(&factory().await).await;
    delete_stale_expected_conflicts(&factory().await).await;
    delete_matching_expected_removes(&factory().await).await;

    // Every store gets the tombstone cases, so a new substrate cannot pass
    // conformance without replicated deletion (gonzalo#203).
    run_tombstone_conformance(&factory, crate::DEFAULT_ANCESTOR_CAP).await;
}

/// (a) A `put` then an unconditional `delete` (`expected = None`) removes the
/// record: the follow-up `get` returns `None`.
async fn delete_unconditional_removes<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "del", "unconditional");
    let rec = sample(key.clone(), b"bye");
    assert!(matches!(
        store.put(rec, None).await.unwrap(),
        PutResult::Committed(_)
    ));
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    assert_eq!(store.get(&key).await.unwrap(), None);
}

/// (b) Deleting an absent key with `expected = None` is an idempotent `Deleted`.
async fn delete_absent_is_idempotent<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "del", "absent");
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
}

/// (c) A conditional `delete` with the wrong expected revision is a `Conflict`
/// whose `current.revision` is the live revision, and the record still exists.
async fn delete_stale_expected_conflicts<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "del", "stale");
    let rec = sample(key.clone(), b"keep");
    let rev = match store.put(rec, None).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("unexpected conflict on create"),
    };
    let wrong = Revision::initial(b"a-revision-that-was-never-current");
    assert_ne!(wrong, rev);
    match store.delete(&key, Some(wrong)).await.unwrap() {
        DeleteResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, rev);
        }
        DeleteResult::Deleted => panic!("stale conditional delete must conflict"),
    }
    // The record survived the rejected delete.
    assert_eq!(store.get(&key).await.unwrap().unwrap().revision, rev);
}

/// (d) A conditional `delete` with the matching expected revision removes it.
async fn delete_matching_expected_removes<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "del", "matching");
    let rec = sample(key.clone(), b"gone");
    let rev = match store.put(rec, None).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("unexpected conflict on create"),
    };
    assert_eq!(
        store.delete(&key, Some(rev)).await.unwrap(),
        DeleteResult::Deleted
    );
    assert_eq!(store.get(&key).await.unwrap(), None);
}

/// Distinct keys that mapped to the *same* physical path under the old lossy
/// `_`-collapse (`.` and `/` both became `_`) must now be independent records:
/// no cross-key overwrite, no spurious OCC conflict, and `list()` must return
/// each original key verbatim (encode/decode round-trip).
async fn special_char_keys_dont_collide<S: Store>(store: &S) {
    let dotted = RecordKey::new("ns", "col", "v1.0");
    let under = RecordKey::new("ns", "col", "v1_0");
    let slashy = RecordKey::new("a/b", "c.d", "e/f");

    // Creating all three with `expected = None` must each Commit — under the
    // old collision, `under` would see `dotted` already present and Conflict.
    for (k, payload) in [
        (&dotted, b"dotted".as_slice()),
        (&under, b"under".as_slice()),
        (&slashy, b"slashy".as_slice()),
    ] {
        match store.put(sample(k.clone(), payload), None).await.unwrap() {
            PutResult::Committed(_) => {}
            PutResult::Conflict(_) => panic!("distinct key {k:?} collided onto an existing record"),
        }
    }

    // Each retrievable independently with its own body — no clobber.
    for (k, payload) in [
        (&dotted, b"dotted".as_slice()),
        (&under, b"under".as_slice()),
        (&slashy, b"slashy".as_slice()),
    ] {
        let got = store.get(k).await.unwrap().expect("record present");
        assert_eq!(got.body.bytes(), payload, "wrong body for {k:?}");
    }

    // `list()` round-trips the exact keys (decode is the inverse of encode).
    let keys = store.list(&KeyPrefix::default()).await.unwrap();
    for k in [&dotted, &under, &slashy] {
        assert!(keys.contains(k), "list() missing {k:?}; got {keys:?}");
    }
}

async fn get_absent_returns_none<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "missing");
    assert_eq!(store.get(&key).await.unwrap(), None);
}

async fn put_then_get_roundtrips<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "a");
    let rec = sample(key.clone(), b"hello");
    let PutResult::Committed(committed_rev) = store.put(rec.clone(), None).await.unwrap() else {
        panic!("expected Committed");
    };
    assert_eq!(committed_rev, rec.revision);
    assert_eq!(store.get(&key).await.unwrap(), Some(rec));
}

async fn stale_expected_returns_conflict<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "b");
    let first = sample(key.clone(), b"v1");
    let committed = match store.put(first.clone(), None).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("unexpected conflict on create"),
    };

    // A second writer who never saw `committed` tries to create again.
    let stale = sample(key.clone(), b"v2-from-stale-writer");
    match store.put(stale, None).await.unwrap() {
        PutResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, committed);
        }
        PutResult::Committed(_) => panic!("expected conflict for stale write"),
    }
}

async fn update_commits_then_stale_update_conflicts<S: Store>(store: &S) {
    let key = RecordKey::new("ns", "col", "upd");

    // Create v1.
    let v1 = sample(key.clone(), b"v1");
    let rev1 = match store.put(v1, None).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("unexpected conflict on create"),
    };

    // Update with the correct `expected` revision commits.
    let mut v2 = sample(key.clone(), b"v2");
    v2.parent = Some(rev1.clone());
    v2.revision = rev1.next(b"v2");
    let rev2 = match store.put(v2, Some(rev1.clone())).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(_) => panic!("update with correct expected must commit"),
    };
    assert_ne!(rev2, rev1, "an update produces a new revision");

    // A writer who still holds `rev1` tries to update again: conflict against
    // the now-current `rev2`. On a store with native conditional writes this is
    // enforced atomically at the object level (`If-Match`), not just by the
    // pre-read — closing the read-then-write TOCTOU.
    let mut stale = sample(key.clone(), b"v3-from-stale-writer");
    stale.parent = Some(rev1.clone());
    stale.revision = rev1.next(b"v3");
    match store.put(stale, Some(rev1)).await.unwrap() {
        PutResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, rev2);
        }
        PutResult::Committed(_) => panic!("stale update must conflict"),
    }

    // The winning value is still readable and is v2.
    assert_eq!(store.get(&key).await.unwrap().unwrap().revision, rev2);
}

/// Tombstone, recreation and purge semantics every `Store` must share
/// (ADR 0021, spec §6.1). `factory` must build fresh, empty stores whose
/// ancestor cap is `cap`.
pub async fn run_tombstone_conformance<S, F, Fut>(factory: F, cap: usize)
where
    S: Store,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = S>,
{
    delete_hides_from_get_and_list(&factory().await).await;
    delete_visible_to_raw_reads(&factory().await).await;
    delete_stale_expected_writes_no_tombstone(&factory().await).await;
    delete_of_absent_key_writes_nothing(&factory().await).await;
    delete_of_tombstone_is_noop(&factory().await).await;
    independent_deletes_are_identical(&factory().await, &factory().await).await;
    tombstone_never_collides_with_empty_body(&factory().await).await;
    recreate_continues_chain(&factory().await).await;
    put_some_over_tombstone_is_not_found(&factory().await).await;
    consumer_put_of_a_tombstone_is_rejected(&factory().await).await;
    replication_overwrite_of_tombstone(&factory().await).await;
    put_raw_create_over_tombstone_conflicts(&factory().await).await;
    put_raw_never_restamps(&factory().await).await;
    delete_as_stamps_author(&factory().await).await;
    delete_keeps_the_prior_author(&factory().await).await;
    purge_removes_physically(&factory().await).await;
    purge_absent_is_noop(&factory().await).await;
    purge_conflicts_after_recreation(&factory().await).await;
    ancestors_capped_and_ordered(&factory().await, cap).await;
    put_raw_truncates_ancestors_and_excludes_own_revision(&factory().await, cap).await;
}

fn tomb_key(id: &str) -> RecordKey {
    RecordKey::new("ns", "tomb", id)
}

fn tomb_prefix() -> KeyPrefix {
    KeyPrefix {
        namespace: Some("ns".into()),
        collection: Some("tomb".into()),
    }
}

async fn committed<S: Store>(store: &S, rec: Record, expected: Option<Revision>) -> Revision {
    match store.put(rec, expected).await.unwrap() {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    }
}

async fn put_then_delete<S: Store>(
    store: &S,
    key: &RecordKey,
    payload: &[u8],
) -> (Revision, Record) {
    let rev = committed(store, sample(key.clone(), payload), None).await;
    assert_eq!(
        store.delete(key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    let tomb = store
        .get_raw(key)
        .await
        .unwrap()
        .expect("tombstone visible to get_raw");
    (rev, tomb)
}

/// A deleted key is absent to consumers; its sibling is unaffected.
async fn delete_hides_from_get_and_list<S: Store>(store: &S) {
    let gone = tomb_key("gone");
    let kept = tomb_key("kept");
    committed(store, sample(kept.clone(), b"stay"), None).await;
    let _ = put_then_delete(store, &gone, b"bye").await;
    assert_eq!(store.get(&gone).await.unwrap(), None);
    assert_eq!(store.list(&tomb_prefix()).await.unwrap(), vec![kept]);
}

/// Raw reads see the tombstone, shaped exactly per spec §3.1.
async fn delete_visible_to_raw_reads<S: Store>(store: &S) {
    let key = tomb_key("raw");
    let (rev, t) = put_then_delete(store, &key, b"bye").await;
    assert!(t.is_tombstone());
    assert_eq!(t.kind, RecordKind::Tombstone);
    assert_eq!(t.body, Body::Inline(Vec::new()));
    assert!(t.deleted_at.is_some(), "tombstone carries deleted_at");
    assert_eq!(t.revision.counter, rev.counter + 1);
    assert_eq!(t.revision.hash, tombstone_hash());
    assert_eq!(t.parent, Some(rev.clone()));
    assert_eq!(t.ancestors.first(), Some(&rev));
    assert!(store.list_raw(&tomb_prefix()).await.unwrap().contains(&key));
}

/// A stale conditional delete conflicts and leaves the live record in place.
async fn delete_stale_expected_writes_no_tombstone<S: Store>(store: &S) {
    let key = tomb_key("stale");
    let rev = committed(store, sample(key.clone(), b"keep"), None).await;
    let wrong = Revision::initial(b"a-revision-that-was-never-current");
    match store.delete(&key, Some(wrong)).await.unwrap() {
        DeleteResult::Conflict(c) => {
            assert_eq!(c.key, key);
            assert_eq!(c.current.revision, rev);
            assert!(!c.current.is_tombstone());
        }
        DeleteResult::Deleted => panic!("a stale conditional delete must conflict"),
    }
    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert!(!raw.is_tombstone());
    assert_eq!(raw.revision, rev);
}

/// Deleting a key the store never held writes nothing.
async fn delete_of_absent_key_writes_nothing<S: Store>(store: &S) {
    let key = tomb_key("never");
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
}

/// Deleting a tombstone does not advance its chain.
async fn delete_of_tombstone_is_noop<S: Store>(store: &S) {
    let key = tomb_key("twice");
    let (_, first) = put_then_delete(store, &key, b"bye").await;
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    let second = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(second.revision, first.revision);
}

/// Two stores deleting the same revision independently agree on the result.
async fn independent_deletes_are_identical<S: Store>(a: &S, b: &S) {
    let key = tomb_key("same");
    let (_, ta) = put_then_delete(a, &key, b"shared").await;
    let (_, tb) = put_then_delete(b, &key, b"shared").await;
    assert_eq!(ta.revision, tb.revision);
}

/// A tombstone's revision never equals an empty-body edit at the same counter.
async fn tombstone_never_collides_with_empty_body<S: Store>(store: &S) {
    let key = tomb_key("empty");
    let (rev, t) = put_then_delete(store, &key, b"x").await;
    let empty_edit = rev.next(b"");
    assert_eq!(t.revision.counter, empty_edit.counter);
    assert_ne!(t.revision, empty_edit);
}

/// A create over a tombstone continues the revision chain.
async fn recreate_continues_chain<S: Store>(store: &S) {
    let key = tomb_key("recreate");
    let (_, t) = put_then_delete(store, &key, b"v0").await;
    let r = committed(store, sample(key.clone(), b"v1"), None).await;
    assert_eq!(r.counter, t.revision.counter + 1);
    assert_eq!(r.hash, ContentHash::of(b"v1"));
    let live = store
        .get(&key)
        .await
        .unwrap()
        .expect("recreated record is visible");
    assert_eq!(live.revision, r);
    assert_eq!(live.parent, Some(t.revision.clone()));
    assert_eq!(live.deleted_at, None);
    assert_eq!(live.ancestors.first(), Some(&t.revision));
}

/// A tombstone is absent to a conditional put naming any other revision.
async fn put_some_over_tombstone_is_not_found<S: Store>(store: &S) {
    let key = tomb_key("late");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    let with_tomb_rev = store
        .put(sample(key.clone(), b"late"), Some(t.revision.clone()))
        .await;
    assert!(
        matches!(with_tomb_rev, Err(CoreError::NotFound(_))),
        "consumer put naming the tombstone's revision must be NotFound, got {with_tomb_rev:?}"
    );
    let out = store
        .put(
            sample(key.clone(), b"late"),
            Some(Revision::initial(b"a-revision-that-was-never-current")),
        )
        .await;
    assert!(matches!(out, Err(CoreError::NotFound(_))), "got {out:?}");
}

/// Consumer `put` of a `RecordKind::Tombstone` record is rejected with an
/// error on every `current` state that a consumer put can reach: absent, and
/// a live key naming the correct `expected`. Nothing is written either way.
async fn consumer_put_of_a_tombstone_is_rejected<S: Store>(store: &S) {
    let absent = tomb_key("reject-absent");
    let mut t = sample(absent.clone(), b"");
    t.kind = RecordKind::Tombstone;
    t.deleted_at = Some(1);
    let out = store.put(t, None).await;
    assert!(
        out.is_err(),
        "consumer put of a tombstone-kind record over an absent key must error, got {out:?}"
    );
    assert_eq!(store.get_raw(&absent).await.unwrap(), None);

    let key = tomb_key("reject-live");
    let rev = committed(store, sample(key.clone(), b"live"), None).await;
    let mut over_live = sample(key.clone(), b"");
    over_live.kind = RecordKind::Tombstone;
    over_live.deleted_at = Some(1);
    over_live.revision = rev.next(b"");
    let out = store.put(over_live, Some(rev.clone())).await;
    assert!(
        out.is_err(),
        "consumer put of a tombstone-kind record over a live record must error, got {out:?}"
    );
    let raw = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(raw.revision, rev);
    assert!(!raw.is_tombstone());
}

/// Replication naming the tombstone's revision overwrites it unchanged.
async fn replication_overwrite_of_tombstone<S: Store>(store: &S) {
    let key = tomb_key("replicated");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    let mut incoming = sample(key.clone(), b"from-peer");
    incoming.revision = Revision {
        counter: t.revision.counter + 5,
        hash: ContentHash::of(b"from-peer"),
    };
    incoming.parent = Some(t.revision.clone());
    incoming.ancestors = vec![t.revision.clone()];
    let r = match store
        .put_raw(incoming.clone(), Some(t.revision.clone()))
        .await
        .unwrap()
    {
        PutResult::Committed(rev) => rev,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    };
    assert_eq!(r, incoming.revision);
    let got = store.get(&key).await.unwrap().unwrap();
    assert_eq!(got.revision, incoming.revision);
    assert_eq!(got.body, incoming.body);
    assert_eq!(got.ancestors.first(), Some(&t.revision));
}

/// A replication create over a tombstone conflicts carrying it; nothing is written.
async fn put_raw_create_over_tombstone_conflicts<S: Store>(store: &S) {
    let key = tomb_key("raw-create");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    match store
        .put_raw(sample(key.clone(), b"copy"), None)
        .await
        .unwrap()
    {
        PutResult::Conflict(c) => {
            assert!(c.current.is_tombstone());
            assert_eq!(c.current.revision, t.revision);
        }
        PutResult::Committed(rev) => {
            panic!("put_raw must never recreate over a tombstone, committed {rev:?}")
        }
    }
    assert_eq!(
        store.get_raw(&key).await.unwrap().unwrap().revision,
        t.revision
    );
    assert_eq!(store.get(&key).await.unwrap(), None);
}

/// `put_raw` stores the caller's revision verbatim, even a non-sequential one.
async fn put_raw_never_restamps<S: Store>(store: &S) {
    let key = tomb_key("verbatim");
    let rev = committed(store, sample(key.clone(), b"v0"), None).await;
    let mut incoming = sample(key.clone(), b"peer");
    incoming.revision = Revision {
        counter: rev.counter + 9,
        hash: ContentHash::of(b"peer"),
    };
    incoming.parent = Some(rev.clone());
    let stored = match store
        .put_raw(incoming.clone(), Some(rev.clone()))
        .await
        .unwrap()
    {
        PutResult::Committed(r) => r,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    };
    assert_eq!(stored, incoming.revision);
    let got = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(got.revision, incoming.revision);
    assert_eq!(got.ancestors.first(), Some(&rev));
}

/// `delete_as` records the deleter on the tombstone.
async fn delete_as_stamps_author<S: Store>(store: &S) {
    let key = tomb_key("author");
    committed(store, sample(key.clone(), b"mine"), None).await;
    let deleter = Identity::new("deleter");
    assert_eq!(
        store
            .delete_as(&key, None, Some(deleter.clone()))
            .await
            .unwrap(),
        DeleteResult::Deleted
    );
    let t = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(t.meta.author, deleter);
}

/// A plain `delete` (no author) keeps the live record's own author on the
/// tombstone: `sample()` writes every live record as author `"tester"`.
async fn delete_keeps_the_prior_author<S: Store>(store: &S) {
    let key = tomb_key("prior-author");
    committed(store, sample(key.clone(), b"mine"), None).await;
    assert_eq!(
        store.delete(&key, None).await.unwrap(),
        DeleteResult::Deleted
    );
    let t = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(t.meta.author, Identity::new("tester"));
}

/// Purge physically removes the tombstone.
async fn purge_removes_physically<S: Store>(store: &S) {
    let key = tomb_key("purged");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    assert_eq!(
        store.purge(&key, t.revision).await.unwrap(),
        DeleteResult::Deleted
    );
    assert_eq!(store.get_raw(&key).await.unwrap(), None);
    assert!(!store.list_raw(&tomb_prefix()).await.unwrap().contains(&key));
}

/// Purging an absent key is an idempotent no-op.
async fn purge_absent_is_noop<S: Store>(store: &S) {
    let key = tomb_key("purge-absent");
    assert_eq!(
        store.purge(&key, Revision::initial(b"x")).await.unwrap(),
        DeleteResult::Deleted
    );
}

/// Purge naming a tombstone that was since recreated conflicts; the record survives.
async fn purge_conflicts_after_recreation<S: Store>(store: &S) {
    let key = tomb_key("purge-race");
    let (_, t) = put_then_delete(store, &key, b"gone").await;
    let r = committed(store, sample(key.clone(), b"back"), None).await;
    match store.purge(&key, t.revision).await.unwrap() {
        DeleteResult::Conflict(c) => assert_eq!(c.current.revision, r),
        DeleteResult::Deleted => panic!("purge must not remove a recreated record"),
    }
    assert_eq!(store.get(&key).await.unwrap().unwrap().revision, r);
}

/// After more updates than the cap, ancestors hold exactly the newest `cap`.
async fn ancestors_capped_and_ordered<S: Store>(store: &S, cap: usize) {
    let key = tomb_key("cap");
    let mut rec = sample(key.clone(), b"0");
    let mut rev = committed(store, rec.clone(), None).await;
    let mut history = vec![rev.clone()];
    for i in 1..=(cap + 5) {
        let body = i.to_string().into_bytes();
        rec.body = Body::Inline(body.clone());
        rec.parent = Some(rev.clone());
        rec.revision = rev.next(&body);
        rev = committed(store, rec.clone(), Some(rev.clone())).await;
        history.push(rev.clone());
    }
    let stored = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(stored.revision, rev);
    let expected: Vec<Revision> = history.iter().rev().skip(1).take(cap).cloned().collect();
    assert_eq!(stored.ancestors, expected);
}

/// `put_raw` folds an oversized, self-including `incoming.ancestors` the same
/// way every other write does: truncated to `cap`, newest first, and never
/// containing the record's own stored revision.
async fn put_raw_truncates_ancestors_and_excludes_own_revision<S: Store>(store: &S, cap: usize) {
    let key = tomb_key("raw-cap");
    let rev = committed(store, sample(key.clone(), b"0"), None).await;

    let mut incoming = sample(key.clone(), b"peer");
    incoming.revision = Revision {
        counter: rev.counter + 1,
        hash: ContentHash::of(b"peer"),
    };
    incoming.parent = Some(rev.clone());
    incoming.ancestors = (0..cap + 3)
        .map(|i| Revision {
            counter: i as u64,
            hash: ContentHash::of(format!("a{i}").as_bytes()),
        })
        .collect();
    // Including the incoming record's own revision must not survive the fold.
    incoming.ancestors.push(incoming.revision.clone());

    let stored = match store
        .put_raw(incoming.clone(), Some(rev.clone()))
        .await
        .unwrap()
    {
        PutResult::Committed(r) => r,
        PutResult::Conflict(c) => panic!("unexpected conflict: {c:?}"),
    };
    assert_eq!(stored, incoming.revision);

    let got = store.get_raw(&key).await.unwrap().unwrap();
    assert_eq!(got.ancestors.len(), cap);
    assert!(
        !got.ancestors.contains(&stored),
        "ancestors must not contain the record's own stored revision"
    );
    for pair in got.ancestors.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert!(
            a.counter > b.counter || (a.counter == b.counter && a.hash >= b.hash),
            "ancestors not newest-first: {a:?} before {b:?}"
        );
    }
}

/// Run the full blob-store suite against a store produced by `factory`
/// (a fresh, empty [`BlobStore`] per invocation).
pub async fn run_blob_store_conformance<B, F, Fut>(factory: F)
where
    B: BlobStore,
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = B>,
{
    blob_get_absent_returns_none(&factory().await).await;
    blob_put_then_get_roundtrips(&factory().await).await;
    blob_put_is_content_addressed_and_idempotent(&factory().await).await;
    blob_list_reports_stored_hashes(&factory().await).await;
    blob_delete_removes_and_is_idempotent(&factory().await).await;
}

async fn blob_get_absent_returns_none<B: BlobStore>(store: &B) {
    assert_eq!(
        store.get_blob(&ContentHash::of(b"absent")).await.unwrap(),
        None
    );
}

async fn blob_put_then_get_roundtrips<B: BlobStore>(store: &B) {
    let content = b"symbols + references for one file";
    let hash = store.put_blob(content).await.unwrap();
    assert_eq!(hash, ContentHash::of(content));
    assert_eq!(
        store.get_blob(&hash).await.unwrap().as_deref(),
        Some(&content[..])
    );
}

async fn blob_put_is_content_addressed_and_idempotent<B: BlobStore>(store: &B) {
    let content = b"deterministic slice body";
    // Storing the same content twice yields the same hash and never conflicts.
    let first = store.put_blob(content).await.unwrap();
    let second = store.put_blob(content).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first, ContentHash::of(content));
    // The content is still intact after the second (no-op) write.
    assert_eq!(
        store.get_blob(&first).await.unwrap().as_deref(),
        Some(&content[..])
    );
}

async fn blob_list_reports_stored_hashes<B: BlobStore>(store: &B) {
    assert!(store.list_blobs().await.unwrap().is_empty());
    let h1 = store.put_blob(b"slice one").await.unwrap();
    let h2 = store.put_blob(b"slice two").await.unwrap();
    let mut listed = store.list_blobs().await.unwrap();
    listed.sort();
    let mut want = vec![h1, h2];
    want.sort();
    assert_eq!(listed, want);
}

async fn blob_delete_removes_and_is_idempotent<B: BlobStore>(store: &B) {
    let hash = store.put_blob(b"to be collected").await.unwrap();
    assert!(store.get_blob(&hash).await.unwrap().is_some());
    store.delete_blob(&hash).await.unwrap();
    assert_eq!(store.get_blob(&hash).await.unwrap(), None);
    // Deleting an absent blob succeeds (idempotent).
    store.delete_blob(&hash).await.unwrap();
}

async fn list_filters_by_prefix<S: Store>(store: &S) {
    let r1 = store
        .put(sample(RecordKey::new("x", "c1", "1"), b"1"), None)
        .await
        .unwrap();
    assert!(matches!(r1, PutResult::Committed(_)));
    let r2 = store
        .put(sample(RecordKey::new("x", "c2", "2"), b"2"), None)
        .await
        .unwrap();
    assert!(matches!(r2, PutResult::Committed(_)));
    let prefix = KeyPrefix {
        namespace: Some("x".into()),
        collection: Some("c1".into()),
    };
    let mut keys = store.list(&prefix).await.unwrap();
    keys.sort();
    assert_eq!(keys, vec![RecordKey::new("x", "c1", "1")]);
}

#[cfg(test)]
mod self_test {
    use super::*;
    use crate::memstore::MemStore;

    #[tokio::test]
    async fn memstore_passes_store_conformance() {
        run_store_conformance(|| async { MemStore::new() }).await;
    }

    #[tokio::test]
    async fn memstore_passes_tombstone_conformance_small_cap() {
        run_tombstone_conformance(|| async { MemStore::new().with_ancestor_cap(3) }, 3).await;
    }
}
