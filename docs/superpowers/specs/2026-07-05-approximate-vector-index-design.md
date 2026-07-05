# Approximate vector index backend (hnsw_rs)

- **Ticket:** gonzalo#9
- **Date:** 2026-07-05
- **Status:** Accepted
- **Records:** ADR 0014
- **Refs:** `crates/gonzalo-vector/src/{lib,index}.rs`, ADR 0008 (capability layers),
  `docs/superpowers/specs/2026-06-05-gonzalo-design.md` §12 (usearch vs hnsw_rs)

## Problem

`gonzalo-vector` ships only `MemoryVectorIndex` — an exact, brute-force cosine
kNN over an in-memory `HashMap`. Query is O(n) per call. The `VectorIndex` trait
was designed (async, keyed by `RecordKey`) so an approximate/scalable backend
could be added without breaking callers. This adds that backend and settles the
spec's open question of which in-process ANN crate to default to.

## Crate decision: hnsw_rs

`usearch` and `hnsw_rs` were weighed against the trait's actual requirements:

- **Deletion.** The trait requires `remove`, and the knowledge store (#29)
  actively calls it to evict orphaned chunks on re-ingest. `usearch` has native
  removal; `hnsw_rs` has **none**.
- **Build footprint.** `usearch` bundles a C++ library (needs a C++ toolchain and
  adds build time; its FFI `unsafe` is internal to the dependency, so our crates
  stay `forbid(unsafe)`-clean). `hnsw_rs` is **pure Rust** — a clean build with no
  toolchain.

We choose **hnsw_rs** for the pure-Rust build and accept owning a small
deletion layer (below). A head-to-head benchmark (recall/latency) accompanies the
decision.

## Design

### Shipped backend — `HnswVectorIndex` (feature `hnsw`)

`MemoryVectorIndex` remains the exact default. `HnswVectorIndex` lives behind a
non-default `hnsw` feature (optional `hnsw_rs` dependency). CI runs
`--all-features`, so the backend and its tests compile and run there — fine,
`hnsw_rs` is pure Rust.

All state sits behind one `Mutex<Inner>`:

```
Inner {
    hnsw: Hnsw<'static, f32, DistCosine>,
    next_id: usize,
    key_to_id: HashMap<RecordKey, usize>,        // live keys → current id
    id_to_entry: HashMap<usize, (RecordKey, Vec<f32>)>, // live ids → key + vector
    dim: Option<usize>,
    tombstones: usize,                            // dead ids still in the graph
}
```

Vectors are retained in `id_to_entry` for exact re-scoring and rebuilds.

### RecordKey ↔ id bimap

hnsw_rs keys by `usize`. `key_to_id`/`id_to_entry` bridge to `RecordKey`. Ids are
monotonic (`next_id`); an id is never reused within a graph generation.

### KeyPrefix filtering + scoring

hnsw cannot filter by metadata, so `query`:
1. over-fetches from hnsw (`knbn = k·OVERFETCH + tombstones`, `ef ≥ knbn`),
2. maps each returned id via `id_to_entry` (an id absent there is tombstoned →
   skip),
3. applies `filter.matches(key)`,
4. recomputes **exact cosine** on the ≤k survivors (truthful `Match.score`),
5. sorts by descending score, ties broken by `RecordKey` (as `MemoryVectorIndex`),
6. truncates to `k`.

Because the backend is approximate and results are tombstone/filter-reduced,
`query` may return fewer than `k` matches. This is documented on the impl.

### Deletion via tombstone + rebuild

hnsw_rs has no delete or in-place update, so:

- **`remove(key)`** — drop from `key_to_id`/`id_to_entry`, `tombstones += 1`. The
  orphaned vector remains in the graph but is invisible (queries skip ids not in
  `id_to_entry`).
- **`upsert` on an existing key** — tombstone the old id, insert the new vector
  under a fresh id, repoint `key_to_id`.
- **Rebuild** — when `tombstones > live && tombstones > 64`, rebuild the graph
  from the live `id_to_entry` with fresh contiguous ids and reset `tombstones`.
  This bounds graph bloat under the knowledge store's re-ingest churn. Rebuild is
  O(n) inserts, amortized across many mutations.

### Dimension handling & errors

Mirror `MemoryVectorIndex`: the first insert fixes `dim`; a mismatched `upsert`
or `query` returns `CoreError::Backend("vector dimension mismatch: …")`.

## Benchmark — excluded crate `crates/gonzalo-vector-bench`

The full head-to-head lives in a crate **excluded from `[workspace]`**, so
`cargo … --workspace --all-features` (what CI runs) never builds it and usearch's
C++ never reaches CI. It depends on `gonzalo-vector` (`hnsw`), `hnsw_rs`,
`usearch`, and `criterion`, and measures on ~10k × 384-dim random vectors:

- **Recall@10** vs brute-force ground truth (per backend),
- **Insert throughput** and **single-query latency** (per backend).

Run manually: `cargo bench` from inside the crate. Results are recorded in the
crate README and summarized here after the run. If `usearch` will not build
locally, the fallback is hnsw_rs-only numbers plus the documented rationale
above.

## Testing

Feature-gated (`hnsw`) tests mirroring the exact index's behavior, plus the
deletion-specific cases:

- nearest-first ordering on well-separated vectors,
- `k` limits results; oversized `k` returns all live,
- namespace `KeyPrefix` filtering,
- dimension-mismatch error on `upsert` and `query`,
- `remove` drops the key from results; removing an absent key is ok,
- re-`upsert` of a key reflects the new vector (old tombstoned),
- **rebuild correctness:** after enough removes to trigger a rebuild, remaining
  entries still query correctly and removed ones stay gone.

## Scope boundaries (YAGNI)

- In-process CPU index only; no remote/on-disk backend.
- Single default ANN crate (hnsw_rs); usearch appears only in the excluded bench.
- No dynamic hnsw parameter tuning surface beyond sensible construction defaults.

## Acceptance criteria

- [ ] `HnswVectorIndex` implements `VectorIndex` behind the `hnsw` feature;
      `MemoryVectorIndex` stays the default.
- [ ] `remove` honored via tombstone + bounded rebuild; re-upsert updates.
- [ ] `KeyPrefix` filtering + exact cosine re-scoring on results.
- [ ] Feature-gated tests incl. rebuild correctness pass under `--all-features`.
- [ ] Head-to-head benchmark (recall/latency) in the excluded bench crate, with
      results documented.
- [ ] ADR 0014 records the crate choice and deletion strategy.
