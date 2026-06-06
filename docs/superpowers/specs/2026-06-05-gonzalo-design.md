# Gonzalo — Design Spec

- **Date:** 2026-06-05
- **Status:** Draft (awaiting review)
- **Repo:** `caliban-ai/gonzalo`
- **License:** AGPL-3.0-only (matches caliban)
- **Edition / toolchain:** Rust 2024, rust-version 1.95 (matches caliban)

## 1. Purpose

Gonzalo is a robust persistence layer for [caliban](https://github.com/caliban-ai/caliban),
the from-scratch Rust agent harness. Today caliban stores its durable state —
**memory tiers**, **auto-memory topics**, **sessions** (conversation
transcripts), and **checkpoints** — as local-first files on a single machine.
Gonzalo lifts that state into a layer that can be **shared across multiple
systems and contributors** without losing caliban's local-first ergonomics.

Gonzalo is consumed two ways:

1. **As a library** — a set of Rust crates caliban links in-process (the default).
2. **As an optional daemon** — a standalone server other systems (and non-Rust
   tools) can share, reachable over **gRPC or HTTP/JSON**.

### Goals

- Unified persistence for memory + sessions + checkpoints.
- **Pluggable storage substrates** behind one trait: local filesystem, git,
  S3-compatible object store, and a remote-daemon client.
- **Versioned, conflict-aware** writes so concurrent edits by multiple
  contributors are never silently lost.
- **Vector search** and a **code graph** as first-class retrieval capability
  layers over the persistence core.
- Drop-in default behavior for caliban: local-fs substrate mirrors today's
  on-disk layout, so going shared is configuration, not code.

### Non-goals (for the core design; may come later)

- A general-purpose database for arbitrary third-party apps. Caliban is the
  first and primary consumer; the model is shaped by its data.
- Fine-grained RBAC / multi-tenant policy engine. Daemon auth starts simple
  (token + namespace scope) and is designed to be extended later.
- Replacing caliban's provider plumbing. Gonzalo stays provider-agnostic;
  embedding generation is delegated by default (see §7).

## 2. Architecture

```
                 ┌─────────────────────────────────────────────┐
   caliban  ───► │  gonzalo (facade)  — typed API caliban calls │
                 └───────────────┬─────────────────────────────┘
        ┌────────────────────────┼───────────────────────────┐
        ▼                        ▼                            ▼
 ┌─────────────┐        ┌──────────────┐            ┌──────────────────┐
 │ domain layer│        │ vector layer │            │  code-graph layer│
 │ memory/     │        │ Embedder +   │            │  symbols/refs/   │
 │ sessions/   │        │ VectorIndex  │            │  GraphStore      │
 │ checkpoints │        │ traits       │            │  (tree-sitter)   │
 └──────┬──────┘        └──────┬───────┘            └────────┬─────────┘
        └───────────────┬──────┴─────────────────────────────┘
                        ▼
              ┌───────────────────────┐
              │  gonzalo-core         │  Record model, Store + Sync traits,
              │  (no I/O)             │  revisions, conflict types, identity
              └───────────┬───────────┘
        ┌─────────┬───────┴───────┬──────────┬─────────────┐
        ▼         ▼               ▼          ▼             ▼
      fs        git          s3/object   server-client   (in-proc)
   substrate  substrate      substrate    substrate
                        ▲
                        │
              ┌─────────┴──────────┐
              │  gonzalo-server    │  optional daemon: exposes core +
              │  (gRPC + HTTP/JSON)│  vector + graph over the network
              └────────────────────┘
```

**Key principle:** caliban only ever sees the **facade** and the **typed
layers**. Whether those are backed by local files, a git repo, an S3 bucket, or
a remote Gonzalo daemon is *configuration*, not API. The daemon is itself just
another deployment that wraps the same core.

### Core-model decision (Approach A)

The core defines one uniform persisted unit (a `Record`) and a generic `Store`
trait over it. Caliban's data types become **typed views** in a domain layer
built on top. Substrates only ever implement the generic `Store` — they never
know about caliban's types.

This was chosen over (B) typed stores per domain (which would re-implement
versioning/conflict/sync per type × per substrate — combinatorial) and (C) a
schemaless JSON-document store (which discards the type safety that motivates
using Rust). Approach A writes the hard parts — versioning, optimistic
concurrency, conflict surfacing, sync — **once**.

## 3. Crate map

Cargo workspace; all crates prefixed `gonzalo-` except the facade. Heavy
dependencies (git2, aws-sdk, tonic) live only in their owning substrate crate
and are feature-gated, so a filesystem-only build stays lean.

**Foundation**

- **`gonzalo-core`** — `Record` model, `Store` + `Sync` traits, `Revision`/
  version types, `Conflict` + merge primitives, `Identity`/provenance, error
  types. Pure logic, no I/O backends. Defines the substrate **conformance test
  suite** (see §10).

**Storage substrates** (each implements `gonzalo-core::Store`)

- **`gonzalo-store-fs`** — local filesystem; mirrors caliban's current on-disk
  layout. Reference impl + zero-dependency default.
- **`gonzalo-store-git`** — files in a git repo; commit/push/pull/merge for
  shareable, auditable history.
- **`gonzalo-store-s3`** — S3-compatible object store for large/cheap shared
  blobs (sessions, checkpoints).
- **`gonzalo-store-server`** — substrate that proxies to a remote Gonzalo
  daemon (the "central server" backend, client side). Speaks gRPC or HTTP/JSON.

**Capability layers** (beside the domain layer, over core)

- **`gonzalo-vector`** — `Embedder` trait (caller-delegating default +
  self-hosted option) and `VectorIndex` trait; feature-gated impls (in-process
  HNSW, sqlite-vec, remote Qdrant).
- **`gonzalo-graph`** — code-graph model (symbols, files, references, edges), a
  `tree-sitter`-based builder, a `GraphStore` trait + query API; impls over
  sqlite and the remote daemon.

**Domain**

- **`gonzalo-domain`** — typed APIs over core for caliban's data: `memory`
  (tiers + auto-memory topics), `sessions`, `checkpoints`. Maps typed structs ↔
  `Record`s via serde.

**Network / process**

- **`gonzalo-proto`** — shared wire types + serialization for the daemon:
  protobuf (`prost`/`tonic`) for gRPC and serde types for HTTP/JSON, both
  generated/derived from one canonical schema so the two transports stay in
  lockstep.
- **`gonzalo-server`** — the optional daemon binary; wires core + vector +
  graph + identity/auth and serves it over **both** a `tonic` gRPC service and
  an `axum` HTTP/JSON service sharing one core service layer.
- **`gonzalo-cli`** — admin/ops binary: `init`, `sync`, `status`, `migrate`,
  `inspect`, conflict resolution.

**Facade**

- **`gonzalo`** — thin re-export crate giving caliban one dependency and a
  curated public surface; selects substrates/layers via Cargo features.

**Total: 12 crates.** Each has a single responsibility.

## 4. Data model

The universal persisted unit:

```rust
struct Record {
    key:      RecordKey,          // { namespace, collection, id } — stable address
    kind:     RecordKind,         // MemoryTier | Topic | Session | Checkpoint | …
    revision: Revision,           // monotonic counter + content hash
    parent:   Option<Revision>,   // the revision this edit was based on (for OCC)
    body:     Body,               // inline bytes OR a content-addressed blob ref
    meta:     Meta,               // author, origin_system, created, updated, labels
    links:    Vec<RecordKey>,     // typed relations (topic→session, etc.)
}
```

- **`RecordKey`** is the stable address used everywhere — including by the
  vector and graph layers — so semantic/structural queries return first-class
  records.
- **`Body`** is either inline bytes (small records) or a content-addressed blob
  reference (large sessions/checkpoints). Content addressing means sync
  transfers only changed blobs.
- **`Meta.author`** is the contributor `Identity`; `origin_system` records
  which machine produced the edit. Together they give provenance for every
  change.

## 5. Consistency & conflict model

**Versioned records + optimistic concurrency + explicit conflict surfacing.**

- Writes are **conditional**: `put(record, expected_parent_rev)`. If the
  current stored revision ≠ the expected parent, the store returns
  `Conflict { base, theirs, yours }` rather than overwriting.
- The core ships **merge strategies keyed by `RecordKind`**:
  - **Append-only kinds** (auto-memory topics, session transcripts) auto-merge
    by union / concatenation.
  - **Structured kinds** attempt a field-level 3-way merge against `base`.
  - **Anything ambiguous** is surfaced to the caller and to `gonzalo-cli` for
    resolution. **Nothing is ever silently lost.**
- `Conflict` is a typed, recoverable result variant — not a generic error.

## 6. Sync

`Sync` reconciles a local replica with a remote: **pull → detect divergence →
apply the §5 merge strategy → push**, reusing the exact same conflict
machinery as local writes. Content-addressed bodies mean only changed blobs
move over the wire. Sync is substrate-agnostic: any `Store` can be a sync peer.

## 7. Vector layer

- **`Embedder` trait** — pluggable. The default delegates embedding generation
  to the caller (caliban, which already talks to model providers); a
  self-hosted embedder is an opt-in feature so the core stays provider-agnostic.
- **`VectorIndex` trait** — `upsert(key, vector, metadata)` and
  `query(vector, k, filter)`. Embeddings are linked to records by `RecordKey`,
  so semantic search returns first-class records.
- **Impls** (feature-gated): in-process HNSW, sqlite-vec, remote Qdrant.

## 8. Code-graph layer

- A `tree-sitter`-based builder parses a workspace into **symbols, files,
  references, and edges**.
- The graph is **versioned like any other data** (so it syncs and is
  shareable across contributors).
- Queryable **structurally** (`callers_of`, `defines`, `references`) and —
  joined with the vector layer via shared `RecordKey`s — **semantically**.
- `GraphStore` trait with impls over sqlite and the remote daemon.

## 9. Daemon, identity & auth

- **Daemon (`gonzalo-server`)** exposes the core + vector + graph over **two
  transports operators can choose between**:
  - **gRPC** (`tonic`) — strongly typed, streaming for large transfers, codegen
    for the client substrate.
  - **HTTP/JSON** (`axum`) — simple, curl-able, friendly to non-Rust tools.
  Both sit on one shared core service layer; `gonzalo-proto` holds the canonical
  schema both derive from, keeping them in lockstep.
- **Identity** — every write carries an `Identity` (the contributor). In
  library/local mode this is a configured local identity; in daemon mode the
  server authenticates it.
- **Auth (daemon mode)** — token-based (bearer / API key) to start, with a
  `RecordKey`-namespace-scoped permission check. Designed so finer-grained
  policy can slot in later without touching the core.

## 10. Error handling & testing

- **Errors** — `thiserror` per crate; the facade exposes one `GonzaloError`
  enum. `Conflict` is a typed, recoverable variant, not a generic error.
- **Substrate conformance suite** — `gonzalo-core` defines one shared test
  suite that **every** `Store` impl must pass; it is run against fs, git, s3,
  and the server substrate. This is how backends are kept honest.
- **Property tests** for merge/conflict logic.
- **Integration** — `wiremock` / testcontainers for s3 and the daemon.
- **TDD throughout**, per project convention.

## 11. Caliban integration & migration

- Caliban swaps its direct file I/O in `caliban-memory`, `caliban-sessions`,
  and `caliban-checkpoint` for the `gonzalo` facade.
- Because `gonzalo-store-fs` mirrors the existing on-disk layout, **default
  behavior is unchanged** — pointing at git / s3 / a daemon becomes pure
  configuration.
- **`gonzalo-cli migrate`** imports existing caliban data into Gonzalo records;
  idempotent and dry-run-able.

## 12. Open questions / future work

- Exact on-disk record encoding for `gonzalo-store-fs` (must round-trip with
  caliban's current layout) — pinned down during M-fs implementation.
- Choice of default in-process vector index crate (usearch vs hnsw_rs) —
  benchmarked during the vector milestone.
- Daemon discovery/config UX in caliban (how an operator points caliban at a
  remote Gonzalo) — settled alongside caliban integration.

## 13. Build order (informative)

Although this spec covers the whole system, implementation will proceed in
milestones, each with its own plan:

1. `gonzalo-core` + `gonzalo-store-fs` + `gonzalo-domain` + `gonzalo` facade —
   local parity with caliban today, behind the new abstractions.
2. `gonzalo-store-git` and `gonzalo-store-s3` + `Sync`.
3. `gonzalo-proto` + `gonzalo-server` (gRPC + HTTP/JSON) + `gonzalo-store-server`.
4. `gonzalo-vector`.
5. `gonzalo-graph`.
6. `gonzalo-cli` (migrate/sync/inspect) + caliban integration.
