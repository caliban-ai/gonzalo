# gonzalo ↔ Zep / Graphiti parity gap matrix

> **What this is:** a living checklist mapping [Graphiti/Zep's documented
> surface](capability-inventory.md) against gonzalo. Graphiti models memory as
> a **temporal knowledge graph**; gonzalo models it as versioned records with a
> graph as one optional capability. Many Graphiti rows are therefore 🔴 `by
> design` — a graph-first, LLM-extraction memory *policy* that sits above
> gonzalo's substrate — not unbuilt features.
>
> **How to use it:** when a capability lands, tick its row(s) 🔴 → 🟡 or
> 🟡 → ✅ in the same PR that ships the code, and cite the ADR in Notes.
>
> **Companion document:** [`capability-inventory.md`](capability-inventory.md)
> — the dated snapshot this matrix is derived from. Refresh both together.

**Legend:** ✅ parity · 🟡 partial · 🔴 gap · `by design` = intentionally
out of scope for gonzalo's substrate layer, not a backlog item.

**Last refreshed:** 2026-07-19 (initial baseline against Graphiti/Zep snapshot
2026-07-19).

---

## A. Core operations

| Capability | Gonzalo | Notes |
|---|---|---|
| Ingest data (`add_episode`) | 🟡 | Gonzalo takes typed record `put`s, not free-form episodes; there is no LLM ingestion that derives a graph from text |
| Incremental updates (no full recompute) | ✅ | Writes are per-record; index layers (vector/graph) update incrementally, never rebuild the store (ADR 0008, 0012) |
| Hybrid search (semantic + keyword + graph) | 🟡 | Vector recall (ADR 0014) and a code graph (ADR 0012) exist as separate layers; no unified hybrid ranker |
| Graph-distance reranking | 🔴 | No reranking stage |
| Node / edge queries with custom types | 🟡 | Code-graph nodes/edges are queryable (ADR 0012); no user-defined conversational entity ontology |
| Group management (`group_id`) | 🟡 | Namespaces provide scoping (ADR 0015); no first-class group abstraction |

## B. Temporal model

| Capability | Gonzalo | Notes |
|---|---|---|
| Bi-temporal facts (validity windows) | 🔴 `by design` | Gonzalo tracks transaction-time revision lineage (`revision`, `parent`), not valid-time (ADR 0002) |
| Automatic fact invalidation | 🔴 `by design` | Superseded facts would be a domain-layer concern; the core supersedes by revision, not by closing a valid-time window |
| Episode provenance | ✅ | Every write carries an `Identity`; `Meta` records `author` + `origin_system` — provenance is intrinsic, not graph-specific |
| Point-in-time ("as of") queries | 🟡 | Revision history + git substrate allow reconstructing prior transaction-time state; no valid-time "as of" semantics |
| Superseded data never destroyed | ✅ | Revision lineage is append-only; git history is immutable (ADR 0016); ADRs themselves follow the same append-only rule (ADR 0001) |

## C. Storage backend

| Capability | Gonzalo | Notes |
|---|---|---|
| Runs with no external database | ✅ | Filesystem is the zero-dependency default — Graphiti *requires* Neo4j / FalkorDB / Neptune (ADR 0004) |
| Graph database backend | 🔴 `by design` | Gonzalo's graph is a regenerable **index layer** over the record store; a hard invariant forbids a query engine sitting *under* the store as the source of truth (ADR 0012) |
| Pluggable backends | ✅ | fs / git / S3 / remote-daemon behind one `Store`; backend is configuration (ADR 0004, 0009) |
| Every backend conformance-tested | ✅ | Shared conformance suite each `Store` must pass (ADR 0006) |

## D. Pluggable components

| Capability | Gonzalo | Notes |
|---|---|---|
| Swappable LLM providers | 🔴 `by design` | No LLM in gonzalo's loop |
| Swappable embedders | ✅ | `Embedder` trait; local Candle embedder ships (ADR 0013) |
| Custom entity / edge ontology | 🟡 | New capabilities register a `RecordKind` (ADR 0008, 0010); the code-graph model is fixed, not a user ontology |
| Structured-output dependence | ✅ (n/a) | Gonzalo has no LLM extraction step, so it inherits none of Graphiti's small-model reliability caveats |

## E. Interfaces & deployment

| Capability | Gonzalo | Notes |
|---|---|---|
| Library embedding | ✅ | Single `gonzalo` facade crate (ADR 0009) |
| REST service | ✅ | HTTP/JSON over the daemon (ADR 0007) |
| gRPC service | ✅ | Second transport over one canonical schema (ADR 0007) |
| MCP server | 🔴 | No MCP surface today; a candidate for a thin adapter over the daemon |
| Python library | 🔴 | Rust only; a client could be generated from the daemon schema |
| Self-hosted, fully open | ✅ | AGPL-3.0 daemon is the whole product — no managed tier held back; Zep's dashboarded experience is Cloud-only (commercial) |
| Managed cloud + compliance (SOC 2 / HIPAA) | 🔴 `by design` | Not offered; gonzalo is self-hosted |

---

## Where gonzalo leads (not in Graphiti/Zep's surface)

| Capability | Gonzalo | Notes |
|---|---|---|
| Typed concurrent-write conflicts | ✅ | Stale-parent write returns `PutResult::Conflict`, never silently overwrites (ADR 0005) — Graphiti manages a single logical graph, not multi-writer conflict resolution |
| Multi-writer sync | ✅ | `Sync` reuses the exact local-write conflict/merge machinery; any `Store` can be a peer (ADR 0005, 0016, 0017) |
| Git-backed, mergeable history | ✅ | Commit-per-write substrate + three-way content merge on non-ff pull (ADR 0016, 0017) |
| No graph DB to operate | ✅ | Zero-dependency fs default vs a required Neo4j/FalkorDB/Neptune deployment (ADR 0004) |
| Store-is-source-of-truth invariant | ✅ | Query engines back only regenerable index layers, never the durable truth (ADR 0012) |

---

## Reading of the gap

Zep/Graphiti is ahead precisely where it is opinionated: **true temporal
reasoning** (bi-temporal edges, automatic invalidation) and **LLM-driven graph
construction** from raw episodes. Both are 🔴 `by design` for gonzalo — a
temporal-KG memory *policy* could be built as a capability layer *over*
gonzalo's records rather than adopted as the foundation. The rows that read as
a real backlog are: an **MCP adapter** and **Python client** (E), and
optionally a **hybrid ranker** unifying vector + graph recall (A). Graphiti's
hard requirement of an external graph database is exactly the coupling
gonzalo's substrate model is built to avoid.

## Refresh process

1. When a capability lands: tick the relevant row(s) here in the same PR and
   cite the ADR in Notes.
2. When Graphiti/Zep ships something new: refresh
   [`capability-inventory.md`](capability-inventory.md) first (re-fetch the
   upstream docs), then propagate new rows here.
3. Bump the **Last refreshed** date.
