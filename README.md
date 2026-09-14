# gonzalo

[![Release](https://img.shields.io/github/v/release/caliban-ai/gonzalo?sort=semver)](https://github.com/caliban-ai/gonzalo/releases)
[![Container](https://img.shields.io/badge/ghcr.io-gonzalod-2496ED?logo=docker&logoColor=white)](https://github.com/caliban-ai/gonzalo/pkgs/container/gonzalo)
[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue.svg)](LICENSE)
[![CI](https://github.com/caliban-ai/gonzalo/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/caliban-ai/gonzalo/actions/workflows/ci.yml)

A robust, shareable persistence layer for the [caliban-ai](https://github.com/caliban-ai) stack.

Gonzalo lifts [caliban](https://github.com/caliban-ai/caliban)'s local-first state
(memory tiers, auto-memory topics, sessions, and checkpoints) into a layer that can
be shared across multiple systems and contributors, via pluggable storage substrates
behind a generic, versioned, conflict-aware core. It also hosts a tree-sitter code
graph that agents query over MCP.

**Guide:** <https://caliban-ai.github.io/gonzalo/>, covering the CLI, the MCP server,
running `gonzalod`, storage backends, and the architecture decision log. Design specs
live in `docs/superpowers/specs/`, per-milestone build notes in `docs/superpowers/plans/`.

## Ecosystem

| project | role |
|---|---|
| [caliban](https://github.com/caliban-ai/caliban) | the agent |
| [prospero](https://github.com/caliban-ai/prospero) | runs the caliban agent fleet |
| [ariel](https://github.com/caliban-ai/ariel) | chat bridge for the fleet (Discord first, then Slack and Teams). In early implementation (Discord backend and prospero client landed; gonzalo integration not yet wired). Its identity, role grants, channel configuration and audit trail are to be gonzalo records; ariel stores nothing of its own ([#277](https://github.com/caliban-ai/gonzalo/issues/277), [#278](https://github.com/caliban-ai/gonzalo/issues/278)). |
| **gonzalo** | persistence: records, stores, capability layers, daemon, code-graph MCP server |

## Architecture

A generic, versioned `Record`/`Store` core with optimistic-concurrency conflict
surfacing, plus capability layers, all consumed through the `gonzalo` facade
(features in brackets) or the daemon.

| Crate | Role |
|-------|------|
| `gonzalo-core` | `Record` model, `Store`/`BlobStore`/`Sync` traits, revisions, 3-way merge, conformance suite |
| `gonzalo-store-fs` `[fs]` | filesystem substrate (default) |
| `gonzalo-store-git` `[git]` | git-backed substrate (commit-per-write, pull with content-aware merge, push) |
| `gonzalo-store-s3` `[s3]` | S3-compatible object-store substrate (needs atomic `If-Match`; RustFS qualified, ADR 0019) |
| `gonzalo-store-server` `[remote]` | client substrate over a remote daemon (HTTP or gRPC) |
| `gonzalo-domain` | typed views: `MemoryTier`, `Topic`, `Session`, `Checkpoint`, `Ticket` |
| `gonzalo-vector` `[vector]` | `Embedder` + `VectorIndex` (exact in-memory index; approximate `hnsw` feature, ADR 0014) |
| `gonzalo-embed` | local CPU sentence embedder (Candle + all-MiniLM, ADR 0013) |
| `gonzalo-knowledge` `[knowledge]` | knowledge store: `KnowledgeStore` over records + vector by `RecordKey` (ADR 0011) |
| `gonzalo-graph` `[graph]` | tree-sitter code graph over 17 languages: symbols, references, imports, resolution, view diff |
| `gonzalo-graph-sqlite` | persistent SQLite `GraphStore`, one database per indexed view |
| `gonzalo-parse` | crash-isolated parsing: `gonzalo-parse-worker` subprocess pool around tree-sitter |
| `gonzalo-ticket` `[ticket]` | normalized work-item layer: `TicketSource`, `StateMapping` (ADR 0010) |
| `gonzalo-ticket-github` `[ticket-github]` | GitHub connectors: `GitHubSource` (REST issues, read + write-back); `GitHubProjectSource` (Projects v2 board over GraphQL, read + card move) |
| `gonzalo-ticket-jira` `[ticket-jira]` | Jira issue connector (`JiraSource`, statusCategory + ADF, transition write-back) |
| `gonzalo-ticket-linear` `[ticket-linear]` | Linear issue connector (`LinearSource`, GraphQL, read + write-back) |
| `gonzalo-ticket-gitlab` `[ticket-gitlab]` | GitLab issue connector (`GitLabSource`, scoped-label workflow, read + write-back) |
| `gonzalo-ticket-asana` `[ticket-asana]` | Asana task connector (`AsanaSource`, completed/section/field signals, read + write-back) |
| `gonzalo-ticket-config` | multi-connection ticket config (`tickets.toml`) + provider registry |
| `gonzalo-proto` / `gonzalo-server` | daemon (`gonzalod`): gRPC + HTTP/JSON over one service; fs or S3 substrate; namespace-scoped bearer auth (ADR 0015); `/healthz` + `/readyz` probes |
| `gonzalo-mcp` | MCP server exposing the code graph to agents over stdio |
| `gonzalo-cli` | admin/ops CLI (`gonzalo`): `list`/`get`/`status`/`migrate`/`sync`, `index`/`gc`, `ticket sync`/`list`/`get`/`move` |
| `gonzalo-soak` | HA soak harness: stateless `gonzalod` replicas over S3 under replica-kill chaos |

Every storage substrate passes a shared conformance suite shipped by
`gonzalo-core`. The consistency model surfaces concurrent edits as
`PutResult::Conflict` (never silently lost) and auto-merges append-only kinds.

## Quick start

Index a repository and query it from an agent (see the
[MCP guide](docs/guide/src/mcp.md) for install and details):

```bash
gonzalo index --root ~/.gonzalo --repo acme/widgets --view main /path/to/checkout
claude mcp add gonzalo --env GONZALO_ROOT=$HOME/.gonzalo -- gonzalo-mcp
```

Run the daemon (see [Running gonzalod](docs/guide/src/daemon.md)):

```bash
docker run --rm -p 8080:8080 -p 50051:50051 -v gonzalo-data:/data ghcr.io/caliban-ai/gonzalo
```

## Tickets

Gonzalo can import the shared caliban-ai Kanban board (GitHub Projects v2 #1)
into a store as first-class ticket records, with each card's board column
normalized into a `State.category`. Configure connections in a `tickets.toml`
(see `tickets.example.toml`):

```bash
export KANBAN_PROJECT_PAT=ghp_...           # PAT with read:project + repo scope
cp tickets.example.toml tickets.toml
gonzalo ticket sync --config tickets.toml --root ./store
gonzalo ticket list --root ./store
gonzalo ticket get  --root ./store --connection caliban-ai-board "caliban-ai/gonzalo#15"
```

Move a card to a column (write-back):

```bash
gonzalo ticket move --config tickets.toml "caliban-ai/gonzalo#15" in_progress
```

The daemon exposes the same sync operation: `POST /v1/tickets/sync` with a JSON
connection body, or the `TicketSync` gRPC. See the [CLI guide](docs/guide/src/cli.md#tickets).

## License

AGPL-3.0-only. See [LICENSE](LICENSE).

## Building

```bash
cargo build --workspace
cargo test  --workspace
```
