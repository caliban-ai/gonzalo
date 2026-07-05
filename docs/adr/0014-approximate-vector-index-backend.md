# ADR 0014 · Approximate vector index backend (hnsw_rs)

- **Status:** accepted
- **Date:** 2026-07-05
- **Source:** [`docs/superpowers/specs/2026-07-05-approximate-vector-index-design.md`](../superpowers/specs/2026-07-05-approximate-vector-index-design.md)

## Context

`gonzalo-vector` shipped only `MemoryVectorIndex`, an exact brute-force cosine
kNN (O(n) per query). The `VectorIndex` trait (ADR 0008) was designed so an
approximate backend could be added without breaking callers, and the gonzalo
design (§12) left the choice of in-process ANN crate — `usearch` vs `hnsw_rs` —
as an open question to benchmark.

Two forces decided it. First, **deletion**: the trait requires `remove`, and the
knowledge store (#29) calls it to evict orphaned chunks on re-ingest — `usearch`
has native removal, `hnsw_rs` has none. Second, **build footprint**: `usearch`
bundles a C++ library (C++ toolchain, longer builds; its `unsafe` FFI is internal
to the dependency), while `hnsw_rs` is pure Rust. CI builds with
`--all-features --all-targets`, so any feature-gated C++ dependency would still be
compiled in CI.

## Decision

We will add **`HnswVectorIndex`** to `gonzalo-vector`, backed by **`hnsw_rs`**
(pure Rust), behind a non-default **`hnsw`** feature. `MemoryVectorIndex` remains
the exact default.

- **Deletion** — `hnsw_rs` cannot delete or update in place, so we own a
  tombstone-and-rebuild layer: `remove` and re-`upsert` tombstone the old id;
  queries skip tombstoned ids; the graph is rebuilt from live entries once
  `tombstones > live && tombstones > 64`.
- **Keying** — a `RecordKey ↔ usize` bimap bridges the trait's keys to hnsw's
  integer ids; vectors are retained for exact re-scoring and rebuilds.
- **Filtering/scoring** — `query` over-fetches from hnsw, drops tombstoned ids,
  applies the `KeyPrefix` filter, and recomputes exact cosine on the survivors
  (so `Match.score` is truthful); it may return fewer than `k`.
- **Benchmark** — a head-to-head `hnsw_rs` vs `usearch` benchmark (recall@10,
  insert/query latency) lives in a crate **excluded from the workspace**, so
  usearch's C++ never reaches CI. Results are documented.

## Consequences

- **Positive:** sub-linear approximate search behind the unchanged trait; a clean
  pure-Rust CI build with no C++ toolchain; `MemoryVectorIndex` still available
  for exact needs and small indexes.
- **Negative:** we own the tombstone/rebuild deletion layer that `usearch` would
  have provided natively; approximate results mean `query` can miss a true
  neighbor or return `<k`; graph memory grows with churn until a rebuild.
- **Revisit if:** deletion churn makes rebuilds too frequent, we need on-disk or
  distributed scale, or recall proves insufficient — at which point `usearch`
  (native delete) or a served index backend should be reconsidered.
