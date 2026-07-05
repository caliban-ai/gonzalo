# Local Candle embedder (`gonzalo-embed`)

- **Ticket:** gonzalo#97 (child of #40; pairs with the vector⋈graph join #30)
- **Date:** 2026-07-05
- **Status:** Accepted
- **Records:** ADR 0013
- **Refs:** ADR 0011 (knowledge store), ADR 0008 (capability layers), #40 local-only constraint

## Problem

`gonzalo-vector` defines an [`Embedder`] trait but ships no real implementation —
only a bag-of-words test embedder. Semantic search (the knowledge store, ADR
0011) therefore has no genuine semantics: cosine similarity tracks word overlap,
not meaning. We need a **local, FOSS** embedder (no cloud inference API, no keys
in the default path — #40) that produces real sentence embeddings on CPU.

## Decisions

1. **Model:** `sentence-transformers/all-MiniLM-L6-v2` — Apache-2.0 (cleanly
   compatible with this AGPL-3.0 project, ADR 0003), 384-dim, 22M params, the
   canonical sentence-embedding default with a well-trodden Candle BERT path.
2. **Acquisition:** download-on-first-use from HuggingFace via `hf-hub`
   (anonymous, no key, cached in the HF cache dir), with a `model_path` override
   for fully-offline/air-gapped use. Downloading weights is not a cloud inference
   API and needs no key, so #40's constraint holds.
3. **Placement:** a **new crate** `gonzalo-embed` that implements the
   `gonzalo-vector` `Embedder` trait. The heavy ML dependencies stay isolated
   there; `gonzalo-vector` remains dependency-light. Matches the
   crate-per-substrate idiom (`gonzalo-store-fs`/`-s3`).

## Design

### Crate & dependencies

`crates/gonzalo-embed`, depending on `gonzalo-vector` (trait) and `gonzalo-core`
(`Result`/`CoreError`). ML deps confined here: `candle-core`, `candle-nn`,
`candle-transformers` (BERT), `tokenizers`, `hf-hub`, `async-trait`.

### Public surface

```rust
pub struct EmbedderConfig {
    pub model_id: String,             // default "sentence-transformers/all-MiniLM-L6-v2"
    pub revision: String,             // default "main" (a pinned commit is recommended)
    pub model_path: Option<PathBuf>,  // Some(dir) → load locally, skip hf-hub (offline)
}
impl Default for EmbedderConfig { /* the defaults above */ }

pub struct CandleEmbedder { /* Arc<{ tokenizer, BertModel, device }> */ }

impl CandleEmbedder {
    /// Resolve weights (path override or hf-hub), load the model + tokenizer
    /// once on CPU. All failures map to `CoreError::Backend`.
    pub async fn load(config: EmbedderConfig) -> Result<Self>;
}

#[async_trait]
impl Embedder for CandleEmbedder {
    /// Tokenize → BERT forward → masked mean-pool → L2-normalize. Returns a
    /// 384-dim unit vector. Runs the sync CPU forward inside `spawn_blocking`.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}
```

### Inference pipeline

- `load()`: if `model_path` is set, load `model.safetensors`, `tokenizer.json`,
  `config.json` from that dir; otherwise fetch them via `hf-hub` (cached). Build
  the `BertModel` on the CPU `Device`. Store tokenizer + model behind an `Arc`
  so `embed` can clone into a blocking task.
- `embed(text)`: tokenize to input ids + attention mask → BERT forward to
  per-token hidden states → **mean-pool** over tokens weighted by the attention
  mask (masked/padding positions excluded) → **L2-normalize** → `Vec<f32>`
  (length 384). The forward pass is synchronous and CPU-bound, so it executes in
  `tokio::task::spawn_blocking` over the `Arc`-shared model to keep the async
  runtime unblocked.

### Error handling

All `candle`, `tokenizers`, `hf-hub`, and IO errors convert to
`CoreError::Backend(String)` — the trait's `Result` type. No `gonzalo-core`
change is required.

### Two pure helpers (the unit-testable core)

- `mean_pool(hidden: &Tensor, attention_mask: &Tensor) -> Result<Tensor>` —
  sum token vectors where mask == 1, divide by the live-token count.
- `l2_normalize(v: &Tensor) -> Result<Tensor>` — divide by the L2 norm.

These carry the embedding-correctness logic and are tested without a model.

## Testing

- **Unit (fast, no network, no model):** `mean_pool` and `l2_normalize` on
  hand-built tensors — normalization yields unit length; mean-pool ignores
  masked positions; a known input yields the known mean. TDD-driven.
- **Ignored integration (`#[ignore]`):** `CandleEmbedder::load(default)` then
  assert `embed()` returns a 384-length vector, ~unit norm, and a semantic
  sanity check (a related pair scores higher than an unrelated pair). Excluded
  from the default `cargo test` (no 90MB download in CI); run locally / opt-in.
- **Manual end-to-end:** run the ignored test once after implementation to prove
  real semantic embeddings work.

## Scope boundaries (YAGNI)

- Single-text `embed` only — no batch API this pass (the trait is single-text;
  batch is a follow-up if perf demands it).
- No `hf-hub` sub-feature flag — the `model_path` override already covers offline
  builds/runtime.
- CPU only; GPU is out of scope.

## Acceptance criteria

- [ ] `gonzalo-embed` crate impls `Embedder` with a real all-MiniLM model on CPU.
- [ ] Weights resolve via hf-hub download or `model_path` override.
- [ ] Masked mean-pool + L2-normalize, 384-dim output; pure helpers unit-tested.
- [ ] Ignored integration test proves real semantic ranking; CI stays green.
- [ ] ADR 0013 records the model/license/acquisition/crate-boundary decision.
