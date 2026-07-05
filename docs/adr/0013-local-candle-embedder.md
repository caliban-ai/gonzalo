# ADR 0013 · Local Candle embedder for real semantic embeddings

- **Status:** accepted
- **Date:** 2026-07-05
- **Source:** [`docs/superpowers/specs/2026-07-05-local-candle-embedder-design.md`](../superpowers/specs/2026-07-05-local-candle-embedder-design.md)

## Context

`gonzalo-vector` defines an `Embedder` trait (ADR 0008) but ships only a
bag-of-words test embedder, so the knowledge store's semantic search (ADR 0011)
has no real semantics. We need a genuine sentence embedder, subject to #40's
standing constraint: **FOSS, local-only — no cloud embedding APIs, no keys in
the default path.**

The decisions in play were: which model (all-MiniLM-L6-v2 vs bge-small vs
gte-small — weighing MTEB quality, parameter count, and license compatibility
with this AGPL-3.0 project per ADR 0003); how weights are acquired
(download-on-first-use vs user-provided path vs bundling ~90MB in the crate);
and where the implementation lives (a heavy ML dependency set behind a feature
in `gonzalo-vector`, or isolated in a dedicated crate).

## Decision

We will add a new crate **`gonzalo-embed`** implementing the `gonzalo-vector`
`Embedder` trait with a **local CPU** embedder built on Candle
(`candle-transformers` BERT + `tokenizers`).

- **Model:** `sentence-transformers/all-MiniLM-L6-v2` — **Apache-2.0**
  (redistributable, compatible with AGPL-3.0), 384-dim.
- **Acquisition:** download-on-first-use from HuggingFace via `hf-hub`
  (anonymous, no key, cached), with an `EmbedderConfig.model_path` override for
  fully-offline use. Downloading weights is not a cloud inference API and needs
  no key, so #40's constraint holds.
- **Pipeline:** tokenize → BERT forward → masked mean-pool → L2-normalize →
  384-dim unit vector; the sync CPU forward runs inside `spawn_blocking`.
- **Boundary:** the ML dependencies live only in `gonzalo-embed`;
  `gonzalo-vector` stays dependency-light. Matches the crate-per-substrate idiom
  (`gonzalo-store-fs`/`-s3`).

Errors map to `CoreError::Backend`. Scope is single-text `embed` on CPU; batch
and GPU are explicitly out of scope.

## Consequences

- **Positive:** real semantic retrieval behind the existing trait — no caller
  change (the knowledge store just gets a better `Embedder`). ML weight is
  quarantined in one opt-in crate. License is cleanly compatible and local-only
  is preserved.
- **Negative:** first use downloads ~90MB (unless `model_path` is set); Candle +
  tokenizers + hf-hub are a heavy dependency set for that crate; CPU inference is
  slower than a hosted API. Real-model tests must be `#[ignore]`d so CI does not
  download weights.
- **Revisit if:** we need batch/GPU throughput, a different model (quality or
  size), or the download-on-first-use default proves problematic in air-gapped
  deployments (promote `model_path`/a bundled option to the default).
