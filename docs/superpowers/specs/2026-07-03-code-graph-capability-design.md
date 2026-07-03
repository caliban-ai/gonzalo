# Code-graph capability over arbitrary repos — design

- **Date:** 2026-07-03
- **Status:** approved (implementation plan)
- **Ratifies:** [ADR 0012 — Two-level keying for the code graph](../../adr/0012-code-graph-two-level-keying.md)
- **Basis:** design handoff `caliban-gonzalo-code-graph-handoff.md`, verified against
  `gonzalo`@`7e8afff`, `caliban`@`133923b`, `prospero`@`34e20f2` (all `main`).

## Goals

A code graph is a precomputed relationship index: parse source with tree-sitter →
extract nodes (functions, structs, traits…) and edges (calls, imports, impls) →
store in an embedded index with search → resolve call-sites to definitions → expose
as queries. The payoff is substitution: one structural query replaces a dozen
grep/read cycles — fewer tokens, fewer tool calls. It is not LLM magic.

1. **Internal (consumption).** Index the Caliban repo so agents work on it faster.
   Validate the payoff immediately with an off-the-shelf tool (EPIC I).
2. **Product (capability).** Give Caliban a code-graph capability it deploys against
   **arbitrary target repos** it is assigned. The real architectural commitment.

Standing constraint: **FOSS / local-only** — no cloud embedding APIs, no API keys in
the default path.

## Grounded current state

`gonzalo-graph` already exists: `build_rust` runs tree-sitter-rust in-process,
emitting `CodeGraph { symbols, references }`; references are name-based and
**unresolved** (correct layering per ADR 0012, not just unfinished). `Body::Blob`
(content-addressed record bodies) is reserved for a later milestone — this work pulls
it forward. `MergeClass` lacks a `Derived` arm. `gonzalo-server` (`gonzalod`) exposes
`Get/Put/List/TicketSync` over gRPC + HTTP with JSON-in-protobuf-bytes payloads; the
graph is not exposed there. Caliban is a **pure consumer**: no parsing anywhere, a
mature `caliban-mcp-client` (rmcp 1.7) with lazy `ToolSearch` activation (ADR-0046) —
so consuming a graph MCP server is a config entry, zero client code.

Corrections applied from the verification pass (the handoff was otherwise exact):
- Caliban's workspace is `unsafe_code = "deny"` (not `forbid`) — does not change the
  crash-isolation rationale (a C grammar abort is uncatchable either way).
- The path-agnostic fix (A3) covers **both** `Symbol` and `Reference` — both carry a
  `file` field today.
- Checkpoint pre-images live under `blobs/<sha256>.bin` (the content-addressed
  *shape* — the load-bearing point — holds; only the earlier `objects/` name was stale).
- ADR-0037 (sub-agent worktrees/fleet) is partly amended by ADR-0047 — relevant to the
  open view-lifecycle question.

## Architecture (ratified in ADR 0012)

Two surfaces on one `gonzalod`: **graph-as-queries** (an MCP surface Caliban consumes)
and **graph-as-data** (the existing `Get/Put/List` store, inheriting CAS/versioning/
sync). Keying is two-level — content-addressed slices `(file_hash, grammar_version)`
for storage/dedup, per-view manifests `(repo, view_id) → {path → hash}` for identity;
resolution at assembly time; sync set-reconciling (`git diff`-driven); parsing isolated
in a worker subprocess pool. No query engine ever sits under `Store`. See ADR 0012 for
the full rationale and the rejected alternatives.

## Scope this round: usable product path A → C → D, with I and E in parallel

Deferred (roadmap, **not** filed this round): B (scalable `GraphStore` backend —
in-memory is the canary until it stops fitting), F (language breadth), G (assembly-time
name resolution), H (knowledge join + Candle embedder), K (graph diffing across views).

### EPIC A — Two-level storage & incremental foundation `area/store`
- **A1 (M):** content-addressed slice store `slice:(file_hash, grammar_version) → body`,
  write-if-absent, via `Body::Blob`.
- **A2 (M):** per-view manifest record — `RecordKind` keyed `(repo, view_id)`, body
  `{ path → content_hash }`, `MergeClass::Derived`.
- **A3 (S):** path-agnostic slices — drop the `file` field from **both** `Symbol` and
  `Reference`; supply path from the manifest at assembly. Subsumes the `extend`
  duplication defect (slices are write-if-absent, not appended).
- **A4 (M):** set-reconciling sync, `git diff`/`status`-driven (uniform `A`/`M`/`D`).
  Optional watcher path must handle unlink + periodic full reconcile.
- **A5 (S):** `MergeClass::Derived` arm + dispatch wiring (manifests; slices are
  conflict-free by construction).
- **A6 (M):** GC — refcount or mark-sweep over live manifests; frees slices on
  edit/delete/worktree-removal.

### EPIC C — Query surface, assembly & RPCs `area/graph`
- **C1 (M):** view assembly — gather a manifest's slices into a queryable graph; add
  `callees` + transitive `impact` (server-side; never ship the whole graph to the client).
- **C2 (M):** `Definitions`/`ReferencesTo`/`CallersOf`/`Impact` on `Service` + proto
  (reuse JSON-in-protobuf-bytes); all take a view/manifest selector.
- **C3 (S):** expose over gRPC + HTTP.

### EPIC D — MCP server + Caliban consumption `area/integration`
- **D1 (M):** MCP server over C2 (`explore`/`impact`/`search`/`callers`/`callees`/
  `node`/`status`), as a stdio binary and/or HTTP on `gonzalod`. Tools carry a view selector.
- **D2 (S, filed in caliban):** consumption = add a server config entry (no client code).
  Choose stdio-child vs HTTP-to-`gonzalod` per deployment; verify sub-agent inheritance.

### E — Parser crash isolation `area/graph` (single ticket, parallel)
Parse in a worker subprocess pool inside `gonzalod`; respawn on death; store/query stay
in-process. Required before exposing parsing to arbitrary target-repo input at scale.

### I — Internal quick-win `caliban` (single ticket, parallel)
Stand up an off-the-shelf Rust tool (codemap or code-graph-mcp) against the Caliban repo
to validate the payoff with local models. Reversible; independent of A–E.

### J — Persistence-migration coordination `caliban` (tracking issue)
The graph is an early capability in migrating Caliban's local stores
(`caliban-memory`/`-sessions`/`-checkpoint`) onto `gonzalod`. The content-addressed slice
store and checkpoint's `blobs/<sha256>` store are the same shape — align on `Body::Blob`.
Keep graph record/CAS/merge contracts aligned with memory/session/checkpoint.

## Dependencies & sequencing

```
A ──▶ C ──▶ D        (critical path to a usable graph)
E  parallel (before arbitrary-repo scale)
I  parallel (independent; validates payoff)
J  standing coordination note (informs A)
```

## Open questions (carried, not blocking A→D)

Engine choice (Cozo vs SQLite, deferred to B's spike); impact-query latency budget vs
the 1-min MCP timeout; embedder ownership default; grammar-version invalidation policy;
GC mechanism/trigger; license verification before borrowing reference-tool code;
view-lifecycle ownership (who creates/destroys a manifest view — couples to
`caliban-worktrees` + ADR 0037/0047).

## Board structure

Umbrella epic (gonzalo) → child epics A/C/D + tickets E/I + coordination issue J, wired
parent↔child. B/F/G/H/K appear as a roadmap line in the umbrella, unfiled.
