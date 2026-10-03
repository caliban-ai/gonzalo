//! Restore a record from its tombstone, while the tombstone still exists
//! (ADR 0029).
//!
//! Only the kinds whose body a tombstone retains can be restored — the manifest
//! kinds. Every other tombstone has an empty body, so there is nothing to
//! restore from.
//!
//! This rides [`Store::put_raw`] rather than `put`. A consumer `put` treats a
//! tombstone as absent and re-stamps `meta.created`, which is right for `put` —
//! it cannot tell restoring a deleted record from reusing a recycled key, and
//! must not change meaning depending on whether `collect` has run. `put_raw`
//! never re-stamps, so an explicit restore keeps the record's age. "Raw" means a
//! record that already happened, and a restoration is exactly that.

use crate::{
    BlobStore, ContentHash, CoreError, Identity, Manifest, PutResult, Record, RecordKey,
    RecordKind, Result, Revision, Store, VectorManifest,
};

/// Restore the record at `key` from its tombstone, returning the revision
/// written. `author`, when given, is stamped as the restorer; `None` keeps the
/// tombstone's author.
///
/// The body, kind and `meta` come back; `links` do not, because a tombstone
/// never kept them.
///
/// Refuses, without writing anything, when there is no tombstone, the record is
/// live, the tombstone's body was not retained, a blob the body names is
/// missing, or the key changed while this ran.
pub async fn undelete<S>(
    store: &S,
    key: &RecordKey,
    now_ms: i64,
    author: Option<&Identity>,
) -> Result<Revision>
where
    S: Store + BlobStore + ?Sized,
{
    let Some(tomb) = store.get_raw(key).await? else {
        return Err(CoreError::NotFound(key.clone()));
    };
    if !tomb.is_tombstone() {
        return Err(CoreError::Invalid(format!(
            "{key} is live at revision {}; there is nothing to restore",
            tomb.revision.counter
        )));
    }
    let Some(kind) = tomb.deleted_kind else {
        return Err(CoreError::Invalid(format!(
            "{key}: the deleted body was not retained, so it cannot be restored. \
             Only manifest kinds retain a body (ADR 0029)."
        )));
    };

    // Check the blobs before writing: a record for an index that cannot be
    // opened is worse than a refusal.
    let mut missing = Vec::new();
    for hash in referenced_blobs(kind, &tomb)? {
        if !store.has_blob(&hash).await? {
            missing.push(hash.0);
        }
    }
    if !missing.is_empty() {
        return Err(CoreError::Invalid(format!(
            "{key}: cannot restore, {} blob(s) the retained body names are gone: {}",
            missing.len(),
            missing.join(", ")
        )));
    }

    let mut meta = tomb.meta.clone();
    if let Some(author) = author {
        meta.author = author.clone();
    }
    meta.updated = now_ms;

    let restored = Record {
        key: key.clone(),
        kind,
        revision: Revision {
            counter: tomb.revision.counter + 1,
            hash: ContentHash::of(tomb.body.bytes()),
        },
        parent: Some(tomb.revision.clone()),
        body: tomb.body.clone(),
        meta,
        links: Vec::new(),
        // `plan_put_raw` folds the tombstone in.
        ancestors: Vec::new(),
        deleted_at: None,
        deleted_blob: None,
        deleted_kind: None,
    };

    match store.put_raw(restored, Some(tomb.revision.clone())).await? {
        PutResult::Committed(rev) => Ok(rev),
        PutResult::Conflict(c) => Err(CoreError::Invalid(format!(
            "{key}: the key changed while restoring (now revision {}); nothing was written",
            c.current.revision.counter
        ))),
    }
}

/// The blob hashes a retained body names.
fn referenced_blobs(kind: RecordKind, tomb: &Record) -> Result<Vec<ContentHash>> {
    Ok(match kind {
        RecordKind::VectorManifest => VectorManifest::from_body(&tomb.body)?
            .entries
            .into_values()
            .collect(),
        RecordKind::GraphManifest => Manifest::from_body(&tomb.body)?
            .entries
            .into_values()
            .collect(),
        // `deleted_kind` is only ever set for retaining kinds, so this is
        // unreachable through `tombstone_of`; a hand-written record could still
        // get here, and it names no blobs.
        _ => Vec::new(),
    })
}
