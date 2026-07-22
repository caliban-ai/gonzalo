# Zep / Graphiti — documented-capability inventory

**Static snapshot**, captured **2026-07-19**, from the public surface of
[`github.com/getzep/graphiti`](https://github.com/getzep/graphiti),
[`getzep.com`](https://www.getzep.com/), and the Zep paper
([arXiv:2501.13956](https://arxiv.org/abs/2501.13956)). **Graphiti** is an
Apache-2.0 engine for building real-time, temporally-aware knowledge graphs
for AI agents; **Zep** is the memory platform built on it, with **Zep Cloud**
as the commercial managed offering. The older self-hosted **Zep Community
Edition has been deprecated** — self-hosting today means running Graphiti plus
your own graph database.

## 1. What it is

A **temporal knowledge-graph** engine: it ingests conversational and structured
data as *episodes*, extracts entities and relationships via an LLM, and
maintains a graph where every fact carries validity time. New information that
contradicts an existing fact **invalidates** the old fact (closes its validity
window) rather than deleting it, so history stays queryable.

## 2. Core operations

| Operation | Purpose |
|---|---|
| `add_episode` | Ingest a text or JSON episode; incrementally update the graph (no full recompute) |
| Episode retrieve / delete | Manage ingested episodes |
| Hybrid search | Semantic embeddings + BM25 keyword + graph traversal across entities and relationships |
| Reranking | Graph-distance-based reranking of results |
| Node / edge queries | Search via predefined recipes and custom entity/edge types |
| Group management | Organize related data into groups (`group_id`) |

## 3. Temporal model

- **Bi-temporal tracking**: explicit validity windows (when a fact became true /
  ceased to be true), distinct from ingestion time.
- **Automatic fact invalidation**: contradicting facts close the prior fact's
  window; nothing is destroyed.
- **Episode provenance**: every derived fact traces back to the raw source
  episode.
- **Point-in-time queries**: ask what was true at any moment.

## 4. Storage backends (graph databases)

- Neo4j (5.26+)
- FalkorDB (1.1.2+, including embedded FalkorDB Lite)
- Amazon Neptune (+ OpenSearch Serverless)
- Kuzu 0.11.2 (deprecated upstream)

A graph database is **required**; there is no zero-dependency mode.

## 5. Pluggable components

- **LLM providers**: OpenAI (default), Anthropic, Google Gemini, Groq, Azure
  OpenAI, and OpenAI-compatible endpoints (DeepSeek, Together, OpenRouter,
  Ollama, vLLM, llama.cpp, LM Studio). Works best with structured-output-capable
  models.
- **Embedders**: OpenAI default plus alternatives.
- **Custom ontology**: developer-defined entity/edge types via Pydantic models
  (prescribed ontology) or emergent structure (learned ontology).

## 6. Interfaces & deployment

- **Python library** (Python 3.10+; 3.12+ for embedded FalkorDB).
- **MCP server**: Model Context Protocol integration for assistants (Claude,
  Cursor).
- **REST service**: FastAPI-based API (`server/`).
- **Self-hosted**: open-source engine on your own infrastructure + graph DB.
- **Zep Cloud**: managed context-graph infrastructure with dashboard,
  governance, hosted retrieval, and SOC 2 Type 2 / HIPAA compliance
  (commercial).

## 7. Operational notes

- Default LLM concurrency is conservative (`SEMAPHORE_LIMIT=10`) to avoid
  rate-limit errors.
- Anonymous telemetry on by default; disableable via environment variable.
- Reported to outperform Mem0 on LongMemEval (vendor benchmark).

## Licensing

Graphiti: Apache-2.0. Zep Cloud: commercial managed service. Community Edition:
deprecated.

---

**Note:** This inventory records Graphiti/Zep's documented surface. Benchmark
and quality claims are the vendor's; the companion
[`parity-gap-matrix.md`](parity-gap-matrix.md) maps each row against gonzalo.
