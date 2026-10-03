# Guiding Principles & Invariants

Gonzalo's design philosophy is otherwise recoverable only by reading the full
[ADR log](./adr/index.md) and the founding design spec. This page **synthesizes**
that philosophy into one place: the guiding principles that shape the system and
the inviolable invariants it must never break. It complements — it does not
replace — the ADRs; each item cites the ADR(s) it derives from, and should be
kept in sync when those are superseded.

## Guiding principles

1. **One uniform `Record`, one generic `Store` ("Approach A").** Every domain
   type is a serde view over `Record { key, kind, revision, parent, body, meta,
   links }`; versioning, concurrency, conflict, and sync are written once in the
   core (ADR 0002).
2. **Substrate pluggability — the backend is configuration, not API.**
   fs/git/S3/daemon-client each implement only `Store`; moving from local to
   git/S3/daemon is a config change, not a code change; fs is the
   zero-dependency default (ADR 0004, 0009).
3. **Optimistic concurrency with explicit, typed conflict surfacing.**
   `put(record, expected_parent_rev)`; a stale parent yields
   `PutResult::Conflict` (recoverable, not an error); merge is keyed by
   `RecordKind`, and `Sync` reuses the exact same machinery (ADR 0005).
4. **Layering discipline — capabilities compose over a storage-only core.**
   Vector, graph, tickets, and knowledge are added as layers, each keyed by
   `RecordKey`; substrates never know about vectors or graphs (ADR 0008, 0010,
   0011, 0012).
5. **Minimal-core-touch for new capabilities.** A new layer may register a
   `RecordKind` at most — no new core traits or types enter (ADR 0010, 0011).
   This aligns with the rule of three for promoting anything into the core.
6. **Provider-agnostic boundaries via traits.** Embedding goes through
   `Embedder`, tickets through `TicketSource`; provider divergence is handled by
   `capabilities()` negotiation, never `if provider == …` branches (ADR 0008,
   0010).
7. **The conformance suite is the executable contract.** One shared conformance
   suite that every `Store` implementation must pass (ADR 0006).
8. **A single canonical schema behind dual transports.** gRPC (tonic) and
   HTTP/JSON (axum) sit over one service layer; `gonzalo-proto` is the single
   schema, so the transports cannot drift (ADR 0007).
9. **Single-facade public surface + workspace discipline.** Caliban depends on
   one crate — the `gonzalo` facade; substrates and layers are toggled by Cargo
   feature (ADR 0009). That surface is **grouped by domain**: the root holds the
   record and store core and nothing else, and every domain type lives under the
   module naming its layer — `gonzalo::ticket::State`, `gonzalo::fleet::Person` —
   so gonzalo's nouns cannot collide with a consumer's own, and two layers may
   use the same word (ADR 0026).
10. **Normalize on the shared spine, preserve the raw losslessly.** Tickets
    normalize to `State { category, resolution, raw_name, raw_id }` plus a
    bounded `fields` map; the status signal is configured per connection, not
    hard-coded per provider (ADR 0010).
11. **Separate storage identity from query identity.** The code graph keys
    slices by content + grammar hash (which dedups) and resolves them through a
    per-worktree manifest — git's blob/tree split (ADR 0012).
12. **Retrieval returns first-class records, never bare ids.** Vector, graph,
    knowledge, and ticket queries resolve back through the `Store` to whole
    records (ADR 0008, 0011).
13. **Rust-native deliverables; the daemon is the non-Rust boundary.** We ship
    Rust crates, the `gonzalod` image, and binaries of our own binary crates —
    not client SDKs in other languages. Non-Rust consumers integrate over the
    daemon and its published schema, which ADR 0007 already keeps from drifting
    (ADR 0020). The schema is published as artifacts, not just promised: the
    `.proto` and an OpenAPI document are attached to each release, so a client
    is generated and owned by its consumer rather than maintained here.
14. **Deletion is a write, not an absence.** A delete writes a tombstone that
    `sync` and git `pull` replicate like any other record, so a delete made on
    one store takes effect on its peers instead of being undone by the next
    sync. Reclaiming the space is a separate, deliberate step (ADR 0021).
15. **Derived bytes are swept by marking from the records, never from a
    caller.** Blobs are shared, so nothing frees one implicitly. GC builds its
    mark set from the store's own records — record bodies, the blob a tombstone
    pins, a graph manifest's slices, a vector manifest's shards — and a caller
    cannot narrow it (ADR 0024). The sweep also spares anything younger than a
    grace period, because a blob is uploaded before the record that references
    it commits (ADR 0028).
16. **Gonzalo stores; it does not decide.** The fleet access-control records
    hold identity, role grants and channel configuration, and gonzalo never
    evaluates a role in them — the consumer reads them and decides. `gonzalod`
    keeps authorizing its own API by ADR 0015's token principal, which is
    unrelated to `FleetRole` (ADR 0022, ADR 0023).
17. **Declare what cannot be verified, and fail loudly on a mismatch.** Gonzalo
    never runs the embedder behind a caller's vectors, so a durable vector index
    stores the embedding space and dimension it was opened with and errors
    naming both values when a later open disagrees. Configuration drift is
    caught once, at startup, instead of silently skewing every query after
    (ADR 0027).

## Inviolable invariants

1. **Concurrent edits are never silently lost** — the core invariant. A
   stale-parent write MUST return `Conflict`, never overwrite; ambiguous merges
   MUST surface (ADR 0005).
2. **Conflict is a typed, recoverable result** — `PutResult::Conflict`, never
   collapsed into `GonzaloError` (ADR 0005).
3. **All persistence funnels through the one `Record`/`Store`** — no parallel
   typed store re-implements versioning (ADR 0002).
4. **Substrates implement only the generic `Store` and stay type-blind** — no
   substrate-specific escape hatches (ADR 0004, 0008).
5. **Every `Store` implementation must pass the shared conformance suite**
   (ADR 0006).
6. **Capability layers never bypass or mutate the core** — a layer may register
   a `RecordKind` + merge class at most (ADR 0008, 0010, 0011).
7. **`Sync` reuses the exact local-write conflict/merge machinery** — any
   `Store` can be a sync peer (ADR 0005).
8. **The daemon's two transports derive from one canonical schema + one service
   layer** (`gonzalo-proto`) (ADR 0007).
9. **The code graph is NEVER keyed by `(repo, path)`** — two-level keying;
   slices are content-addressed, path-agnostic, and stored raw, and resolution
   tolerates missing targets (ADR 0012).
10. **No query engine ever sits under the `Store` substrate** — engines back
    only regenerable index layers, never the durable source of truth (ADR 0012).
11. **`unsafe_code` is forbidden workspace-wide** (ADR 0009).
12. **License is AGPL-3.0-only** (ADR 0003).
13. **The core does no I/O** — `gonzalo-core` is pure logic; all I/O lives in
    substrates (design spec §3).
14. **Every write carries provenance identity** — an `Identity`; `Meta` records
    `author` and `origin_system` (design spec §4, §9). The store also stamps
    `created` and `updated` inside the same critical section that decides the
    write, overwriting whatever the client sent: times a caller can set are
    times a caller can lie about. A replication write keeps the source's times,
    because syncing a record is not an edit.
15. **ADRs are an append-only log** — superseded, never deleted (ADR 0001).
16. **A consumer read never shows a tombstone** — `get` and `list` hide them.
    Only the raw surface exposes a tombstone, and that is the surface
    replication uses; a deleted key can never reappear in a listing (ADR 0021,
    ADR 0025).
17. **A tombstone pins the deleted record's blob** until the tombstone itself is
    collected, so a delete never destroys content a peer can still sync back
    (ADR 0024).
18. **The daemon's surface stays described, not merely implemented** — the
    served-operation table is checked against the router's own source and
    against the published OpenAPI document, in both directions, so an operation
    can be neither served without a description nor described without being
    served (ADR 0007, ADR 0020).
