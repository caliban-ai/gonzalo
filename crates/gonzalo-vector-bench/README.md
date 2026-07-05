# gonzalo-vector-bench

ANN head-to-head for the approximate vector index (#9 / ADR 0014): **hnsw_rs**
(the shipped backend) vs **usearch**, on recall and latency.

This crate is **excluded from the gonzalo workspace** (see the root `Cargo.toml`
`exclude`). `usearch` bundles a C++ library; keeping the bench out of
`--workspace` means CI's `--all-features --all-targets` never compiles it. It is
its own standalone workspace.

## Run

```bash
cd crates/gonzalo-vector-bench
cargo run --release
```

Deterministic (seeded LCG), so runs are comparable. Measures, over `N=10000`
vectors of `dim=384` with `Q=200` queries and `k=10`:

- **build** — time to insert the whole corpus,
- **query (avg)** — mean single-query latency,
- **recall@10** — overlap of the approximate top-10 with brute-force ground truth.

## Results (2026-07-05, Apple Silicon, `--release`)

| backend | build  | query (avg) | recall@10 |
|---------|--------|-------------|-----------|
| hnsw_rs | 14.5 s | 729 µs      | 68.4 %    |
| usearch | 11.3 s | 370 µs      | 64.0 %    |

**Reading these numbers.** The two crates are close: `usearch` builds and queries
somewhat faster; `hnsw_rs` had marginally higher recall on this data. Both are
easily fast enough for gonzalo's scale.

The **absolute recall looks low** because the corpus is *uniform-random* vectors:
in 384 dimensions random points are nearly equidistant (curse of dimensionality),
so the "true" top-10 are near-ties separated by negligible cosine gaps, and any
ANN index "misses" some of them without meaningfully worse results. Real
embeddings (e.g. all-MiniLM output, #97) are clustered and recall much higher
(typically >95%). The benchmark's value here is the **relative** comparison on
identical data, not the absolute recall figure.

## Conclusion

`hnsw_rs` is performance-competitive with `usearch`, so choosing it for the
**pure-Rust build** (no bundled C++ / no toolchain in CI) costs little. The one
real tradeoff is deletion: `hnsw_rs` has no delete/update, which the shipped
`HnswVectorIndex` covers with a tombstone-and-rebuild layer (ADR 0014).
