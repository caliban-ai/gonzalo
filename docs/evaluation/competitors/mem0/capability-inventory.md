# Mem0 — documented-capability inventory

**Static snapshot**, captured **2026-07-19**, from Mem0's public surface:
[`github.com/mem0ai/mem0`](https://github.com/mem0ai/mem0),
[`docs.mem0.ai`](https://docs.mem0.ai/introduction), and
[`mem0.ai`](https://mem0.ai/). Mem0 ("mem-zero") is an Apache-2.0 memory layer
for AI agents: after each interaction an LLM extracts the durable facts, stores
them, and on the next call retrieves only the relevant ones to inject into the
prompt. It ships as a library, a self-hosted server, and a hosted platform
(Y Combinator S24).

## 1. Core memory API

| Operation | Purpose |
|---|---|
| `add` | Extract and store memories from a conversation or direct input (LLM-driven fact extraction, with add/update/delete reconciliation against existing memories) |
| `search` | Retrieve the top-k relevant memories for a query, filtered by scope (`user_id` / `agent_id` / `run_id`) |
| `get` / `get_all` | Fetch stored memory entries |
| `update` | Modify an existing memory |
| `delete` / `delete_all` | Remove memories |
| `history` | Inspect the change history of a memory |
| `reset` | Clear memory state |

## 2. Memory model

- **Multi-level memory**: user-, session-, and agent-scoped state, with adaptive
  personalization.
- **LLM-extracted facts**: durable facts (preferences, decisions, context) are
  distilled from raw conversation rather than stored verbatim; an "add" pass
  reconciles new facts against existing ones (add / update / no-op / delete).
- **Graph memory** (optional): relationships between entities captured in a graph
  store, layered beside the vector store.

## 3. Retrieval

- **Hybrid / fusion retrieval**: semantic (vector) + BM25 keyword + entity
  matching, scored in parallel and fused.
- **Temporal reasoning**: time-aware ranking of the right dated instance for
  "current state", "past events", and "upcoming plans" queries.
- **Scoped filters**: retrieval constrained by `user_id` / `agent_id` / `run_id`
  and metadata.

## 4. Pluggable components

- **LLMs**: OpenAI (default, `gpt-5-mini`) plus a broad supported-LLM list.
- **Embedders**: default `text-embedding-3-small`; alternatives supported.
- **Vector stores**: Qdrant is the documented default; many others supported.
- **Graph store**: optional graph backend for graph memory.
- **History store**: relational (e.g. Postgres) for change history.

## 5. SDKs & interfaces

- **Python**: `pip install mem0ai` (optional `[nlp]` extra).
- **JavaScript / TypeScript**: `npm install mem0ai`.
- **REST API** + hosted **dashboard** on the platform; per-user API keys and
  request audit logs on the self-hosted server.
- **CLI** available via npm/pip.

## 6. Deployment

| Mode | Setup | Notes |
|---|---|---|
| Library (OSS) | `pip` / `npm install` | Embeds in the app; brings its own vector store |
| Self-hosted server | Docker Compose | Dashboard, auth, per-user API keys, request audit logs |
| Cloud platform | `app.mem0.ai` | Fully managed, zero-ops |

## 7. Integrations

- Framework and coding-assistant integrations (e.g. Claude Code, Cursor,
  Windsurf) and an "OpenMemory" MCP surface.

## Licensing

Apache-2.0 (OSS library and self-hosted server); the cloud platform is a
commercial managed service.

---

**Note:** This inventory captures the documented surface, not internal
behavior. Retrieval-quality claims (e.g. benchmark numbers) are the vendor's;
this file records *what* is offered, and the companion
[`parity-gap-matrix.md`](parity-gap-matrix.md) maps each row against gonzalo.
