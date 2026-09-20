# Deleting records, resetting namespaces, and collecting tombstones

Deleting a record in gonzalo is a **replicated write**. When you delete a record,
the store keeps a small marker, a *tombstone*, where the record was. The tombstone
travels to other stores through `sync` and git `pull` like any other change, so a
delete made on one machine takes effect everywhere and stays in effect.

Up to 0.6, deletion only removed the record from the store you ran it on, and the
next sync with a peer that still had the record brought it back. That no longer
happens. The design is recorded in
[ADR 0021](./adr/0021-replicated-deletion-with-tombstones.md).

## Upgrade every binary together

> **Warning.** 0.7 adds a record kind that older binaries cannot read. Upgrade
> every gonzalo binary that touches a store, and every program built on
> `gonzalo-core`, **at the same time.**
>
> - A 0.6 binary that reads a store holding a tombstone fails on that key with a
>   serialization error. This is loud, and no data is lost.
> - A 0.6 binary that runs **sync** against 0.7 data can't see tombstones and
>   copies deleted records back. This is **silent**, and nothing on the new side
>   can detect or block it.
> - A 0.7 client talking to a 0.6 `gonzalod` still gets, puts, lists and deletes
>   normally. Replication reads fail with
>   `daemon predates replication reads (gonzalo#203); upgrade gonzalod` instead
>   of quietly doing the wrong thing.

## Two kinds of reads

Everything an application does (`gonzalo list`, `gonzalo get`, the MCP server,
memory tiers, tickets) uses **consumer reads**. Consumer reads hide tombstones, so
a deleted record looks exactly like one that never existed.

Replication uses **raw reads**, which show tombstones, and **raw writes**, which
store a record exactly as given. `sync`, git `pull` and `gonzalo collect` need to
see deletes in order to propagate or clean them up. You won't normally use either
yourself. On the daemon they are separate routes
(see [Over the daemon](#over-the-daemon)).

The `gonzalo delete`, `reset` and `collect` commands below work on a local store
directory given by `--root`, which defaults to the current directory. For a store
behind `gonzalod`, call `gonzalo_core::reset_as` or `collect` with a `ServerStore`:
each tombstone goes through the daemon's delete route and its authorization.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. For `collect`, conflicts are reported but aren't failures |
| `1` | Error: I/O, the store, or `--ancestor-cap 0` (`ancestor cap must be at least 1`) |
| `2` | Usage error: a missing or malformed argument |
| `3` | Conflict (`delete` and `reset` only): someone changed a record you were deleting. Re-read, or re-run the reset |

Every command takes `--root <DIR>` (default `.`, a leading `~` is expanded) and
`--ancestor-cap <K>` (see [Ancestor cap](#ancestor-cap)). Each command's `--help`
ends with its exit codes.

## Deleting a record

```sh
gonzalo delete --root ~/.gonzalo --namespace notes --collection topics --id rust-async
```

```text
deleted: notes/topics/rust-async
```

The command exits `0` when the record is deleted, and also when it was already
deleted or never existed: deleting is idempotent.

Pass `--expected` to delete only if nobody has changed the record since you read
it. The value is the `revision` object from `gonzalo get`'s JSON output (e.g.
`{"counter":3,"hash":"…"}`). Anything that isn't valid revision JSON is a usage
error (exit `2`):

```sh
gonzalo delete --root ~/.gonzalo --namespace notes --collection topics --id rust-async \
  --expected '{"counter":3,"hash":"9f2c…"}'
```

If someone has changed the record, nothing is deleted, and the command prints the
current revision and exits `3`:

```text
conflict: notes/topics/rust-async
current:  {"counter":4,"hash":"b71e…"}
```

A tombstone names who deleted the record as its author. `gonzalo delete` and
`gonzalo reset` both record `gonzalo-cli`. Library `reset` and `Store::delete`
keep each record's author unless `reset_as` or `delete_as` names a deleter.
Through the daemon, a non-admin token is always recorded as itself. An admin
token, or a daemon without auth, keeps a named deleter; with no name, an admin is
recorded as itself and a daemon without auth keeps the record's last author.

Deleting a key this store has never seen writes nothing. If a peer holds a record
you want gone, delete it on that peer, or sync first and then delete.

On a filesystem store, deleting a key takes a per-record lock file
(`<id>.json.lock`) and creates the namespace and collection directories if they
don't exist, and these are left in place: removing a lock file another process
holds would break the lock. So `gonzalo delete` with a mistyped `--root` or
namespace creates those directories and still exits `0`. Check the `--root` you
pass. Reads and listings ignore lock files.

### Delete versus a concurrent edit

If you delete a record on one machine while someone edits the same version on
another, sync can't know which intent should win. It reports a **conflict** for
that key and leaves both sides as they are, the same way it reports any edit it
can't merge. Resolve it by deleting again or rewriting the record on one side,
then sync.

### Recreating a deleted record

Writing a record to a deleted key simply recreates it, and the new record wins
over the old delete everywhere it syncs. Gonzalo sets the new record's revision so
that it comes after the delete. Programs writing through the API should use the
revision the write returns, not the one they built themselves. A conditional write
that names a revision to a deleted key fails with "not found", because to an
application the key doesn't exist. Write it unconditionally to recreate it.

## Resetting a namespace

`reset` deletes every live record in a namespace, or in one collection of it:

```sh
gonzalo reset --root ~/.gonzalo --namespace scratch
gonzalo reset --root ~/.gonzalo --namespace scratch --collection sessions
```

It prints a summary such as `12 deleted, 0 conflicts` on stdout and exits `0`. If
any record was edited during the reset, it also prints one `conflict: <ns/col/id>`
line per record to stderr, followed by a final
``re-run `gonzalo reset` to delete the N conflicted record(s)`` line on stderr, and
exits `3`. Run it again to finish. `--namespace` is required, and leaving it out is
a usage error (exit `2`). Resetting every namespace at once isn't offered; loop
over namespaces if that's really what you want.

Reset is **not atomic**. It deletes records one at a time, and a record someone
edits during the reset is reported as a conflict and left alone. Reset is
**safe to re-run**: a second run deletes what the first missed and skips what's
already gone. Because reset is made of ordinary deletes, it replicates like them. Over a
daemon it needs both `read` and `write` on the namespace: `reset_as` lists
and reads each record (`read`) before deleting it (`write`).

On a git store every tombstone is its own commit, so resetting N records makes N
commits (and collecting N tombstones makes N more). `reset` and `collect` stop at
the first store error (for example, a daemon token without the needed scope) and
report it; what they already did stays done, so re-running after fixing the cause
is safe.

## Collecting tombstones

Tombstones are small, but they stay until you remove them. `collect` physically
removes tombstones older than a horizon you choose:

```sh
gonzalo collect --root ~/.gonzalo --older-than 90d
gonzalo collect --root ~/.gonzalo --older-than 90d --namespace scratch
```

`--older-than` is required and has no default. It takes a positive integer and
exactly one unit, `d`, `h`, `m` or `s` (`90d`, `36h`). Zero, a missing unit, a
compound value such as `1d12h`, an unknown unit, or a value too large are all usage
errors (exit `2`). The command prints the horizon as you typed it, with its length
in seconds, then how many tombstones it purged, how many it kept because they carry
no deletion time, and how many conflicted:

```text
horizon:   90d (7776000s)
purged:    41
unstamped: 0
conflicts: 0
```

A conflict means the key was recreated while collection ran, and the new record is
left intact. Each conflict is also printed as a `conflict: <key>` line on stderr.
`collect` still exits `0` with conflicts, since nothing needs retrying. Without
`--namespace`, it runs across the whole store. `--collection` without
`--namespace` is a usage error (exit `2`).

There is **no default horizon**, and nothing collects automatically. That is
deliberate, and the next section explains why.

Collection only frees space in the store you run it on. If a peer still holds the
tombstone, the next sync copies it back. That's harmless (it's still a delete),
but to reclaim space everywhere, run `collect` on every store with the same
horizon.

On an S3 store, collecting also makes listings cheaper. A tombstone there
carries a marker object that `list` reads to resolve it, so the reads a listing
does are bounded by the tombstones outstanding
([ADR 0025](./adr/0025-s3-tombstone-markers.md)). Collecting removes both, and a
collection with no tombstones left is listed without reading a single record.

## Choosing a collection horizon

**This is the one decision in this page that can lose data.**

A tombstone is the only thing that stops a deleted record coming back. Suppose
you collect a tombstone, and later a peer that was offline the whole time syncs.
That peer still holds the live record and never saw the delete. With the
tombstone gone, sync treats the peer's copy as a record this store has never seen
and copies it back. The record is resurrected, on a different machine, possibly
weeks later, with no error anywhere.

So the rule is:

> **The horizon must be longer than the longest time any peer might go without
> syncing, plus a safety margin.**

Gonzalo can't know how long your peers stay offline, which is why you have to say.

| Your setup | Longest realistic gap between syncs | Suggested `--older-than` |
|---|---|---|
| One store, never synced with anything | none | any, e.g. `7d` |
| A few machines that sync with a daemon daily | a long weekend, a holiday | `30d`–`60d` |
| Laptops that can be offline for weeks (travel, leave) | several weeks | `90d` or more |
| Peers you don't control, or you don't know | unknown | don't collect, or `180d`+ |

Practical advice:

- **When in doubt, go longer.** A horizon that's too long costs a little storage.
  One that's too short costs data, silently, later.
- **Clocks.** The horizon is measured against each tombstone's deletion time. A
  machine whose clock was ahead produces tombstones that look younger than they
  are, so they're collected *later*, never earlier. Clock skew can delay
  collection but can't trigger it early.
- **Log the output.** If you run `collect` from a scheduler, keep its output. It
  records the horizon used, which is what you'll want to know if a record ever
  reappears.
- **Retiring a peer?** Sync it one last time before you drop it, so its view of
  deletes is current. A peer that is gone for good can't resurrect anything.

## Reclaiming a deleted record's bytes

Deleting a blob-backed record doesn't free the blob, and neither does collecting
its tombstone on its own. Blobs are content-addressed and shared — two records
with identical content hold one blob — so freeing one means proving no record
points at it. That's a separate, explicit sweep:

```console
$ gonzalo gc --root ./store
scanned:  128
freed:    3
retained: 41
```

`gc` marks every blob the store still needs, from the records themselves: a
record whose body *is* a blob, the blob a tombstone pins, and every code-graph
slice a view's manifest names. Everything else is deleted.

**A tombstone pins the blob of the record it replaced** (ADR 0024). A delete is
replicated, not final: while the tombstone is around, a peer that never saw the
delete can still sync the record back, and re-putting the same content doesn't
re-upload it. So the bytes survive exactly as long as the tombstone does, which
is the horizon you already chose. Reclaiming them is three steps, in order:

1. `delete` the record — the tombstone appears, holding the blob;
2. `collect` past the horizon — the tombstone goes, releasing the pin;
3. `gc` — nothing references the blob now, so it's swept.

Like collection, GC is explicit and never automatic: a blob swept while some
peer still holds the record naming it can't be restored by a later sync.

## Ancestor cap

Each record keeps a short list of the revisions it came from, so sync can tell
"this side is just behind" apart from "both sides changed". The list is capped at
32 entries by default. To change it, pass `--ancestor-cap <n>` to `gonzalo delete`,
`reset`, `collect` or `sync`, or set the `GONZALO_ANCESTOR_CAP` environment
variable for `gonzalod`, which has no command-line flags. The cap must be at least
1. Most deployments never need to change it.

A record edited more times than the cap since two stores last synced is treated as
changed on both sides: sync merges it or reports a conflict, but never silently
overwrites. Peers with different caps work together correctly.

## Git stores: resolve pull conflicts before pushing

If `pull` reports conflicts, don't push until you've resolved them. A delete that
conflicts with a remote edit (or an edit that conflicts with a remote delete) keeps
your local side in the merge commit, so pushing would publish your live record
over the other side's delete.

## Over the daemon

`gonzalod` serves the same operations. Existing record routes keep their
meaning, with tombstones hidden:

| Operation | HTTP | Permission |
|---|---|---|
| Delete (writes a tombstone; deleter as above) | `DELETE /v1/records/{ns}/{col}/{id}` | `write` on the namespace |
| Raw read of one record | `GET /v1/raw/records/{ns}/{col}/{id}` | `read` on the namespace |
| Raw write (replication; revision stored verbatim, never re-stamped) | `PUT /v1/raw/records/{ns}/{col}/{id}` | `write` on the namespace |
| Raw key listing | `GET /v1/raw/keys?namespace=&collection=` | `read` on the namespace; `read` on `*` without `namespace` |
| Purge (physical removal) | `POST /v1/purge/{ns}/{col}/{id}` with body `{"expected": <revision>}` | admin |

gRPC has the matching `GetRaw`, `ListRaw`, `PutRaw` and `Purge` RPCs, with the same
permissions.

`collect` and `purge` through a daemon need an admin token; a non-admin token
fails on the first eligible tombstone, before anything is purged.

**Who a replicated record is attributed to.** A raw write never changes a record's
revision, but its author follows the same rule as a normal write. If the caller is
not an admin, the daemon sets the record's author to the caller, so nobody can plant
a record in someone else's name through the raw route. If the caller is an admin, or
the daemon runs without auth, the record keeps the author it arrived with. Use an
admin token for replication when authorship must carry across stores.

**Writes to a deleted key.** A conditional write (normal or raw) whose expected
revision the daemon rejects as not found returns **HTTP 412**
(gRPC `FailedPrecondition`), and the Rust client reports it as not found. A
conditional normal write over a deleted key is the common case. This used to be an
opaque HTTP 500. It is not a 404, because on the raw routes a 404 means the daemon
is too old to have them.

Purge requires admin because it is the one operation that can make a
delete undoable across peers.

**Blob GC is not a daemon route.** `gonzalo gc` sweeps a filesystem store
directly. A library caller can run `gc_blobs` against any store that is both a
`Store` and a `BlobStore`, a daemon-backed one included, but the token then
needs `read` on `*` (the sweep lists every key, tombstones included) and
`read`/`write` on `_blobs`. A token scoped to one namespace would mark a partial
view of liveness and sweep the rest — which is why the mark set is never a
caller's to supply.
