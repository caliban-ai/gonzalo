# gonzalo

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
| `gonzalo-ticket-github` `[ticket-github]` | GitHub issue connector (`GitHubSource`, read-only) |
| `gonzalo-ticket-jira` `[ticket-jira]` | Jira issue connector (`JiraSource`, statusCategory + ADF, read-only) |
| `gonzalo-ticket-linear` `[ticket-linear]` | Linear issue connector (`LinearSource`, GraphQL, read-only) |
| `gonzalo-ticket-gitlab` `[ticket-gitlab]` | GitLab issue connector (`GitLabSource`, scoped-label workflow, read-only) |
| `gonzalo-proto` / `gonzalo-server` | daemon: gRPC + HTTP/JSON over one service, optional bearer auth (`gonzalod` bin) |
| `gonzalo-cli` | admin/ops CLI (`gonzalo`): `list`/`get`/`status`/`migrate`/`sync` |

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

The daemon exposes the same operation: `POST /v1/tickets/sync` with a JSON
connection body, or the `TicketSync` gRPC. Write-back (moving cards) is not yet
implemented — this is a read-only import (ADR 0010 phase 1).

## License

AGPL-3.0-only. See [LICENSE](LICENSE).

## Building

```bash
cargo build --workspace
cargo test  --workspace
```
