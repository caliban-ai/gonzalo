# gonzalo ↔ Mem0 parity gap matrix

> **What this is:** a living checklist mapping [Mem0's documented
> surface](capability-inventory.md) against gonzalo. Use it to see, per
> capability, whether gonzalo reaches parity — and, importantly, which rows
> gonzalo leaves 🔴 *by design* because they belong to a memory *policy* that
> sits above gonzalo's storage substrate.
>
> **How to use it:** when a capability lands, tick its row(s) 🔴 → 🟡 or
> 🟡 → ✅ in the same PR that ships the code, and cite the ADR in Notes.
>
> **Companion document:** [`capability-inventory.md`](capability-inventory.md)
> — the dated snapshot of Mem0's public surface this matrix is derived from.
> Refresh both together.

**Legend:** ✅ parity · 🟡 partial · 🔴 gap · `by design` = intentionally
out of scope for gonzalo's substrate layer, not a backlog item.

**Last refreshed:** 2026-07-19 (initial baseline against Mem0 snapshot
2026-07-19).

---

## A. Core memory API

| Capability | Gonzalo | Notes |
|---|---|---|
| Store an item (`add`) | ✅ | `Store::put(record, expected_parent_rev)`; typed records rather than extracted facts (ADR 0002) |
| Retrieve relevant items (`search`) | ✅ | Vector capability layer (`Embedder` + `VectorIndex`) and knowledge store resolve queries back to whole records (ADR 0008, 0011, 0014) |
| Fetch by id (`get` / `get_all`) | ✅ | `Store::get` / list over the record store (ADR 0002) |
| Update an item (`update`) | ✅ | A `put` against the current revision; stale parent yields `Conflict` (ADR 0005) |
| Delete (`delete`) | ✅ | `Store::delete`, OCC-aware, propagated by `Sync` (ADR 0018) |
| Change history (`history`) | ✅ | Revision lineage (`revision`, `parent`) per record; git substrate gives commit-per-write history (ADR 0002, 0016) |
| `reset` whole namespace | 🟡 | Achievable by clearing a store root / namespace; no single first-class "reset" call |
| Scope filters (`user`/`agent`/`run`) | 🟡 | Records carry `Meta` + `links`; namespace scoping exists (ADR 0015), but no built-in user/session/run memory scoping model |

## B. Memory model

| Capability | Gonzalo | Notes |
|---|---|---|
| LLM-driven fact extraction | 🔴 `by design` | Gonzalo stores caller-supplied typed records; it runs no LLM extraction/summarization pipeline. This is a memory *policy* that belongs above the substrate |
| Add/update/delete fact reconciliation | 🔴 `by design` | Same rationale; reconciliation logic would be a layer over gonzalo, not in it |
| Multi-level (user / session / agent) memory | 🟡 | Modelable via record `kind` + namespaces (ADR 0015), but not a first-class memory-scope abstraction |
| Typed memory views | ✅ | `MemoryTier`, `Topic`, `Session`, `Checkpoint` are serde views over `Record` (`gonzalo-domain`) |
| Graph memory (entity relationships) | 🟡 | Ships a tree-sitter **code** graph keyed by `RecordKey` (ADR 0012); not a general conversational entity graph |

## C. Retrieval

| Capability | Gonzalo | Notes |
|---|---|---|
| Semantic / vector recall | ✅ | Exact cosine + approximate index backends (ADR 0013, 0014) |
| BM25 keyword ranking | 🔴 | No keyword ranker today |
| Fusion (semantic + keyword + entity) | 🔴 | Gonzalo does vector recall only; no score fusion |
| Temporal reasoning (time-aware ranking) | 🔴 `by design` | No valid-time model; revision lineage is transaction-time only |
| Results resolve to first-class records | ✅ | Invariant: retrieval returns whole records via the `Store`, never bare ids (ADR 0008, 0011) |

## D. Pluggable components

| Capability | Gonzalo | Notes |
|---|---|---|
| Swappable embedders | ✅ | `Embedder` trait; local Candle embedder ships (ADR 0013) |
| Swappable vector stores | 🟡 | `VectorIndex` trait with in-memory exact + approximate backends; not yet a Qdrant-class external adapter |
| Swappable LLMs | 🔴 `by design` | Gonzalo has no LLM in its loop |
| Graph store | 🟡 | `gonzalo-graph` / `GraphStore` over records (ADR 0012) |
| Relational history store | ✅ | History is intrinsic to the record model + substrate; no separate DB required |

## E. SDKs & interfaces

| Capability | Gonzalo | Notes |
|---|---|---|
| Library embedding | ✅ | Single `gonzalo` facade crate; capabilities behind Cargo features (ADR 0009) |
| Python SDK | 🔴 | Rust only today; a client could be generated from the daemon schema |
| JS / TS SDK | 🔴 | As above |
| REST API | ✅ | HTTP/JSON over the daemon (ADR 0007) |
| gRPC API | ✅ | Second transport over one canonical schema (ADR 0007) — Mem0 has no gRPC surface |
| CLI | ✅ | `gonzalo` admin/ops CLI (`list`/`get`/`status`/`migrate`/`sync`, `ticket …`) |
| Hosted dashboard | 🔴 `by design` | No managed platform; gonzalo is self-hosted, AGPL-3.0 |

## F. Deployment

| Capability | Gonzalo | Notes |
|---|---|---|
| Embed with no external DB | ✅ | Filesystem is the zero-dependency default (ADR 0004) — Mem0's library still brings a vector store |
| Self-hosted server | ✅ | `gonzalod` daemon, optional bearer auth (ADR 0007, 0015) |
| Managed cloud | 🔴 `by design` | Not offered |
| Pluggable storage backends | ✅ | fs / git / S3 / remote-daemon, all behind one `Store`; backend is configuration, not code (ADR 0004, 0009) |

---

## Where gonzalo leads (not in Mem0's surface)

These are gonzalo capabilities Mem0's documented surface does not cover — the
substance of the different-layer positioning:

| Capability | Gonzalo | Notes |
|---|---|---|
| Typed concurrent-write conflicts | ✅ | Stale-parent write returns `PutResult::Conflict`, never silently overwrites — the core invariant (ADR 0005) |
| Multi-writer sync | ✅ | `Sync` reuses the exact local-write conflict/merge machinery; any `Store` can be a peer (ADR 0005, 0016, 0017) |
| Git-backed, mergeable history | ✅ | Commit-per-write substrate with three-way content merge on non-fast-forward pull (ADR 0016, 0017) |
| Backend-as-configuration | ✅ | fs → git → S3 → daemon is a config change, not a rewrite (ADR 0004, 0009) |
| Per-write provenance | ✅ | Every write carries an `Identity`; `Meta` records `author` + `origin_system` |
| Executable substrate contract | ✅ | Every `Store` implementation must pass the shared conformance suite (ADR 0006) |

---

## Reading of the gap

Mem0 is ahead on **LLM-facing memory ergonomics**: automatic fact extraction,
fusion retrieval, temporal ranking, and a Python/TS-first SDK with a hosted
platform. Most of those are 🔴 `by design` for gonzalo — they are a memory
*policy* that a caller (or a thin layer) would run *over* gonzalo, which
supplies the durable, shareable, conflict-aware substrate underneath. The rows
worth treating as a genuine backlog (not positioning) are: **BM25 / fusion
retrieval** (C), an **external vector-store adapter** (D), and **Python/TS
client SDKs** generated from the daemon schema (E).

## Refresh process

1. When a capability lands: tick the relevant row(s) here in the same PR and
   cite the ADR in Notes.
2. When Mem0 ships something new: refresh
   [`capability-inventory.md`](capability-inventory.md) first (re-fetch the
   upstream docs), then propagate new rows here.
3. Bump the **Last refreshed** date.
