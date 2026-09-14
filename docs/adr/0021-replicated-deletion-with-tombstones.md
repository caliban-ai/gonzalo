# ADR 0021 · Replicated deletion with tombstones

- **Status:** accepted
- **Date:** 2026-09-13
- **Source:** [`docs/superpowers/specs/2026-09-13-tombstone-replication-design.md`](../superpowers/specs/2026-09-13-tombstone-replication-design.md)

## Context

**This ADR supersedes the local-only deletion decision of
[ADR 0018](0018-record-deletion-and-sync.md).** ADR 0018's OCC semantics for
`delete` still hold: an `expected` revision, `Conflict` on a mismatch, and
atomicity in the same critical section as `put`. Only its choice to make deletion
local-only, which it deferred as a known sharp edge, is replaced here.

Under ADR 0018, `delete` physically removed a record. Gonzalo replicates in two
ways: `sync` takes the union of two stores' keys and copies one-sided records
across, and git `pull` ([ADR 0017](0017-nonff-pull-content-merge.md)) runs a
three-way merge. A physical delete leaves no trace, so when sync met a peer that
still held the record, "deleted here" and "never existed here" looked the same and
sync copied the record back. **A deleted record came back on the next sync.**

gonzalo#203 asked for a first-class namespace reset: clear a namespace or a
collection in one call, with the same meaning on every substrate. A reset built
on local-only delete has the same resurrection problem at scale: reset a namespace
on a laptop, sync with the team daemon, and every record returns. Gonzalo targets
deployments from one local directory up to replicated daemons over shared object
storage, and a reset that only works if you never sync doesn't meet that. So
deletion had to replicate before reset could be built.

Options weighed:

- **Local-only reset.** Simple, and it resurrects on the first sync. Unusable as
  soon as a namespace is replicated. Rejected.
- **Hybrid: ship local-only reset now, tombstones later.** This ships a reset
  whose meaning changes in a later release, which is worse than waiting for the
  right meaning. Rejected.
- **Tombstones.** Deletion becomes a replicated write. Taken, with the details
  below.

## Decision

We will make deletion a **replicated write**.

**Tombstone record.** `delete` writes a record of the new kind
`RecordKind::Tombstone` at the key's normal path. There is no side index and no
new storage layout. The tombstone has an empty inline body, `parent` set to the
deleted revision, and `deleted_at` set to the deletion time in milliseconds since
the Unix epoch. Its revision is `{ counter: deleted.counter + 1, hash:
ContentHash::of(b"gonzalo:tombstone:v1") }`. The domain-separated hash means a
tombstone's revision can never equal a live edit's revision. If tombstones used
the empty-body hash, a live edit to an empty body at the same counter would look
"already in sync" with a concurrent delete. The same construction makes two peers
that independently delete the same revision produce identical tombstones, which
sync treats as already in sync. The shared helpers live in
`crates/gonzalo-core/src/tombstone.rs`.

**Two surfaces.** Consumer reads (`get`, `list`) hide tombstones, so a deleted
key looks absent to applications. The replication surface shows them, and `sync`,
`pull` and collection use only that surface:

- `get_raw` and `list_raw` read tombstones;
- `put_raw(record, expected)` writes a record exactly as given;
- `purge` physically removes a record if its revision matches, and is now the only
  physical removal in the system.

All four are **required** `Store` methods with no default implementation. A default
of `get_raw = get` or `put_raw = put` would compile, pass every test that doesn't
involve deletion, and resurrect records in production. For the same reason, the
daemon client never falls back from a replication call to a consumer call. Against
a daemon that predates these routes (HTTP 404 on a raw route, or gRPC
`Unimplemented`) it returns the explicit upgrade error `CoreError::Backend("daemon
predates replication reads (gonzalo#203); upgrade gonzalod")`.

**Consumer writes.** `delete` of a live record writes the tombstone, or conflicts
on a stale `expected`. `delete` of a tombstone or an absent key writes nothing and
returns `Deleted`. An absent key has no revision a tombstone could descend from. A
consumer `put` with `expected = None` over a tombstone is a **recreation**. The
store re-stamps the caller's revision to `tombstone.counter + 1` with the
tombstone as `parent`, so the recreated record is newer than the delete instead of
losing to it. A consumer `put` with any `Some(expected)` over a tombstone returns
`NotFound`, because to a consumer the key is absent.

**Replication writes never re-stamp.** `put_raw` follows one rule:

- nothing stored and no `expected`: write;
- nothing stored but `Some(expected)`: `NotFound`;
- `expected` equals the stored revision, tombstone or not: write the record
  verbatim;
- anything else: `Conflict`, carrying the stored record, which may be a tombstone.

This closes a resurrection window. Sync copies a record that exists on only one
side after a raw read of the other side. If that copy went through consumer
`put(record, None)`, a delete landing on the destination between the raw read and
the write would turn the copy into a recreation, re-stamped newer than the delete,
and the deleted record would come back. Through `put_raw` the same race is a
`Conflict` that the next sync pass resolves with the tombstone in view.

**Authorship.** `delete_as(key, expected, author: Option<Identity>)` is the
required delete method, and `delete` is a provided method that passes no author. A
tombstone carries the deleted record's metadata. When an author is given, it
becomes the tombstone's author, so the record of a delete names who deleted it
rather than who last edited it. With no author, the tombstone keeps the record's
last author. `gonzalo delete` and `gonzalo reset` both record `gonzalo-cli`.
Library `reset`, and the provided `delete` it uses by default, pass no author, so
they keep each record's own last author; `reset_as` names a deleter, the same way
`delete_as` does.

**Who may name a deleter.** Over the daemon, a delete may carry an optional
claimed deleter: the `author` field of the HTTP `DELETE` body, or gRPC
`DeleteRequest.author_json`. `ServerStore::delete_as` sends its author there. The
daemon keeps [ADR 0015](0015-namespace-scoped-daemon-auth.md)'s promise that
authorship can't be forged, and stamps `Principal::delete_author(claimed)`:

- a non-admin principal is always stamped as itself, whatever it claims;
- an admin keeps the claim, or is stamped as itself when there is none;
- a daemon running without auth keeps the claim, or stamps none when there is
  none, so the tombstone keeps the record's last author.

ADR 0015 has no admin role of its own: an admin is a principal with `"*"` in both
its read and write lists, and a daemon without auth counts as admin.

Replication writes follow the same author rule. `put_raw` never re-stamps a
record's *revision*, but the daemon does protect its author. A `put_raw` from a
non-admin principal has `meta.author` restamped to that principal, exactly like a
consumer `put`. A `put_raw` from an admin principal, or on a daemon running without
auth, keeps the replicated record's author. An admin token is the replication
credential, and only it can carry another writer's authorship across stores.
Without this rule, any principal with `write` on a namespace could forge records
attributed to someone else by sending them through the raw route.

**Raw-write trust equals namespace-write trust.** `put_raw` needs only `write` on
the namespace, and the writer supplies the record's revision, `ancestors` and
`deleted_at` verbatim, including a tombstone. A namespace writer can therefore
write a tombstone with any `deleted_at`, making it collectable early, or an
`ancestors` list that makes sync fast-forward over a peer. That is no more than
`write` already allows, since a writer can delete or overwrite the record
directly. Authorship is the only thing the daemon protects, as above.

A consumer `put` or `put_raw` that the store rejects with `NotFound`, such as a
`Some(expected)` over a tombstone, is returned by the daemon as HTTP 412 or gRPC
`FailedPrecondition`. The client maps that back to `NotFound`. 404 is not used,
because on the raw routes a 404 is how the client recognises a daemon that predates
them.

Every substrate makes these decisions through the same pure planner functions in
core, so they behave identically by construction. The conformance suite
(`crates/gonzalo-core/src/conformance.rs`) proves it on each substrate. The lock-based
stores (fs, git) plan inside their lock. s3 has no lock: its writes are
conditional on the ETag it read. When a conditional write loses a race —
`PreconditionFailed` (HTTP 412), `ConditionalRequestConflict` (HTTP 409), or
`NoSuchKey` (the object vanished to a concurrent purge) — s3 re-reads, re-plans
and retries, up to 8 attempts, then returns a backend error. It therefore
reaches the same outcome a lock-based store would, instead of guessing from a
single re-read.

**Ordering by bounded ancestry.** Every record carries `ancestors`, its most recent
prior revisions sorted newest first and capped per store. The cap defaults to 32
and is never 0. It is set with `--ancestor-cap` on `gonzalo delete`, `reset`,
`collect` and `sync`, and with the `GONZALO_ANCESTOR_CAP` environment variable on
`gonzalod`. When both sides of a sync hold a key with different revisions, sync
(`crates/gonzalo-core/src/sync.rs`) decides:

- one side's revision is in the other's `ancestors`: that side is behind, and is
  overwritten (a fast-forward);
- neither contains the other, and both are tombstones: the higher
  `(counter, hash)` wins on both sides;
- neither contains the other, and exactly one is a tombstone: a `SyncConflict`,
  with neither side written. A delete racing an edit of the same revision can't be
  resolved without discarding someone's intent, and gonzalo already surfaces
  unmergeable divergence rather than guessing;
- neither contains the other, and both are live: the existing merge path.

A chain longer than the cap looks like divergence, so the bounded list **fails
safe**: a conflict or a merge, never a silent overwrite. Records written before
this change have no ancestors and take the old merge path unchanged. git `pull`
applies the same kind rules to paths changed on both sides of its real merge base.

**Reset.** `reset` tombstones every live record under a prefix that must name a
namespace, using ordinary conditional deletes. It is not atomic, because no
substrate offers multi-key transactions. It is idempotent: a re-run tombstones what
the first run missed and reports keys edited concurrently as conflicts. It needs
only `write` on the namespace. It stops at the first store error, and a re-run
after fixing the cause is safe.

**CLI.** The CLI works on a local fs store only (`--root <DIR>`, default `.`).
There are three commands:

- `gonzalo delete --namespace <N> --collection <C> --id <I> [--expected <REVISION_JSON>]`, where `--expected` is the `revision` object from `gonzalo get`'s JSON output (e.g. `{"counter":3,"hash":"…"}`);
- `gonzalo reset --namespace <N> [--collection <C>]`;
- `gonzalo collect --older-than <DURATION> [--namespace <N> [--collection <C>]]`, where `<DURATION>` is a positive integer plus exactly one unit of `d`, `h`, `m` or `s`.

Each also takes `--ancestor-cap <K>`, which defaults to 32; a cap of 0 is an error.

Exit codes are `0` on success, `1` on an error, `2` on a usage error, and **`3` on a
conflict**: a stale `--expected` on delete, or one or more concurrently edited
records on reset (re-run to finish). `reset` prints one `conflict: <key>` line per
conflict on stderr, then `X deleted, M conflicts` on stdout, and, when M > 0, a
final ``re-run `gonzalo reset` to delete the M conflicted record(s)`` hint on
stderr. A conflict is a normal, recoverable outcome
([ADR 0005](0005-optimistic-concurrency-and-conflict-surfacing.md)), so a script can
tell "re-read and retry" apart from "something is broken" without parsing output.
`collect` exits `0` even when it reports conflicts. A collect conflict means the
key was recreated during collection, and there is nothing to retry.

**Collection.** `collect` purges tombstones whose `deleted_at` is at least an
operator-supplied horizon old, conditionally on the tombstone's revision, so a key
recreated meanwhile survives. There is no default horizon, and the CLI requires
`--older-than`, which rejects 0. Tombstones without `deleted_at` are never collected, and a
future-dated `deleted_at` (clock skew) counts as too young. On the daemon, raw
reads need `read` on the namespace and `put_raw` needs `write` on it; `purge` needs
admin, and unscoped raw listing needs `read` on `*` (ADR 0015).

s3 purge requires an **atomic conditional `DeleteObject`**: `If-Match` evaluated in
the same critical section as the removal. RustFS `1.0.0-beta.8` is not atomic. It
evaluates `If-Match` on arrival and deletes whatever object is current when the
removal lands: 197/200 violations in a raw purge-versus-create probe, and 39/50
through `S3Store`. RustFS `1.0.0-rc.6` is atomic: 0/200 and 0/50. **RustFS ≥
1.0.0-rc.6 is the minimum**, and `docker-compose.rustfs.yml` pins it.
[ADR 0019](0019-s3-backend-qualification-rustfs.md) is left as written, because
accepted ADRs are append-only; its qualification table reflects the original
beta.8 run. Other S3 backends need the hardening tracked in gonzalo#286 before
they can be qualified.

**Version.** Required trait methods (`get_raw`, `list_raw`, `put_raw`, `purge`,
`delete_as`) and a new `RecordKind` variant break
`gonzalo-core`'s API, so this ships as 0.7.0 across the lockstep workspace.

Rejected alternatives for the mechanism:

- **Counter comparison for ordering** (higher counter wins). A peer that edits many
  times offline gets a higher counter than a peer that deleted once, so a stale
  edit would beat a newer delete. Counters count edits; they don't order them.
- **Version vectors.** Exact under every topology, but they need an entry per
  writer that grows with every replica and principal. That doesn't fit a record
  format that is also a human-readable file in git.
- **Automatic or background collection.** Purging a tombstone too early is data
  loss that shows up later, on a different machine, when a long-offline peer syncs
  its live copy back. Only the operator knows how long peers stay offline. An
  automatic collector would have to guess, and a wrong guess fails silently.
  Explicit collection keeps that decision with the operator and prints the horizon
  used. A hub that tracks each peer's last sync could make collection safe
  automatically later. That's an optimisation, not a correctness requirement.
- **A separate tombstone index.** A side index would let `list` skip reading
  records, but it is a second structure that must stay consistent with the records
  on every substrate, and it hides deletes from git history. Keeping the tombstone
  at the record's own path shows a delete as an ordinary modification.

## Consequences

- **Positive:** a delete sticks across `sync` and `pull` on every substrate, so
  namespace reset means the same thing locally and replicated. Sync fast-forwards
  exactly when one side is behind: before this, an `Opaque` kind such as
  `Checkpoint` reported a conflict even though nothing had diverged. Concurrent
  deletes converge without coordination, and delete-versus-edit is surfaced rather
  than silently decided. The HA soak (`crates/gonzalo-soak/`) races deletes against
  edits and recreations across daemon replicas under replica-kill chaos. It checks
  that acknowledged deletes survive; that no two conditional writes commit on the
  same base revision (`StaleBaseCommitted`), which catches both a resurrection over
  a tombstone and a delete that wipes out a committed edit; that every run
  observes at least one delete conflict, seeded deterministically so the check
  doesn't depend on scheduling; and that the replicas agree on each lifecycle
  key's deletion state. Every soak replica fronts one shared bucket and no sync
  runs between them, so the soak tests the daemon's read and write paths, not
  replication itself, which the sync and pull tests cover.
- **Negative:** tombstones take storage until an operator collects them. Consumer
  `list` has to read each record to filter tombstones, which on s3 is a `GetObject`
  per key and expensive for large namespaces. Deleting a blob-backed record doesn't
  reclaim its blob, because blobs are content-addressed and may be shared, so purge
  can't remove them. **Mixed versions are unsafe:** a pre-0.7 binary that reads a
  store holding a tombstone fails on that key with a serialization error (loud, no
  data loss), and a pre-0.7 binary that runs sync can't see tombstones and copies
  deleted records back (silent). Every binary that reads a store or runs sync must
  be upgraded together. The store can't tell that from a genuine recreation, so it
  can't block it. A collection horizon shorter than a peer's offline window
  resurrects records on that peer's next sync. Recreation re-stamps the caller's
  revision, so a caller that ignores the returned revision and reuses its own gets a
  conflict on its next conditional write. Blob garbage collection, `list`
  performance on s3, and populating `Meta.created`/`Meta.updated` are follow-up
  work.

  **git.** Don't push a git store while `PullReport.conflicts` is non-empty. A
  delete-versus-edit conflict keeps the local side in the merge commit, so a push
  would publish the live record over the remote's tombstone. A crash between
  writing a tombstone to the working tree and committing it leaves the tombstone
  uncommitted, and a later pull's forced checkout discards it. `put` already has
  the same window; it is tracked in gonzalo#283. Non-fast-forward pull does not yet
  use ancestor ordering the way sync does (gonzalo#289), and `SyncReport` does not
  signal when sync stops without converging (gonzalo#290). Independent deletes of
  the same revision converge, but each peer stamps its own `deleted_at`, so peers
  may purge at different times. That is harmless, because purge removes only the
  exact revision it names.
- **Revisit if:** a deployment needs collection without an operator-chosen horizon
  (the trigger for hub-tracked peer sync times); ancestor chains routinely exceed
  the cap so that fast-forwards degrade into conflicts in practice (the trigger for
  a larger default or real causality metadata); or s3 `list` cost on large
  namespaces becomes a bottleneck (the trigger for a key-level tombstone marker,
  which is a layout change needing its own decision).
