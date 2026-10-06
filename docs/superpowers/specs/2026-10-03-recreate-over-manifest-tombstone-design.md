# Guarding a manifest tombstone against recreation — design

**Ticket:** gonzalo#333
**Status:** approved
**Date:** 2026-10-03

## Context

ADR 0029 made a deleted manifest recoverable. A tombstone for `GraphManifest` or
`VectorManifest` retains the deleted body and records `deleted_kind`, so blob GC marks
through it and the blobs the manifest names survive until `collect` removes the tombstone
past its horizon. `undelete` then puts the record back. For a vector index that window is
the only thing standing between an accidental `gonzalo delete` and permanent loss, because
shards are caller-supplied and cannot be regenerated.

**Any consumer `put` that creates over that tombstone destroys the window.** `plan_put`'s
recreation arm (`crates/gonzalo-core/src/tombstone.rs:193-218`) treats a tombstone as
absent — correctly, under ADR 0021, because consumers never see tombstones and a create
must be able to succeed past one. It overwrites the tombstone, clearing `deleted_at`,
`deleted_blob`, `deleted_kind` and the retained body, and re-stamping `meta.created`. The
blobs the discarded body named become unreferenced, so the next sweep frees them.

Two live paths reach it, and both reach it the same way — by deriving `expected` from a
**consumer** read, which hides the tombstone and yields `None`:

1. **`RecordVectorIndex`.** `open_with_shards` reads `store.get(&key)`
   (`crates/gonzalo-vector/src/record_index.rs:165`), so `existing` is `None` and
   `last_seen` is `None`. The first commit builds `Record::create(..)` with
   `expected: None` (`record_index.rs:417-431`) and lands on the recreation arm. This is
   the severe path: the most likely operator action after an accidental delete — restart
   the process, reopen the index — is exactly the action that makes `undelete` impossible.
2. **The graph indexer.** `gonzalo index` derives `expected` from a consumer `get`
   (`crates/gonzalo-cli/src/lib.rs:~609`) and does the same thing to a `GraphManifest`
   tombstone. Lower stakes, because graph slices regenerate from source, but the restore
   window is destroyed just as silently.

The vector commit path already *anticipated* this case for bookkeeping — a comment at
`record_index.rs:445` notes the store "can re-stamp it (for example, recreating a record
over a tombstone)" and correctly prefers the store's returned revision. The recreation was
foreseen as a revision concern and never as a data hazard.

### What is not the problem

`plan_put_raw` is not involved. It already Conflicts a create over a tombstone, precisely
so replication cannot resurrect a deleted record, and `undelete` rides its matching-revision
arm. Replication and restore are both unaffected by anything in this design.

### Why documentation was not enough

ADR 0029 documents the hazard and tells operators to run `undelete` before anything opens
the key. That is a real mitigation for an operator who knows. It is not a mitigation for a
process that restarts on its own, which is the common case, and it leaves a promise the
system does not keep: the blobs are pinned "until `collect`", except that an ordinary write
can unpin them at any moment.

## Decisions

1. **`plan_put` refuses a create over a tombstone that carries `deleted_kind`.** The guard
   lives at the core chokepoint every substrate routes through, so it covers both known
   paths and every future writer.
2. **The guard keys off the stored tombstone, never the incoming record.** It fires when a
   restore window exists, which is exactly when `deleted_kind.is_some()`.
3. **Both manifest kinds are guarded**, symmetric with ADR 0029, which retains both bodies
   for the same reason. Graph slices regenerate, but an asymmetric rule would have to be
   justified forever and would leave a graph manifest's window silently destroyable.
4. **A deliberate recreate goes through a new `gonzalo purge` subcommand**, which removes
   one tombstone explicitly. It is the precise inverse of the guard.
5. **`gonzalo purge` refuses anything that is not a tombstone.** `Store::purge` itself is
   unchanged.

### Rejected alternatives

- **Guard only in `RecordVectorIndex::open`.** Fixes the severe path at its source and
  leaves core semantics untouched, but does nothing for the graph indexer and protects no
  future consumer that writes to a manifest key. The hazard is a property of the consumer
  write path, not of one function.
- **Guard only `VectorManifest`.** Protects the unrecoverable case with zero friction for
  `gonzalo index`. Rejected per decision 3.
- **Auto-restore: let `open` silently `undelete`.** Makes the problem disappear with no
  refusal, and is wrong. It resurrects a record an operator deliberately deleted, turning
  `delete` into a suggestion.
- **A `--force` flag on each writer.** Most ergonomic for the rebuild flow, but it puts the
  opt-in far from the tombstone it destroys, duplicates purge-then-create in every writer,
  and does nothing for library consumers.
- **Change `plan_purge` to require a tombstone.** Would make the escape hatch safe in core
  rather than in the CLI, but `collect` depends on the current contract and `Store::purge`
  is documented as the mechanism collection uses. The check belongs at the operator surface.

## The guard

> **Note (as shipped).** This design sketches the trigger as
> `deleted_kind.is_some()`. The shipped guard is narrower: an explicit match on
> `GraphManifest | VectorManifest`. Do not implement `is_some()`; see
> [ADR 0030](../../adr/0030-manifest-tombstone-recreate-guard.md) for the
> narrowing and why.

A new arm in `plan_put`, ahead of the existing recreation arm:

```rust
Some(t) if t.is_tombstone() => match expected {
    // A manifest tombstone is a restore window: its retained body is the only
    // record of which blob was which shard, and GC marks through it (ADR 0029).
    // A create would overwrite the tombstone and discard that body, unpinning
    // blobs that may be unregenerable, so refuse and make the caller choose
    // between restoring and discarding (ADR 0030).
    None if t.deleted_kind.is_some() => {
        PutPlan::Rejected(MANIFEST_TOMBSTONE_RECREATE_REJECTED)
    }
    None => { /* the existing recreation arm, unchanged */ }
    // Consumers never learn a tombstone's revision, so any `Some` here is stale.
    Some(_) => PutPlan::NotFound,
},
```

with a new constant beside `CONSUMER_TOMBSTONE_REJECTED` and in its style:

```rust
/// The reason a consumer `put` that would create over a *manifest* tombstone is
/// rejected (see [`PutPlan::Rejected`]). The tombstone retains the deleted
/// manifest's body, which is the restore window ADR 0029 promises.
pub const MANIFEST_TOMBSTONE_RECREATE_REJECTED: &str =
    "a deleted manifest is at this key; restore it with `gonzalo undelete`, \
     or discard the tombstone with `gonzalo purge`";
```

**Why this needs no plumbing.** `PutPlan::Rejected(&'static str)` already exists, and every
substrate already maps it to `CoreError::Invalid(reason)` — `fs` at `lib.rs:437`, `s3` at
`lib.rs:650`, `git` at `lib.rs:125`, `memstore` at `memstore.rs:83`, and S3's second call
site at `lib.rs:1457`. `CoreError::Invalid` is the variant the daemon answers as `400` /
`InvalidArgument` rather than `500` (gonzalo#299). So the guard requires no new `PutPlan`
variant, no substrate change, no new route, and no `.proto` change.

**The message is a constant.** `Rejected` carries `&'static str`, so it cannot name the key.
This follows the existing precedent; the caller knows which key it wrote to, and the message
spends its budget on what to do next instead.

### Why existing recreation tests are unaffected

The guard reads the **stored** tombstone's `deleted_kind`. Every existing test that recreates
over a tombstone builds it with `tomb(counter)`
(`crates/gonzalo-core/src/tombstone.rs:740-743`), which is `tombstone_of` over a non-manifest
`live(..)` record, so `deleted_kind` is `None` and the guard does not fire. That includes
`recreating_over_a_tombstone_clears_deleted_kind` (`tombstone.rs:812-824`), whose point is
that an **incoming** live record cannot smuggle a `deleted_kind` onto itself — a different
field on a different record, so that test keeps passing and keeps proving what it claims.
The conformance case `recreate_continues_chain` likewise recreates over a `Topic` tombstone.

## The escape hatch

```
gonzalo purge --namespace <ns> --collection <col> --id <id> [--root <path>]
```

It reads the record raw, refuses unless it is a tombstone, and otherwise purges it at its
current revision:

```rust
pub async fn purge(root: &Path, namespace: &str, collection: &str, id: &str) -> Result<()> {
    let store = open_store(root, DEFAULT_ANCESTOR_CAP)?;
    let key = RecordKey::new(namespace, collection, id);
    let Some(record) = store.get_raw(&key).await? else {
        return Err(/* NotFound: nothing at this key */);
    };
    if !record.is_tombstone() {
        return Err(/* Invalid: refuses to physically remove a LIVE record */);
    }
    store.purge(&key, record.revision).await?;
    Ok(())
}
```

**The live-record refusal is the load-bearing part of this section.** `plan_purge`
(`crates/gonzalo-core/src/tombstone.rs:328-338`) does **not** check the record's kind — it
removes whatever matches `expected`, live or tombstone. `Store::purge`'s own doc calls it
"the only physical removal in the system: used by tombstone collection". Exposing it to
operators without the check would hand them a way to delete a live record leaving no
tombstone, which a peer that has not synced since would then resurrect — the exact failure
ADR 0021 designed tombstones to prevent. Core keeps its contract, because `collect` relies
on it; the operator surface gets the guard.

Purging is destructive and discards a restore window, so the command prints what it removed
(`purged: <key>` and the revision) rather than succeeding silently.

Two contract details, stated so they are not decided twice:

- **An absent key is an error, not an idempotent success.** This deliberately differs from
  `gonzalo delete`, which exits `0` for an absent key because delete is convergent — asking
  for a state that already holds. Purge is a targeted destructive act on a specific
  tombstone the operator believes exists, so a typo in the key must be visible rather than
  reported as success. Refusals and absent keys both exit `1` with the reason on stderr, as
  `gonzalo undelete` does; `EXIT_CONFLICT` (3) is not reused, because neither case is
  retryable with a fresh revision.
- **No confirmation prompt and no `--yes` flag.** `delete`, `reset` and `collect` are all
  destructive and none prompts, so adding one here alone would be an inconsistency rather
  than a safeguard. The protection is that the command refuses to touch a live record at
  all, and that reaching it at all requires the operator to have been told to by the guard.

## What operators and callers see

| Situation | Before | After |
|---|---|---|
| `gonzalo index` after `gonzalo delete`/`reset` on a graph manifest | silently destroys the restore window | fails with the constant's guidance; nothing is written |
| `RecordVectorIndex` upsert on a deleted index | silently destroys the window and orphans the shards | first commit fails with the same guidance; shards stay pinned |
| Deliberate rebuild from scratch | implicit, by writing over the tombstone | `gonzalo purge` then write |
| Accidental delete, operator wants it back | `gonzalo undelete`, if nothing wrote first | `gonzalo undelete`, and nothing ordinary can have written first |
| Replication of a create over a tombstone | `plan_put_raw` Conflicts | unchanged |
| `undelete` | rides `plan_put_raw`'s matching-revision arm | unchanged |

### Known limit this design accepts

A `RecordVectorIndex` opened over a tombstone still **opens successfully** and fails later,
at its first commit, because `open_with_shards` keeps reading through `store.get`. Nothing is
lost and the failure is loud, but the error surfaces from inside a commit rather than from
the call that was actually wrong. Fixing it means `open` reading raw and refusing early, with
an explicit recreate affordance — a change to published `gonzalo-vector` API that belongs in
its own ticket. **File a follow-up.**

## Testing

Red-first wherever a test states a real claim.

| Test | What it proves |
|---|---|
| A create over a `VectorManifest` tombstone is `Rejected` | The guard, on the severe kind |
| Same for a `GraphManifest` tombstone | Decision 3's symmetry |
| A create over a non-manifest tombstone still recreates | The guard is surgical; ADR 0021's rule survives where no window exists |
| `expected: Some(_)` over a manifest tombstone is still `NotFound` | The new arm did not capture the stale-revision case |
| `recreating_over_a_tombstone_clears_deleted_kind` still passes untouched | The guard reads the stored tombstone, not the incoming record |
| Conformance: a create over a manifest tombstone is refused identically on every substrate | A substrate that mapped `Rejected` differently, or wrote anyway, would silently reopen the hazard |
| End to end: delete a vector index, attempt an upsert, the commit fails and `gc` still frees nothing | The hazard is closed where it actually bit |
| End to end: `purge` then create succeeds | The escape hatch works and the guard is not a dead end |
| `purge` of a tombstone removes it and the next sweep frees the blobs | Purge still does its job |
| `purge` of a **live** record is refused and the record survives | The trap in `plan_purge` is not exposed to operators |
| `purge` of an absent key reports not-found | The command's own contract |
| CLI: `gonzalo index` after a delete fails with the guidance on stderr and exit 1 | What an operator actually hits |

The conformance case matters most: `plan_put` is reached through a `PutPlanner` function
pointer on `fs`, `git` and `memstore`, and inline at two S3 call sites, so "every substrate
refuses identically" is a claim only conformance can make.

## Documentation

- **ADR 0030**, amending **ADR 0029**: the hazard 0029 documents as a known limit becomes
  guarded. 0029's negative bullet currently ends "#333 tracks it" and must be rewritten to
  point at 0030, keeping the `RecordVectorIndex::open` late-failure limit and the new
  follow-up reference. Annotate both sides; 0029 stays `accepted`.
- ADR 0030 also answers #333's open questions explicitly: the guard went in `plan_put`
  rather than `open`; both kinds are covered; `gonzalo index` is covered by the same arm
  rather than needing its own treatment.
- `docs/guide/src/deletion.md` — the guard, and `gonzalo purge` with its live-record refusal.
- `docs/guide/src/cli.md` — a `gonzalo purge` row in the Records table.
- `CHANGELOG.md` — the new refusal as a behaviour change under the unreleased heading, since
  a `put` that used to succeed now errors, plus the new subcommand.
- No OpenAPI change: no route, schema or field moves. The daemon's existing `PUT` answers
  `400` through the unchanged `CoreError::Invalid` mapping.

## Out of scope

- Making `RecordVectorIndex::open` refuse early (the known limit above; its own ticket).
- Changing `plan_purge`'s contract in core.
- Any change to `plan_put_raw`, replication, or `undelete`.
- Closing #325's residual sweep windows (#328).
- Retaining bodies for non-manifest kinds.

## Revisit if

- A non-manifest kind starts retaining a body, at which point `deleted_kind.is_some()` stops
  being a synonym for "a manifest restore window exists" and the guard's condition needs to
  say what it means directly.
- Operators find purge-then-create too sharp in practice and want a single explicit
  recreate affordance after all.
- `PutPlan::Rejected` ever needs to carry a dynamic message, which would let the refusal
  name the key.
