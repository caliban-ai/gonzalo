# gonzalo

[![crates.io](https://img.shields.io/crates/v/gonzalo.svg)](https://crates.io/crates/gonzalo)
[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue.svg)](LICENSE)
[![CI](https://github.com/caliban-ai/gonzalo/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/caliban-ai/gonzalo/actions/workflows/ci.yml)

A robust, shareable persistence layer for [caliban](https://github.com/caliban-ai/caliban).

Gonzalo lifts caliban's local-first state — memory tiers, auto-memory topics,
sessions, and checkpoints — into a layer that can be shared across multiple
systems and contributors, via pluggable storage substrates behind a generic,
versioned, conflict-aware core. See `docs/superpowers/specs/` for the design
and `docs/superpowers/plans/` for the per-milestone build notes.

## Architecture

A generic, versioned `Record`/`Store` core with optimistic-concurrency conflict
surfacing, plus capability layers — all consumed through the `gonzalo` facade
(features in brackets) or the daemon.

| Crate | Role |
|-------|------|
| `gonzalo-core` | `Record` model, `Store`/`Sync` traits, revisions, merge, conformance suite |
| `gonzalo-store-fs` `[fs]` | filesystem substrate (default) |
| `gonzalo-store-git` `[git]` | git-backed substrate (commit-per-write, FF pull/push) |
| `gonzalo-store-s3` `[s3]` | S3-compatible object-store substrate |
| `gonzalo-store-server` `[remote]` | client substrate over a remote daemon (HTTP or gRPC) |
| `gonzalo-domain` | typed views: `MemoryTier`, `Topic`, `Session`, `Checkpoint`, `Ticket` |
| `gonzalo-vector` `[vector]` | `Embedder` + `VectorIndex` (exact cosine in-memory index) |
| `gonzalo-graph` `[graph]` | tree-sitter code graph (`build_rust`, `GraphStore`) |
| `gonzalo-ticket` `[ticket]` | normalized work-item layer: `TicketSource`, `StateMapping` (ADR 0010) |
| `gonzalo-ticket-github` `[ticket-github]` | GitHub connectors: `GitHubSource` (REST issues, read + write-back); `GitHubProjectSource` (Projects v2 board over GraphQL, read + card move) |
| `gonzalo-ticket-jira` `[ticket-jira]` | Jira issue connector (`JiraSource`, statusCategory + ADF, transition write-back) |
| `gonzalo-ticket-linear` `[ticket-linear]` | Linear issue connector (`LinearSource`, GraphQL, read + write-back) |
| `gonzalo-ticket-gitlab` `[ticket-gitlab]` | GitLab issue connector (`GitLabSource`, scoped-label workflow, read + write-back) |
| `gonzalo-ticket-asana` `[ticket-asana]` | Asana task connector (`AsanaSource`, completed/section/field signals, read + write-back) |
| `gonzalo-ticket-config` | multi-connection ticket config (`tickets.toml`) + provider registry → `Box<dyn TicketSource>` |
| `gonzalo-knowledge` `[knowledge]` | knowledge store: `KnowledgeStore` over records + vector by `RecordKey` (ADR 0011) |
| `gonzalo-proto` / `gonzalo-server` | daemon: gRPC + HTTP/JSON over one service, optional bearer auth (`gonzalod` bin); `TicketSync` RPC + `POST /v1/tickets/sync` |
| `gonzalo-cli` | admin/ops CLI (`gonzalo`): `list`/`get`/`status`/`migrate`/`sync`, `ticket sync`/`list`/`get` |

Every storage substrate passes a shared conformance suite shipped by
`gonzalo-core`. The consistency model surfaces concurrent edits as
`PutResult::Conflict` (never silently lost) and auto-merges append-only kinds.

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
gonzalo ticket get  --root ./store "caliban-ai/gonzalo#15"
```

Move a card to a column (write-back):

```bash
gonzalo ticket move --config tickets.toml "caliban-ai/gonzalo#15" in_progress
```

The daemon exposes the same sync operation: `POST /v1/tickets/sync` with a JSON
connection body, or the `TicketSync` gRPC. Board write-back is now supported via
`ticket move`, which updates a card's column on GitHub Projects v2 (ADR 0010).

## License

AGPL-3.0-only. See [LICENSE](LICENSE).

## Building

```bash
cargo build --workspace
cargo test  --workspace
```
