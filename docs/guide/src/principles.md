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
   feature (ADR 0009).
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
    `author` and `origin_system` (design spec §4, §9).
15. **ADRs are an append-only log** — superseded, never deleted (ADR 0001).
