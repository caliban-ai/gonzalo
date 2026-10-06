# ADR 0031 · The daemon's physical-removal surfaces require admin, and refuse to remove a live record

- **Status:** accepted
- **Date:** 2026-10-06
- **Amends:** [ADR 0030](0030-manifest-tombstone-recreate-guard.md), two of whose
  three known limitations this closes (gonzalo#341 and gonzalo#342). ADR 0030 is
  not superseded and stays `accepted`; its `plan_put` guard and `gonzalo purge`
  escape hatch stand unchanged.
- **Amends:** [ADR 0015](0015-namespace-scoped-daemon-auth.md), which authorized
  the raw replication write with `write` on the namespace. It now requires
  admin, so the authorization table in that record's scope has one fewer
  non-admin route.

## Context

ADR 0030 closed the accidental path to destroying a deleted manifest's restore
window: `plan_put` refuses a consumer create over a tombstone carrying a
manifest `deleted_kind`. It also recorded, and deliberately left open, two ways
to reach the same damage over the daemon. Both were found in the whole-branch
review of gonzalo#333 and are now confirmed by test rather than by argument —
each reproduced as a failing assertion before this change.

**A namespace writer could destroy the restore window (gonzalo#342).**
`PUT /v1/raw/records/{ns}/{col}/{id}` authorized `Access::Write`, while the
comparable `POST /v1/purge/...` required admin. The raw route reaches
`plan_put_raw`, which writes verbatim and so bypasses ADR 0030's guard — by
design, because that same matching-revision arm is what `undelete` rides. A
tombstone's revision is `{live_counter + 1, tombstone_hash()}` and
`tombstone_hash()` is a constant, so the revision is predictable without even
reading it. A principal holding plain `Write` could therefore aim a fresh empty
manifest at the tombstone and take the retained body, the shard pin and any
chance of `undelete` with it. No race, no admin token.

**The daemon could physically remove a live record (gonzalo#341).**
`plan_purge` is kind-blind: it removes whatever matches the expected revision,
live or tombstone. gonzalo#333 guarded the CLI's `gonzalo purge` against that,
because purging a *live* record leaves no tombstone, and a peer that has not
synced since would resurrect it — the failure [ADR
0021](0021-replicated-deletion-with-tombstones.md) designed tombstones to
prevent. The daemon had no equivalent check, so ADR 0030's claim that "the check
belongs at the operator surface" held for only one of the system's two operator
surfaces.

## Decision

**1. The raw replication write requires admin**, on both transports, matching
`POST /v1/purge/...` and the `Purge` RPC. The authorization check also now runs
*before* the request body is inspected, so an unauthorized caller cannot learn
whether its record agreed with the path, and on gRPC cannot reach serde at all
(the ordering gonzalo#146 established for `purge`).

This is the credential replication already uses: ADR 0015 named an admin token
the replication credential, and daemon-to-daemon and operator replication run
with one. The non-admin path existed only to be hardened — a non-admin's raw
write was restamped with the caller's identity so authorship could not be
forged. That restamping is now unreachable and is deleted; authorship stays
unforgeable by construction, because no non-admin reaches the route.

**2. `Service::purge` refuses to remove a live record**, so the daemon and the
CLI agree. The refusal is `CoreError::Invalid`, which both transports already
map to the caller's error rather than an outage (`400` / `InvalidArgument`,
gonzalo#299), and the message directs the operator to `gonzalo delete` while
warning that purging the resulting tombstone would discard the restore window.

**The refusal is deliberately narrow: only a revision that *matches* a live
record.** That is the only call that would actually remove one. Two neighbouring
cases must keep their existing answers, because tombstone collection depends on
both, and a first, broader attempt at this guard broke the second:

- A **mismatched** revision over a live record is `collect` losing an OCC race
  to a recreate between listing and purging. `plan_purge` answers `Conflict` and
  removes nothing, which `collect` records in `CollectReport::conflicts`.
  Refusing there instead would turn a routine race into a hard failure of the
  whole collection run — caught by
  `conformance::purge_conflicts_after_recreation` across all five substrates.
- A key that is **already gone** still reports `Deleted`, so re-running
  collection after a partial run stays safe.

### Alternatives rejected

- **A kind-aware refusal on the raw route**, leaving ordinary replication to
  non-admin tokens. This was gonzalo#342's narrower option. It breaks
  convergence: a peer that legitimately purged a tombstone and created a
  manifest at that key could no longer replicate that state, because the
  receiving daemon would refuse the write. Replication must be able to carry any
  state a peer legitimately reached.
- **A guard inside `plan_put_raw`.** Its matching-revision arm is exactly what
  `undelete` rides, so refusing there would break the capability the guard
  exists to protect.
- **A guard inside `plan_purge`.** That is core, shared by `collect` and every
  substrate, and it is where the kind-blindness is load-bearing: `collect`
  needs `Conflict`, not an error, and ADR 0030 deliberately kept kind-awareness
  out of core. The check belongs at the operator surface, which is what
  `Service` is for the daemon.

## Consequences

- **Positive:** the two documented ways to destroy a manifest's restore window
  over the daemon are closed, and both are pinned by tests that fail without the
  change — including end to end against a real daemon over HTTP.
- **Positive:** the daemon's two physical-removal-equivalent routes now agree
  with each other and with the CLI. "Physical removal needs admin" is true of
  the whole surface rather than half of it.
- **Positive:** authorization precedes body handling on both raw transports, so
  the gonzalo#146 ordering now covers the raw write too.
- **Negative, BREAKING:** **a replication peer configured with a non-admin token
  stops working.** Its `put_raw` begins returning `403` / `PermissionDenied`
  where it previously succeeded. ADR 0015 already described an admin token as
  the replication credential, so a correctly configured peer is unaffected, but
  this is an API-visible authorization change and any peer relying on a scoped
  token must be re-credentialed.
- **Negative, BREAKING:** an admin purge of a **live** record at its current
  revision now fails with `400` / `InvalidArgument` where it previously
  succeeded. Tombstone collection is unaffected; a caller deliberately removing
  a live record must use `gonzalo delete` and then purge the tombstone, which is
  the sequence that keeps peers convergent.
- **Negative:** `Service::purge` costs one extra `get_raw` per call. Collection
  over a daemon therefore pays two reads per *eligible tombstone* rather than
  one — it already reads each key to classify it. That is proportional, not
  asymptotic, and collection is background maintenance, but it is a real cost on
  a large store.
- **Negative (known limitation):** `RecordVectorIndex::open` still reads through
  `store.get`, so opening a deleted index succeeds and fails later at the first
  commit. Unchanged by this decision and still tracked in gonzalo#340.

## Revisit if

- A replication topology appears that genuinely needs a non-admin writer — a
  partially-trusted peer allowed to replicate one namespace and nothing else.
  The answer would be a distinct replication capability rather than reverting to
  `Access::Write`, since plain namespace write is what made the restore window
  reachable.
- Some peer-repair flow turns out to need the physical removal of a live record.
  Then the route should be reopened *explicitly*, with the resurrection hazard
  stated, rather than left reachable by omission as it was here.
- The extra `get_raw` per purge shows up in collection timings on a large store.
  The revision-hash shortcut — a tombstone's hash is the constant
  `tombstone_hash()`, so a non-matching hash could be refused without a read —
  would avoid it, at the cost of a proxy check that a record whose body happens
  to equal the tombstone domain would defeat.
