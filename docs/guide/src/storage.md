# Storage backends

Every substrate implements one `Store` trait over one uniform `Record`
([ADR 0002](./adr/0002-uniform-record-store-core.md),
[ADR 0004](./adr/0004-pluggable-storage-substrates.md)). Moving from local disk to git,
S3 or a daemon is a configuration change, not a code change. Each substrate must pass
the shared conformance suite that `gonzalo-core` ships
([ADR 0006](./adr/0006-substrate-conformance-suite.md)).

## The facade

Rust consumers depend on the `gonzalo` crate and enable what they need
([ADR 0009](./adr/0009-workspace-layout-and-facade.md)):

| feature | brings in |
|---|---|
| `fs` (default) | `gonzalo-store-fs`, the filesystem substrate |
| `git` | `gonzalo-store-git`, the git-backed substrate |
| `s3` | `gonzalo-store-s3`, the S3-compatible substrate |
| `remote` | `gonzalo-store-server`, a client `Store` over a running `gonzalod` |
| `vector` | `gonzalo-vector`: the `Embedder` trait and vector indexes |
| `knowledge` | `gonzalo-knowledge`: records plus vector search by `RecordKey` ([ADR 0011](./adr/0011-knowledge-store-capability.md)); implies `vector` |
| `graph` | `gonzalo-graph`: the tree-sitter code graph |
| `ticket`, `ticket-github`, `ticket-jira`, `ticket-linear`, `ticket-gitlab`, `ticket-asana` | the ticket layer and its connectors |

The core and `gonzalo-domain` (typed views for memory tiers, topics, sessions,
checkpoints and tickets) are always included.

## Choosing a substrate

| substrate | good for | notes |
|---|---|---|
| **fs** | a single machine; the CLI and `gonzalo-mcp` | zero dependencies. Per-record file locks make concurrent writers on one host safe. Two fs stores reconcile with `gonzalo sync`. |
| **git** | sharing state between machines or people through an ordinary git remote | commits every write. `GitStore::pull(remote, branch)` merges non-fast-forward histories by record content ([ADR 0017](./adr/0017-nonff-pull-content-merge.md)); `push` publishes. Not a daemon substrate, and not a blob store. |
| **s3** | several stateless `gonzalod` replicas over shared storage | the backend must support atomic `If-Match` conditional writes. See below. |
| **remote** | a Rust program using a daemon someone else runs | HTTP or gRPC, optional bearer token. See [Running gonzalod](./daemon.md#from-rust). |

The daemon can serve `fs` or `s3` (`GONZALO_STORE`). The CLI and `gonzalo-mcp` work on
`fs` roots.

### S3: only atomic conditional writes are safe

Optimistic concurrency over S3 depends on the object store enforcing `If-Match`
atomically. "S3-compatible" does not imply that. A store that checks and then sets
lets concurrent writers all commit, and updates are lost without any conflict being
reported, which is the one outcome gonzalo exists to prevent
([ADR 0019](./adr/0019-s3-backend-qualification-rustfs.md)).

| backend | atomic `If-Match` | status |
|---|---|---|
| RustFS | yes | **the qualified backend** for multi-replica HA |
| MinIO | yes | technically sound; not chosen, for project-sustainability reasons |
| Garage | no | **unsafe**: lets several racers commit |

For AWS S3 itself, set `GONZALO_S3_BUCKET` with no endpoint and let the ambient AWS
configuration supply region and credentials. ADR 0019's qualification runs did not
cover it.

Qualify any other backend by running `gonzalo-store-s3`'s conditional-write
conformance case against it, not by reading its compatibility matrix.
`scripts/rustfs-up.sh` starts a pinned single-node RustFS for local testing, and the
`gonzalo-soak` crate runs replicas against it while killing them.

## Concurrency and conflicts

Writes are optimistic. `put(record, expected)` names the revision the caller believes
is current (`None` for "expect no record"). If the store holds something else, the
result is `PutResult::Conflict` carrying the live record. A conflict is a normal,
typed, recoverable result, never an error and never a silent overwrite
([ADR 0005](./adr/0005-optimistic-concurrency-and-conflict-surfacing.md)).

Sync uses the same machinery. Records carry content-addressed ancestry, so two
diverged copies merge 3-way against their common ancestor, with the merge rule chosen
by record kind; append-only kinds auto-merge
([ADR 0016](./adr/0016-threeway-merge-stored-ancestry.md)).

## Deletion

`Store::delete(key, expected)` is OCC-aware in the same way as `put`: with `Some(rev)`
it deletes only if `rev` is still current, and otherwise returns
`DeleteResult::Conflict` with the live record. Deleting an absent key is an idempotent
success.

In released versions a delete is **local only**
([ADR 0018](./adr/0018-record-deletion-and-sync.md)): it does not propagate through
sync or git pull, so syncing with a peer that still holds the record brings it back.
Delete on every side, or do not rely on sync to carry deletes.

Replicated deletion through tombstones is under way in
[#203](https://github.com/caliban-ai/gonzalo/issues/203) and not yet released. Until
the release notes say otherwise, ADR 0018 describes the behaviour you can rely on.

## Blobs

The fs and S3 substrates also implement `BlobStore`, a content-addressed store for
large bodies. Blobs are keyed by content hash and written only if absent, so storing
the same bytes twice is a no-op. Code-graph slices shared across views are stored
this way ([ADR 0012](./adr/0012-code-graph-two-level-keying.md)).

## Vector search and embeddings

`gonzalo-vector` defines the `Embedder` trait and an exact in-memory index,
`MemoryVectorIndex`. Its `hnsw` feature adds `HnswVectorIndex`, an approximate index
for larger collections ([ADR 0014](./adr/0014-approximate-vector-index-backend.md)).
`gonzalo-embed` provides a local CPU embedder built on Candle and all-MiniLM
([ADR 0013](./adr/0013-local-candle-embedder.md)). It is a separate crate, not a
facade feature.
