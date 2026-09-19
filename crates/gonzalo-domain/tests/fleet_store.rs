//! Fleet records against a real store: OCC redemption, sync fast-forward of a
//! redeemed token, sync conflict on independent redemptions, and write-once
//! audit entries and bindings (ADR 0022).

use gonzalo_core::{Body, Identity, Meta, PutResult, Record, RecordKey, RecordKind, Store, sync};
use gonzalo_domain::RecordCodec;
use gonzalo_domain::fleet::{
    AuditEntry, AuditResult, Authenticator, BindingOrigin, FleetActor, FleetRole, GrantScope,
    IdentityBinding, LinkSecret, LinkToken,
};
use gonzalo_store_fs::FsStore;

const NOW: i64 = 1_700_000_000_000;

fn meta() -> Meta {
    Meta::new(Identity::new("fleet-test"), "fleet-test")
}

fn record(key: RecordKey, kind: RecordKind, body: Body) -> Record {
    Record::create(key, kind, body, meta())
}

fn secret() -> LinkSecret {
    LinkSecret::from_bytes([7; 32])
}

fn token() -> LinkToken {
    LinkToken::new(
        &secret(),
        FleetRole::Operator,
        GrantScope::Fleet,
        None,
        FleetActor::Service("ariel-cli".into()),
        NOW,
        NOW + 600_000,
    )
}

fn binding_key() -> RecordKey {
    IdentityBinding::key_for(&Authenticator::Discord, "1234").unwrap()
}

async fn put_unredeemed(store: &dyn Store) {
    let t = token();
    let result = store
        .put(record(t.key(), LinkToken::KIND, t.to_body().unwrap()), None)
        .await
        .unwrap();
    assert!(matches!(result, PutResult::Committed(_)));
}

/// The stored token, redeemed by `person` at `at`, as a record descending from it.
fn redeemed(stored: &Record, person: &str, at: i64) -> Record {
    let t = LinkToken::from_body(&stored.body)
        .unwrap()
        .redeem(&secret(), person, binding_key(), at)
        .unwrap();
    stored.update(t.to_body().unwrap(), meta())
}

async fn consumed_by(store: &dyn Store) -> Option<String> {
    let stored = store.get(&token().key()).await.unwrap().unwrap();
    LinkToken::from_body(&stored.body)
        .unwrap()
        .consumed
        .map(|c| c.person)
}

#[tokio::test]
async fn concurrent_redemptions_commit_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    put_unredeemed(&store).await;
    let stored = store.get(&token().key()).await.unwrap().unwrap();
    let expected = Some(stored.revision.clone());

    let (first, second) = tokio::join!(
        store.put(redeemed(&stored, "p1", NOW + 1), expected.clone()),
        store.put(redeemed(&stored, "p2", NOW + 2), expected),
    );
    let results = [first.unwrap(), second.unwrap()];
    let committed = results
        .iter()
        .filter(|r| matches!(r, PutResult::Committed(_)))
        .count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, PutResult::Conflict(_)))
        .count();
    assert_eq!((committed, conflicts), (1, 1));

    let winner = consumed_by(&store).await;
    assert!(
        winner.as_deref() == Some("p1") || winner.as_deref() == Some("p2"),
        "exactly one redemption is stored, got {winner:?}"
    );
}

#[tokio::test]
async fn redeemed_token_stays_redeemed_through_sync() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (FsStore::new(dir_a.path()), FsStore::new(dir_b.path()));
    put_unredeemed(&a).await;
    let report = sync(&a, &b).await.unwrap();
    assert!(report.conflicts.is_empty());
    assert_eq!(consumed_by(&b).await, None, "B holds the unredeemed token");

    let stored = a.get(&token().key()).await.unwrap().unwrap();
    let result = a
        .put(
            redeemed(&stored, "p1", NOW + 1),
            Some(stored.revision.clone()),
        )
        .await
        .unwrap();
    assert!(matches!(result, PutResult::Committed(_)));

    let report = sync(&a, &b).await.unwrap();
    assert!(
        report.conflicts.is_empty(),
        "a redemption fast-forwards the unredeemed peer: {report:?}"
    );
    assert_eq!(consumed_by(&a).await.as_deref(), Some("p1"));
    assert_eq!(consumed_by(&b).await.as_deref(), Some("p1"));
}

#[tokio::test]
async fn independent_redemptions_surface_a_sync_conflict() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (FsStore::new(dir_a.path()), FsStore::new(dir_b.path()));
    put_unredeemed(&a).await;
    let report = sync(&a, &b).await.unwrap();
    assert!(report.conflicts.is_empty());

    for (store, person, at) in [(&a, "p1", NOW + 1), (&b, "p2", NOW + 2)] {
        let stored = store.get(&token().key()).await.unwrap().unwrap();
        let result = store
            .put(redeemed(&stored, person, at), Some(stored.revision.clone()))
            .await
            .unwrap();
        assert!(matches!(result, PutResult::Committed(_)));
    }

    let report = sync(&a, &b).await.unwrap();
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    assert_eq!(report.conflicts[0].key, token().key());
    assert_eq!(consumed_by(&a).await.as_deref(), Some("p1"), "A unchanged");
    assert_eq!(consumed_by(&b).await.as_deref(), Some("p2"), "B unchanged");
}

#[tokio::test]
async fn audit_entries_are_write_once_per_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let entry = AuditEntry {
        actor: FleetActor::Person("p1".into()),
        action: "spawn".into(),
        target: "caliban-ai/gonzalo".into(),
        at: NOW,
        surface_ref: None,
        result: AuditResult::Succeeded,
    };
    let key = entry.key("n1").unwrap();
    let first = store
        .put(
            record(key.clone(), AuditEntry::KIND, entry.to_body().unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(first, PutResult::Committed(_)));

    let collision = AuditEntry {
        result: AuditResult::Denied,
        ..entry
    };
    let second = store
        .put(
            record(key, AuditEntry::KIND, collision.to_body().unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(second, PutResult::Conflict(_)),
        "a colliding audit key must conflict, never overwrite"
    );
}

#[tokio::test]
async fn second_bind_of_one_account_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let bind = |person: &str| IdentityBinding {
        authenticator: Authenticator::Discord,
        subject: "1234".into(),
        person: person.into(),
        handle: None,
        email: None,
        bound_at: NOW,
        bound_by: BindingOrigin::Operator(FleetActor::Service("ariel-cli".into())),
    };
    let key = binding_key();
    let first = store
        .put(
            record(
                key.clone(),
                IdentityBinding::KIND,
                bind("p1").to_body().unwrap(),
            ),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(first, PutResult::Committed(_)));
    let second = store
        .put(
            record(key, IdentityBinding::KIND, bind("p2").to_body().unwrap()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(second, PutResult::Conflict(_)));
}
