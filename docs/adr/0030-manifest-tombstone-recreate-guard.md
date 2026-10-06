# ADR 0030 · A create over a manifest tombstone is refused, and `gonzalo purge` discards one deliberately

- **Status:** accepted
- **Date:** 2026-10-03
- **Source:** [`docs/superpowers/specs/2026-10-03-recreate-over-manifest-tombstone-design.md`](../superpowers/specs/2026-10-03-recreate-over-manifest-tombstone-design.md)
- **Amends:** [ADR 0029](0029-manifest-tombstone-pin.md), whose negative
  consequence named this hazard and left it documented rather than closed. A
  consumer create over a manifest tombstone is now refused.

ADR 0029 is not superseded and stays `accepted`; its retention, marking and
`undelete` decisions stand unchanged.

## Context

ADR 0029 makes a deleted manifest recoverable: the tombstone keeps the body,
`gc` marks through it, and `undelete` restores it until `collect` removes the
tombstone. That window is only as strong as the weakest write that can reach the
key.

`plan_put` (`crates/gonzalo-core/src/tombstone.rs`) treats a tombstone as
absent. That is correct under ADR 0021, because consumers never see tombstones,
so from a consumer's side a deleted key and a never-written key are the same
thing, and a create there must succeed. But a create over a manifest tombstone
overwrites it: the retained body is cleared, `meta.created` is re-stamped, and
the blobs the body named lose their pin. For a vector index the shards cannot be
regenerated, so this is permanent loss, reached by an ordinary write.

Two paths reach it, and they reach it for the same reason: each derives
`expected` from a consumer `get`, which hides the tombstone, so each concludes
"nothing is here" and creates.

- `RecordVectorIndex::open_with_shards`
  (`crates/gonzalo-vector/src/record_index.rs:165`) reads the key with
  `store.get`, sees no record, and builds a fresh empty manifest. Its first
  commit is a `Record::create` with `expected: None`.
- The graph indexer in `gonzalo-cli` does the same for a `GraphManifest` key
  after `gonzalo delete`. The stakes are lower, since slices regenerate from
  source, but the restore window is gone all the same.

The vector commit path had already anticipated recreation over a tombstone, for
revision bookkeeping (`crates/gonzalo-vector/src/record_index.rs:439`: the store
may re-stamp the revision when a record is recreated over a tombstone). It did
not treat it as a data hazard.

The most likely operator action after an accidental delete is to restart the
application and reopen the index, and that is exactly the action that made
`undelete` impossible.

## Decision

**`plan_put` refuses a consumer create over a manifest tombstone, and
`gonzalo purge` is the deliberate way to discard one.**

- **The guard.** When `expected` is `None` and the stored record is a tombstone
  whose `deleted_kind` is `GraphManifest` or `VectorManifest`, `plan_put`
  returns `PutPlan::Rejected(MANIFEST_TOMBSTONE_RECREATE_REJECTED)`. The
  constant is in `crates/gonzalo-core/src/tombstone.rs` and re-exported from the
  crate root. Its message points at the two exits: `gonzalo undelete` to restore,
  `gonzalo purge` to discard.
- **The trigger is an explicit kind match, not `deleted_kind.is_some()`.**
  `deleted_kind` is `serde(default)`, and `plan_put_raw` stores replicated
  records verbatim, so a tombstone arriving from a peer can carry any
  `deleted_kind` it likes. A foreign tombstone with a non-manifest value has no
  pin and no restore window; refusing a create over it would protect nothing,
  and `undelete` would refuse it too, stranding the key with `purge` as the only
  exit. Matching the two manifest kinds keeps the refusal exactly as wide as the
  window it guards.
- **Surface.** `PutPlan::Rejected` already carries a `&'static str` and every
  substrate already maps it to `CoreError::Invalid`, which the daemon answers
  as HTTP 400 / gRPC `InvalidArgument`, not 500. No new plan variant, `Store`
  method, wire field or substrate code is needed. The `PutPlan::Rejected`
  rustdoc previously said `Backend`; that was stale and now says `Invalid`.
- **Replication and `undelete` are untouched.** `plan_put_raw` is unchanged, so
  a replicated create over a tombstone still `Conflict`s and `undelete` still
  writes through it. A conformance case holds all five substrates to the
  refusal and asserts the tombstone's revision, retained body and `deleted_kind`
  all survive it.
- **`gonzalo purge --namespace --collection --id`.** It removes one tombstone
  and discards its restore window. It reads raw and refuses anything that is not
  a tombstone. An absent key is an error (exit 1), unlike `gonzalo delete`,
  because an operator who typed a key to discard expects it to exist. There is
  no prompt and no `--yes`: the command's only effect is the one its name says.
  It prints the key and the removed revision. `Store::purge` and `plan_purge`
  are unchanged.

### Why the chokepoint, not `open`

The hazard is a property of the consumer write path, not of one function.
Guarding `RecordVectorIndex::open` alone would fix the likeliest trigger and
leave the graph indexer, and every future writer that derives `expected` from a
consumer `get`, exposed. Putting the check in `plan_put` makes it hold once, for
every substrate and every caller, and it is the only place that sees the stored
tombstone and the incoming write together.

### Why both manifest kinds

ADR 0029 retains both bodies for the same reason, so the guard follows it. An
asymmetric rule, guarding vector manifests but not graph manifests, would need
justifying forever, and it would leave a graph manifest's window silently
destroyable by the first `gonzalo index`.

### Why the purge check is in the CLI, not `plan_purge`

`plan_purge` is kind-blind: it removes whatever record matches the revision.
`collect` depends on that contract, and `Store::purge` is documented as
collection's mechanism, so tightening it would change a surface other code
relies on. Purging a live record would leave no tombstone behind, and a peer
that had not synced since would resurrect it (ADR 0021). The check therefore
sits at the operator surface, where a person can mistype a key, and the CLI
refuses a live record and tells them to `delete` it instead.

## Consequences

- **Positive:** the restore window ADR 0029 promises now holds against ordinary
  writes, not only against an operator who knows to run `undelete` first.
  Reopening a deleted index no longer destroys it.
- **Positive:** no new wire surface, plan variant or substrate change. Operators
  gain a precise single-key purge they did not have; before, discarding one
  tombstone meant `collect` with a horizon that swept others too.
- **Negative:** **`gonzalo reset` followed by `gonzalo index` now fails** until
  the operator runs `gonzalo purge` or `gonzalo undelete` on each manifest key.
  That is friction by design: `reset` tombstones manifests, and rebuilding over
  them used to discard the window silently.
- **Negative:** **a `put` that used to succeed now errors.** Any client that
  creates at a manifest key over its tombstone gets a 400 where it used to get
  a revision. This is a behaviour change, and it is the point of the guard.
- **Negative:** the refusal message is a `&'static str` and cannot name the
  key. The caller already knows which key it wrote.
- **Negative:** **a `RecordVectorIndex` opened over a tombstone still opens
  successfully and fails only at its first commit.** `open` reads through
  `store.get`, which hides tombstones, so it cannot tell. Nothing is lost and
  the failure is loud, but the error surfaces from inside a commit rather than
  from the call that was wrong. Tracked in gonzalo#340.
- **Negative:** **a refused upsert still costs one orphaned shard blob until the
  next sweep.** `RecordVectorIndex` stages the shard blob before the manifest
  write that is then rejected. It is the same class of orphan a lost OCC race
  already produces (`crates/gonzalo-vector/src/record_index.rs:368`), reclaimed
  by `gc`, but it is a new way to reach it.
- **Negative (known limitation):** **the daemon exposes `Store::purge` to an
  admin principal with no tombstone check**
  (`crates/gonzalo-server/src/service.rs:162`,
  `crates/gonzalo-server/src/http.rs:430`,
  `crates/gonzalo-server/src/grpc.rs:285`). An admin can therefore physically
  remove a live record over either transport, which is the same ADR 0021
  resurrection hazard that the CLI check closes. The route arrived with the
  replication surface and predates this decision; closing it is out of scope
  here and is tracked in gonzalo#341.

### Open questions from the ticket

The ticket (#333) left three questions open. The guard went in `plan_put`
rather than `open`, for the reason above. Both manifest kinds are covered.
`gonzalo index` needs no separate treatment, because the same `plan_put` arm
covers it.

## Revisit if

- A non-manifest kind starts retaining a body. A restore window would then exist
  for a kind the explicit match does not name, and the guard's trigger would
  need widening deliberately rather than by `deleted_kind.is_some()`.
- Operators find purge-then-create too sharp a way to discard a deleted index.
- `PutPlan::Rejected` gains a dynamic message, which would let the refusal name
  the key.
- The daemon's purge route gains a tombstone check (gonzalo#341), or
  `RecordVectorIndex::open` learns to see tombstones (gonzalo#340).
