# ADR 0008 · Domain, vector, and graph as capability layers over core

- **Status:** accepted
- **Date:** 2026-06-13

## Context

Caliban needs more than key-value persistence: typed access to its own data,
semantic (vector) retrieval, and structural (code-graph) queries. These could
have been built into the core or into each substrate, but that would entangle
retrieval concerns with storage and force every substrate to reimplement them.

## Decision

Keep `gonzalo-core` storage-only and add capabilities as **layers over it**,
each keyed by the shared `RecordKey`:

- `gonzalo-domain` — typed views (`MemoryTier`, `Topic`, `Session`,
  `Checkpoint`) mapped to/from `Record` via serde.
- `gonzalo-vector` — `Embedder` + `VectorIndex` traits. Embedding generation
  delegates to the caller by default (the core stays provider-agnostic). What
  shipped in M4 is an **exact in-memory cosine** index; approximate/external
  indexes (HNSW, sqlite-vec, Qdrant) anticipated by the design spec remain
  future, feature-gated impls.
- `gonzalo-graph` — a `tree-sitter`-based Rust code graph (`build_rust`) and a
  `GraphStore` trait over symbols / files / references / edges.

Because every layer keys off `RecordKey`, semantic and structural queries return
first-class records, and the layers compose (vector ⋈ graph) by shared key.

## Consequences

- **Positive:** Storage and retrieval stay decoupled — substrates never know
  about vectors or graphs. The shared `RecordKey` makes the layers composable
  and keeps query results first-class. Provider-agnostic embedding keeps the
  core free of model dependencies.
- **Negative:** The shipped exact in-memory vector index does not scale to large
  corpora; production-scale retrieval will need the not-yet-built approximate
  indexes. Two retrieval layers add API surface beyond plain persistence.
- **Revisit if:** corpus size outgrows the exact in-memory index (prioritize a
  real ANN impl), or a capability needs core/substrate support that a pure layer
  cannot provide.
