//! ANN head-to-head: `hnsw_rs` (the shipped backend) vs `usearch`, on recall@10
//! versus brute-force ground truth plus build and per-query latency (#9/ADR
//! 0014). Deterministic (seeded LCG) so runs are comparable.
//!
//! Run: `cd crates/gonzalo-vector-bench && cargo run --release`.

use std::time::Instant;

use hnsw_rs::prelude::{DistCosine, Hnsw};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

const N: usize = 10_000; // corpus size
const Q: usize = 200; // queries
const DIM: usize = 384; // all-MiniLM dimension
const K: usize = 10; // top-k for recall

/// Tiny deterministic LCG → uniform f32 in [-1, 1).
struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((self.0 >> 33) as f32 / (1u64 << 31) as f32) - 1.0
    }
    fn vector(&mut self) -> Vec<f32> {
        (0..DIM).map(|_| self.next_f32()).collect()
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// Brute-force top-K ids by cosine (ground truth).
fn brute_top_k(corpus: &[Vec<f32>], query: &[f32]) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = corpus
        .iter()
        .enumerate()
        .map(|(i, v)| (i, cosine(query, v)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    scored.into_iter().take(K).map(|(i, _)| i).collect()
}

fn recall(approx: &[usize], truth: &[usize]) -> f32 {
    let hits = approx.iter().filter(|id| truth.contains(id)).count();
    hits as f32 / truth.len() as f32
}

fn main() {
    println!("corpus N={N}, dim={DIM}, queries={Q}, k={K}\n");

    let mut rng = Lcg(0x9E3779B97F4A7C15);
    let corpus: Vec<Vec<f32>> = (0..N).map(|_| rng.vector()).collect();
    let queries: Vec<Vec<f32>> = (0..Q).map(|_| rng.vector()).collect();
    let truth: Vec<Vec<usize>> = queries.iter().map(|q| brute_top_k(&corpus, q)).collect();

    // ---- hnsw_rs ---------------------------------------------------------
    let t = Instant::now();
    let hnsw = Hnsw::<f32, DistCosine>::new(16, N, 16, 200, DistCosine);
    for (i, v) in corpus.iter().enumerate() {
        hnsw.insert((v, i));
    }
    let hnsw_build = t.elapsed();

    let t = Instant::now();
    let mut hnsw_recall = 0.0f32;
    for (q, truth) in queries.iter().zip(&truth) {
        let ids: Vec<usize> = hnsw.search(q, K, 64).iter().map(|n| n.d_id).collect();
        hnsw_recall += recall(&ids, truth);
    }
    let hnsw_query = t.elapsed() / Q as u32;
    hnsw_recall /= Q as f32;

    // ---- usearch ---------------------------------------------------------
    let t = Instant::now();
    let opts = IndexOptions {
        dimensions: DIM,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        ..Default::default()
    };
    let index = Index::new(&opts).expect("usearch index");
    index.reserve(N).expect("reserve");
    for (i, v) in corpus.iter().enumerate() {
        index.add(i as u64, v).expect("add");
    }
    let us_build = t.elapsed();

    let t = Instant::now();
    let mut us_recall = 0.0f32;
    for (q, truth) in queries.iter().zip(&truth) {
        let ids: Vec<usize> = index
            .search(q, K)
            .expect("search")
            .keys
            .iter()
            .map(|k| *k as usize)
            .collect();
        us_recall += recall(&ids, truth);
    }
    let us_query = t.elapsed() / Q as u32;
    us_recall /= Q as f32;

    // ---- report ----------------------------------------------------------
    println!("{:<10} {:>12} {:>14} {:>12}", "backend", "build", "query (avg)", "recall@10");
    println!("{}", "-".repeat(52));
    println!(
        "{:<10} {:>12} {:>14} {:>11.1}%",
        "hnsw_rs",
        format!("{:.2?}", hnsw_build),
        format!("{:.2?}", hnsw_query),
        hnsw_recall * 100.0
    );
    println!(
        "{:<10} {:>12} {:>14} {:>11.1}%",
        "usearch",
        format!("{:.2?}", us_build),
        format!("{:.2?}", us_query),
        us_recall * 100.0
    );
}
