# ADR 0011 · Knowledge store over the capability layers

- **Status:** accepted
- **Date:** 2026-06-14
- **Source:** [#16](https://github.com/caliban-ai/gonzalo/issues/16)

## Context

Caliban needs a "what do we know about X" retrieval surface that spans its
durable record kinds — memory tiers, auto-memory topics, sessions, and now
tickets (ADR 0010) — backed by semantic (vector) and, later, structural (graph)
retrieval. The ingredients already exist: `gonzalo-vector` (`Embedder` +
`VectorIndex`), the `gonzalo-domain` typed views, and `gonzalo-graph` — all keyed
by the shared `RecordKey` (ADR 0008). What is missing is a surface that composes
them. Without one, every caller re-wires embed → index → fetch by hand, and there
is no single place that decides which record kinds are knowledge-bearing or how
their text is extracted.

ADR 0008 also flagged that the shipped exact in-memory `VectorIndex` does not
scale; a real corpus needs a production index. That decision has been deferred
until there was a consumer for it — the knowledge store is that consumer.

## Decision

Add a `gonzalo-knowledge` capability crate (facade feature `knowledge`) that
composes the existing layers; `gonzalo-core` does not change.

- **Surface.** `KnowledgeStore<S: Store, V: VectorIndex, E: Embedder>` with:
  - `ingest(key)` — fetch the `Record`, extract its knowledge text, embed it via
    `E`, and `upsert` it into `V` under the *same* `RecordKey`.
  - `query(text, k, filter)` — embed the query, `VectorIndex::query` for the
    top-`k` keys, then resolve them through `S` to first-class records.
  - Results are `Hit { record, score }` — first-class records, per ADR 0008's
    principle that retrieval returns records, not bare ids.
- **Knowledge-bearing kinds** (extracted via `gonzalo-domain` views):
  `MemoryTier` (content), `Topic` (slug + bullets), `Session` (name + turn text),
  `Ticket` (title + body + labels), `TicketEvent` (body). `Checkpoint` is **not**
  knowledge-bearing. This mapping lives in one function, `knowledge_text`.
- **Chunking.** Phase 1 embeds one document per record. Per-kind chunking
  (a session by turn, a long tier by section) is a future refinement behind the
  same surface.
- **First production index.** Adopt **sqlite-vec** as the zero-infra default
  (a single file, no daemon — matching the `fs`-default ethos), with **Qdrant**
  as the daemon-side option. Both slot behind the existing `VectorIndex` trait,
  feature-gated per ADR 0009. These impls are follow-ups; the exact in-memory
  index remains the default until they land.
- **Composition.** Because hits key off `RecordKey`, `vector ⋈ graph` is a
  query-time intersection — rank by similarity, then filter/expand by a
  code-graph neighborhood addressed by the same key. The crate ships the vector
  path now; the graph-backed filter lands with a `GraphStore` join.

## Consequences

- **Positive:** One retrieval surface instead of hand-wired embed/index/fetch;
  composes by `RecordKey`, so tickets, memory, and sessions are searched
  uniformly; embedding stays provider-agnostic (delegated to `E`); zero core
  change.
- **Negative:** `KnowledgeStore` is generic over three type parameters
  (`S`, `V`, `E`) — some signature heft. The in-memory index still caps scale
  until sqlite-vec lands. `knowledge_text` couples `gonzalo-knowledge` to the
  domain view shapes (a new knowledge-bearing kind must be added there).
- **Revisit if:** a knowledge-bearing kind needs chunking the one-document model
  can't express; or the generic composition blocks a cross-store optimization a
  unified impl would allow.
