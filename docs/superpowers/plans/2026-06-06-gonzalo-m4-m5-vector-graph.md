# Gonzalo M4 + M5 — Vector & Code-Graph Layers (Design & Build Notes)

- **Date:** 2026-06-06
- **Status:** Implemented on `main`
- Implements design spec §7 (vector) and §8 (code graph).

## M4 — `gonzalo-vector`

The vector-search capability layer over the persistence core. Embeddings are
keyed by `RecordKey`, so semantic search returns first-class record keys.

- **`Embedder`** trait (`async fn embed(&self, text) -> Result<Vec<f32>>`) — the
  seam for embedding generation. The default deployment delegates this to the
  caller (caliban, which talks to model providers); a self-hosted embedder can
  implement the same trait.
- **`VectorIndex`** trait — `upsert(key, vector)`, `remove(key)`,
  `query(vector, k, filter: &KeyPrefix) -> Vec<Match>`. Async so remote/
  approximate backends can implement it later.
- **`MemoryVectorIndex`** — the first impl: an exact brute-force **cosine** kNN
  over an in-memory map, with `KeyPrefix` filtering, dimension validation, and
  deterministic tie-breaking. Zero external dependencies.

**Decision (accepted recommendation):** ship the exact index first. At
agent-memory scale (thousands of vectors) exact search is correct and fast, and
it avoids an approximate-index dependency. HNSW / sqlite-vec / Qdrant backends
are deferred behind the same trait.

## M5 — `gonzalo-graph`

The code-graph capability layer.

- **Model** (`CodeGraph`, `Symbol`, `Reference`, `SymbolKind`) — serializable,
  so a graph can be persisted as a record and synced like any other data.
- **`build_rust(file, src)`** — a `tree-sitter` + `tree-sitter-rust` builder
  that walks the parse tree to extract symbol definitions (functions, structs,
  enums, traits, impls, modules, consts, statics, type aliases) and a
  **name-based** reference/call graph (each `call_expression` becomes a
  `Reference` tagged with its enclosing function).
- **`GraphStore`** trait + **`InMemoryGraphStore`** — structural queries:
  `symbols_in_file`, `definitions`, `references_to`, `callers_of`.

**Decisions (accepted recommendations):**
- **Rust first** (it's caliban's language); the model/store are
  language-agnostic so more tree-sitter grammars slot in later.
- References are **unresolved (name-based)**, not a fully resolved call graph —
  a useful navigation heuristic. True name resolution is deferred.
- Persisting the graph into `Record`s for sync/sharing is supported by the
  serializable model but the wiring into the domain layer is left for the
  caliban-integration milestone.

## Verification

Both crates are unit-tested (vector: 9 tests incl. an `Embedder` exercise and
dimension/ordering checks; graph: 6 tests over a Rust sample). `cargo clippy
--workspace -D warnings` and `cargo fmt --check` clean. Both are exposed via
facade features (`vector`, `graph`).
