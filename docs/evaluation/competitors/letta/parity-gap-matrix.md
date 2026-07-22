# gonzalo ↔ Letta (MemGPT) parity gap matrix

> **What this is:** a living checklist mapping [Letta's documented
> surface](capability-inventory.md) against gonzalo. The framing gap dominates
> this one: **Letta is an agent runtime**, gonzalo is a **persistence
> substrate**. Whole categories of Letta's surface — the agent loop, tool
> calling, self-editing memory — are 🔴 `by design` for gonzalo, which is a
> plausible durable store *beneath* a Letta-style agent, not a replacement for
> its loop.
>
> **How to use it:** when a capability lands, tick its row(s) 🔴 → 🟡 or
> 🟡 → ✅ in the same PR that ships the code, and cite the ADR in Notes.
>
> **Companion document:** [`capability-inventory.md`](capability-inventory.md)
> — the dated snapshot this matrix is derived from. Refresh both together.

**Legend:** ✅ parity · 🟡 partial · 🔴 gap · `by design` = intentionally
out of scope for gonzalo's substrate layer, not a backlog item.

**Last refreshed:** 2026-07-19 (initial baseline against Letta snapshot
2026-07-19).

---

## A. Agent runtime

| Capability | Gonzalo | Notes |
|---|---|---|
| Agent loop (reasoning + tool calls + multi-step) | 🔴 `by design` | Gonzalo runs no agent loop; it is storage only. A Letta-class agent would sit above it |
| Built-in tool calling | 🔴 `by design` | Not a gonzalo concern |
| MCP tool integration | 🔴 | No MCP surface today (candidate thin adapter over the daemon) |
| Multi-agent / subagents | 🔴 `by design` | Orchestration is above the substrate |
| Model-agnostic provider selection | 🔴 `by design` | No LLM in gonzalo's loop |

## B. Memory model

| Capability | Gonzalo | Notes |
|---|---|---|
| Persistent state that survives restarts | ✅ | Durable records on the chosen substrate (ADR 0002, 0004) |
| Self-editing memory (model rewrites blocks) | 🔴 `by design` | Gonzalo takes caller writes via `put`; it has no autonomous memory-curation policy |
| OS-style tiered memory (core / recall / archival) | 🟡 | `MemoryTier` is a typed view over records (`gonzalo-domain`), but tier *promotion/eviction* policy is not built in |
| Labeled memory blocks | 🟡 | Representable as records with `kind` + `meta`; no block-editing tool protocol |
| Sleep-time / background curation | 🔴 `by design` | A policy layer above gonzalo |
| Transparent / inspectable state | ✅ | Records are plain, inspectable; fs/git substrates make state directly readable on disk |

## C. Storage & infrastructure

| Capability | Gonzalo | Notes |
|---|---|---|
| Runs with no external database | ✅ | Filesystem default — Letta needs Postgres + pgvector (SQLite for light runs) (ADR 0004) |
| Vector / archival search | ✅ | Vector capability layer, exact + approximate index (ADR 0013, 0014) |
| Agent-scoped stores | 🟡 | Namespaces scope stores (ADR 0015), but gonzalo's aim is the opposite: a store *shared* across systems and contributors |
| Pluggable backends | ✅ | fs / git / S3 / remote-daemon behind one `Store` (ADR 0004, 0009) |
| Git-backed memory | ✅ | Commit-per-write git substrate (ADR 0016, 0017) — matches Letta Code's 2026 git-backed-memory direction |

## D. APIs, SDKs & tooling

| Capability | Gonzalo | Notes |
|---|---|---|
| REST API | ✅ | HTTP/JSON over the daemon (ADR 0007) |
| gRPC API | ✅ | Second transport over one canonical schema (ADR 0007) |
| Python SDK | 🔴 | Rust only; a client could be generated from the daemon schema |
| TypeScript SDK | 🔴 | As above |
| Visual dev environment (ADE-equivalent) | 🔴 `by design` | Agent-debugging UI is out of scope; gonzalo ships an admin/ops CLI |
| CLI | ✅ | `gonzalo` CLI: `list`/`get`/`status`/`migrate`/`sync`, `ticket …` |

## E. Deployment

| Capability | Gonzalo | Notes |
|---|---|---|
| Self-hosted server (Docker) | ✅ | `gonzalod` daemon; container published to GHCR (ADR 0007) |
| Local / embedded | ✅ | Embed the `gonzalo` facade with the fs substrate — no server needed |
| Managed cloud | 🔴 `by design` | Not offered |

---

## Where gonzalo leads (not in Letta's surface)

| Capability | Gonzalo | Notes |
|---|---|---|
| Typed concurrent-write conflicts | ✅ | Stale-parent write returns `PutResult::Conflict`, never silently overwrites (ADR 0005) — Letta state is agent-scoped, not multi-writer conflict-managed |
| Multi-writer, shared store | ✅ | Built to lift local-first state into a layer shared across systems and contributors; `Sync` reuses the write-path machinery (ADR 0005, 0016, 0017) |
| Backend-as-configuration | ✅ | fs → git → S3 → daemon is a config change (ADR 0004, 0009); Letta assumes Postgres + pgvector |
| Per-write provenance | ✅ | `Identity` on every write; `Meta` records `author` + `origin_system` |
| Executable substrate contract | ✅ | Shared conformance suite each `Store` must pass (ADR 0006) |

---

## Reading of the gap

Letta and gonzalo barely overlap as products: Letta is a **complete stateful-agent
framework** (loop, tools, self-managing memory, ADE), and almost all of that is
🔴 `by design` for gonzalo. Where they *do* meet is durable memory — and there
gonzalo's angle is the inverse of Letta's: **shared, multi-writer, conflict-aware,
backend-agnostic** persistence rather than turnkey per-agent state. The genuine
convergence point is **git-backed memory**, which Letta Code adopted in 2026 and
gonzalo has as a first-class substrate. The realistic backlog items here are
**client SDKs** (D) and an optional **MCP adapter** (A) — everything else is a
deliberate boundary, not a gap. The clean summary: gonzalo is a candidate
archival substrate *for* a Letta-style agent, not a competitor to its loop.

## Refresh process

1. When a capability lands: tick the relevant row(s) here in the same PR and
   cite the ADR in Notes.
2. When Letta ships something new: refresh
   [`capability-inventory.md`](capability-inventory.md) first (re-fetch the
   upstream docs), then propagate new rows here.
3. Bump the **Last refreshed** date.
