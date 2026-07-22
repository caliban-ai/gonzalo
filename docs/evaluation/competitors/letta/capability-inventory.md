# Letta (MemGPT) — documented-capability inventory

**Static snapshot**, captured **2026-07-19**, from the public surface of
[`github.com/letta-ai/letta`](https://github.com/letta-ai/letta),
[`letta.com`](https://www.letta.com/), and the MemGPT paper. **Letta**
(formerly **MemGPT**, from UC Berkeley's Sky Computing Lab) is an Apache-2.0
platform for building **stateful agents** with persistent, transparent memory.
Its defining idea is an OS-inspired memory hierarchy exposed to the model as
editable **memory blocks**; an agent created once retains its full state —
memory, history, tools, model settings — indefinitely.

## 1. What it is

An **agent runtime**, not just a store: it owns the agent loop (reasoning, tool
calls, multi-step execution) and bundles memory management into a persistent
server process. This is the key framing difference from a storage substrate.

## 2. Memory model

- **OS-inspired hierarchy**: a small in-context **main / core memory**, plus
  **recall** storage (conversation history) and **archival** storage
  (long-term, vector-searchable).
- **Memory blocks**: labeled, editable units of core memory; the model rewrites
  them in its normal loop (self-editing memory).
- **Self-editing via tools**: the agent calls memory tools to insert, replace,
  and evict content across tiers.
- **Sleep-time / background memory**: memory can be curated and improved
  out-of-band over time.
- **Transparency**: memory state is inspectable rather than hidden.

## 3. Storage & infrastructure

- **Backend**: PostgreSQL + pgvector (archival/vector); SQLite for lightweight
  local runs.
- **Agent-scoped state**: each agent has its own core + archival store; state is
  persisted server-side and survives restarts.

## 4. APIs, SDKs & tooling

- **REST API** exposed by the Letta server.
- **Python SDK** and **TypeScript SDK**.
- **ADE (Agent Development Environment)**: a visual tool to inspect/debug agent
  memory and state.
- **Tools & MCP**: built-in tool calling and Model Context Protocol tool
  integration.
- **Multi-agent**: agents and subagents; shared memory blocks across agents.

## 5. Models

- Model-agnostic; works across providers (Anthropic, OpenAI, and others),
  selectable per agent.

## 6. Deployment

| Mode | Notes |
|---|---|
| Self-hosted server | Official Docker image (`letta/letta`); the app server hosts agents |
| Local | Agents can run fully on the user's machine (Letta Code, 2026) |
| Cloud | Managed cloud platform |
| Desktop / CLI | Desktop app and **Letta Code** CLI |

## 7. 2026 developments

- **Letta Code** (April 2026): a locally-running, deeply personalized agent
  runtime with **git-backed memory**, skills, and subagents that work across
  model providers.

## Licensing

Apache-2.0 (open-source platform); a managed cloud is also offered.

---

**Note:** This inventory records Letta's documented surface. Letta is an agent
*framework*; the companion [`parity-gap-matrix.md`](parity-gap-matrix.md) maps
each row against gonzalo, whose scope is the persistence layer *beneath* an
agent rather than the agent loop itself.
