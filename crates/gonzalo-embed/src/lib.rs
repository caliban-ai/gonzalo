//! Local CPU sentence-embedding [`Embedder`](gonzalo_vector::Embedder) for
//! gonzalo, built on Candle + `all-MiniLM-L6-v2` (ADR 0013).
//!
//! [`CandleEmbedder::load`] resolves the model weights (via an
//! [`EmbedderConfig::model_path`] override, else a one-time anonymous `hf-hub`
//! download) and loads the BERT model + tokenizer once on CPU. [`embed`] then
//! tokenizes, runs the forward pass, masked-mean-pools the token states, and
//! L2-normalizes to a 384-dim unit vector. The synchronous CPU forward runs
//! inside `spawn_blocking` so it never blocks the async runtime.
//!
//! [`embed`]: CandleEmbedder::embed

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use candle_core::{D, DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use gonzalo_core::{CoreError, Result};
use gonzalo_vector::Embedder;
use tokenizers::Tokenizer;

/// Configuration for [`CandleEmbedder::load`].
#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    /// HuggingFace model id. Default `sentence-transformers/all-MiniLM-L6-v2`.
    pub model_id: String,
    /// Model revision (branch, tag, or commit). Default `main`.
    pub revision: String,
    /// If set, load `model.safetensors`/`tokenizer.json`/`config.json` from this
    /// directory instead of downloading — fully offline.
    pub model_path: Option<PathBuf>,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            model_id: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            revision: "main".to_string(),
            model_path: None,
        }
    }
}

struct Inner {
    tokenizer: Tokenizer,
    model: BertModel,
    device: Device,
}

/// A local CPU sentence embedder (Candle + all-MiniLM). Cheap to clone.
#[derive(Clone)]
pub struct CandleEmbedder {
    inner: Arc<Inner>,
}

impl CandleEmbedder {
    /// Resolve the weights, tokenizer, and config, then load the model on CPU.
    /// All failures surface as [`CoreError::Backend`].
    pub async fn load(config: EmbedderConfig) -> Result<Self> {
        let (weights, tokenizer_path, config_path) = match &config.model_path {
            Some(dir) => (
                dir.join("model.safetensors"),
                dir.join("tokenizer.json"),
                dir.join("config.json"),
            ),
            None => {
                let api = hf_hub::api::tokio::ApiBuilder::new()
                    .build()
                    .map_err(backend)?;
                let repo = api.repo(hf_hub::Repo::with_revision(
                    config.model_id.clone(),
                    hf_hub::RepoType::Model,
                    config.revision.clone(),
                ));
                (
                    repo.get("model.safetensors").await.map_err(backend)?,
                    repo.get("tokenizer.json").await.map_err(backend)?,
                    repo.get("config.json").await.map_err(backend)?,
                )
            }
        };

        let device = Device::Cpu;
        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(backend)?;
        let cfg: Config = serde_json::from_slice(&std::fs::read(&config_path).map_err(backend)?)
            .map_err(backend)?;
        // Safe (non-mmap) load — the workspace forbids `unsafe`.
        let tensors = candle_core::safetensors::load(&weights, &device).map_err(backend)?;
        let vb = VarBuilder::from_tensors(tensors, DTYPE, &device);
        let model = BertModel::load(vb, &cfg).map_err(backend)?;

        Ok(Self {
            inner: Arc::new(Inner {
                tokenizer,
                model,
                device,
            }),
        })
    }
}

impl Inner {
    /// The synchronous embedding pipeline: tokenize → forward → masked mean-pool
    /// → L2-normalize → a 384-dim unit vector.
    fn embed_blocking(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self.tokenizer.encode(text, true).map_err(backend)?;
        let ids = Tensor::new(encoding.get_ids(), &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(backend)?;
        let attention_mask = Tensor::new(encoding.get_attention_mask(), &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(backend)?;
        let token_type_ids = ids.zeros_like().map_err(backend)?;

        let hidden = self
            .model
            .forward(&ids, &token_type_ids, Some(&attention_mask))
            .map_err(backend)?;

        let pooled = mean_pool(&hidden, &attention_mask).map_err(backend)?;
        let normalized = l2_normalize(&pooled).map_err(backend)?;
        normalized
            .squeeze(0)
            .and_then(|t| t.to_vec1::<f32>())
            .map_err(backend)
    }
}

#[async_trait]
impl Embedder for CandleEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let inner = self.inner.clone();
        let text = text.to_string();
        tokio::task::spawn_blocking(move || inner.embed_blocking(&text))
            .await
            .map_err(backend)?
    }
}

/// Masked mean-pooling: average the per-token hidden states `(batch, seq,
/// hidden)` over the sequence, counting only positions where `attention_mask`
/// `(batch, seq)` is 1. Padding/masked positions are excluded.
fn mean_pool(hidden: &Tensor, attention_mask: &Tensor) -> candle_core::Result<Tensor> {
    let mask = attention_mask.to_dtype(DType::F32)?.unsqueeze(2)?; // (b, s, 1)
    let summed = hidden.broadcast_mul(&mask)?.sum(1)?; // (b, h)
    let counts = mask.sum(1)?; // (b, 1)
    summed.broadcast_div(&counts)
}

/// L2-normalize each row of `(batch, hidden)` to unit length.
///
/// The norm is floored at a tiny epsilon so a zero/degenerate pooled row
/// divides by a small positive value instead of `0.0`, yielding a finite
/// (near-zero) vector rather than emitting `NaN`.
fn l2_normalize(v: &Tensor) -> candle_core::Result<Tensor> {
    let norm = v.sqr()?.sum_keepdim(D::Minus1)?.sqrt()?.maximum(1e-12f64)?; // (b, 1)
    v.broadcast_div(&norm)
}

/// Map any backend/model/tokenizer/IO error into [`CoreError::Backend`].
fn backend<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Backend(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_normalize_yields_unit_length() {
        // A 3-4-5 right triangle row: (3, 4) normalizes to (0.6, 0.8).
        let v = Tensor::from_vec(vec![3f32, 4.0], (1, 2), &Device::Cpu).unwrap();
        let out = l2_normalize(&v)
            .unwrap()
            .squeeze(0)
            .unwrap()
            .to_vec1::<f32>()
            .unwrap();
        assert!((out[0] - 0.6).abs() < 1e-5);
        assert!((out[1] - 0.8).abs() < 1e-5);
        let len = (out[0] * out[0] + out[1] * out[1]).sqrt();
        assert!((len - 1.0).abs() < 1e-5);
    }

    #[test]
    fn l2_normalize_zero_row_is_finite_not_nan() {
        // A degenerate all-zero pooled row must not divide by zero and emit NaN.
        let v = Tensor::from_vec(vec![0f32, 0.0], (1, 2), &Device::Cpu).unwrap();
        let out = l2_normalize(&v)
            .unwrap()
            .squeeze(0)
            .unwrap()
            .to_vec1::<f32>()
            .unwrap();
        assert!(out.iter().all(|x| x.is_finite()), "output must be finite");
    }

    #[test]
    fn mean_pool_ignores_masked_positions() {
        // Two real tokens [1,1] and [3,3] plus a padding token [100,100] that
        // the mask (1,1,0) must exclude. Mean of the real tokens is [2,2].
        let hidden = Tensor::from_vec(
            vec![1f32, 1.0, 3.0, 3.0, 100.0, 100.0],
            (1, 3, 2),
            &Device::Cpu,
        )
        .unwrap();
        let mask = Tensor::from_vec(vec![1u32, 1, 0], (1, 3), &Device::Cpu).unwrap();
        let out = mean_pool(&hidden, &mask)
            .unwrap()
            .squeeze(0)
            .unwrap()
            .to_vec1::<f32>()
            .unwrap();
        assert!((out[0] - 2.0).abs() < 1e-5);
        assert!((out[1] - 2.0).abs() < 1e-5);
    }

    // Real-model check — excluded from default `cargo test` (downloads ~90MB on
    // first run). Run with `cargo test -p gonzalo-embed -- --ignored`.
    #[tokio::test]
    #[ignore = "downloads the all-MiniLM model on first run"]
    async fn real_model_embeds_and_ranks_semantically() {
        let embedder = CandleEmbedder::load(EmbedderConfig::default())
            .await
            .unwrap();
        let anchor = embedder.embed("the cat sat on the mat").await.unwrap();
        assert_eq!(anchor.len(), 384, "all-MiniLM produces 384-dim vectors");
        let len: f32 = anchor.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((len - 1.0).abs() < 1e-3, "output is L2-normalized");

        let related = embedder.embed("a kitten naps on a rug").await.unwrap();
        let unrelated = embedder.embed("the diesel engine roared").await.unwrap();
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!(
            cos(&anchor, &related) > cos(&anchor, &unrelated),
            "a semantically related sentence should score higher"
        );
    }
}
