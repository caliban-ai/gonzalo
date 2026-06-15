# SP1 — Gonzalo ticket board foundation (design)

- **Date:** 2026-06-14
- **Status:** approved
- **Sub-project of:** the "full loop" — Caliban via Prospero, with a Gonzalo ticket
  config, driving the shared `caliban-ai` Projects v2 board.

## Goal

Give Gonzalo a runnable, config-driven path that reads the **real shared Kanban
board** — org-level GitHub **Projects v2 #1** (`caliban-ai`) — and persists its
items as first-class ticket `Record`s carrying their **board column** as a
normalized `State.category`. This is the foundation every later sub-project
builds on.

This merges what were originally two pieces: the ticket *surface/config*
foundation and the *board-status read*. "Driving the board" means the board, so
the foundation reads Projects v2 status from day one — not issue open/closed.

## Context

Decisions locked during brainstorming:

- **Connection unit = the org board (Projects v2, GraphQL).** One connection =
  org + project number. The existing per-provider connectors (`gonzalo-ticket-
  github` and siblings from #19) are **read-only single-page REST issue
  importers** — they cannot see Projects v2 status (GraphQL-only; the status
  field lives on the *project*, not the issue). So the board reader is new ground.
- **Surface = all three:** library + CLI + daemon.
- **Config = multi-connection array** (`[[connection]]`), future-proof for the
  jira/linear/gitlab connectors that already exist.

What already exists and is reused:

- `gonzalo_ticket::TicketSource` (provider boundary), `Cursor`, `Page`,
  `Capabilities`, `SourceError` (ADR 0010).
- The per-connection mapping policy: `StateMapping`, `StateSignal` (incl.
  `NativeStatus`), `FieldMapping`. Projects v2 Status maps cleanly onto
  `StateSignal::NativeStatus`.
- `gonzalo_ticket::record_key` → `tickets/<provider>/<uid>`; multi-repo board
  items persist naturally under `tickets/github/`.
- `RecordCodec` (gonzalo-domain) for `Ticket` ⇄ `Record`; `Store` (gonzalo-core)
  for persistence; `InMemorySource` for tests.

## Architecture (Approach A)

The only structurally-clean shape, because the connectors already depend on
`gonzalo-ticket` for the trait — so a registry that constructs connectors must
live **above** them, not inside `gonzalo-ticket` (which would be a cycle).

```
tickets.toml ─▶ gonzalo-ticket-config (parse + registry)
                     │  builds Box<dyn TicketSource> per [[connection]]
                     ▼
            GitHubProjectSource (GraphQL Projects v2)   [in gonzalo-ticket-github]
                     │  board items across all repos + Status field
                     ▼   → canonical Ticket (State.category via StateMapping/NativeStatus)
            ingest engine (in gonzalo-ticket)
                     │  RecordCodec → Record, key = tickets/github/<owner/repo#n>
                     ▼
                  Store (FsStore / server store)
            ▲                         ▲
   gonzalo ticket sync          gonzalod TicketSync RPC (same engine)
```

### Units and responsibilities

- **`gonzalo-ticket` — ingest engine (new module).** `ingest(source, store, …)`:
  pull `fetch_changed`, encode each `Ticket` via `RecordCodec`, `put` into the
  `Store`. Depends only on the trait + `Store` + domain — **no connector deps**,
  so no cycle. Idempotent re-sync. Shared by CLI and daemon.
- **`gonzalo-ticket-config` — new crate.** Parses the multi-connection TOML and
  holds the **registry/factory** mapping `provider` → connector constructor.
  Depends on all connector crates. Surface: `load(path) -> Config`,
  `Config::sources() -> Vec<(name, Box<dyn TicketSource>)>`.
- **`gonzalo-ticket-github` — add `GitHubProjectSource`.** A second source type
  alongside the existing REST `GitHubSource`; GraphQL Projects v2 reader; reuses
  the crate's mapping types.
- **`gonzalo-cli` — `ticket` subcommands.** `sync`, `list`, `get`.
- **`gonzalo-proto` + `gonzalo-server` — `TicketSync` RPC.** Runs the same
  ingest engine against the server's store.

## The board source (`GitHubProjectSource`)

GraphQL query against `organization(login).projectV2(number).items`, **paginated
to completion** via the GraphQL page cursor carried in `Cursor` (the board is the
source of truth — unlike the single-page punt the REST issue source took,
`TODO(#19)`).

For each item, read the linked issue's `number`, `title`, `body`,
`repository.nameWithOwner`, `state`, `labels`, `assignees`, plus
`fieldValueByName("Status")`.

- **Draft items** (no linked issue → no stable uid) are **skipped**.
- The Status value resolves to `State.category` through a
  `StateMapping { signal: NativeStatus, by_value, default }`.
- **uid = `{nameWithOwner}#{number}`** (e.g. `caliban-ai/gonzalo#15`), so the
  owning repo is encoded for SP4's ticket→repo mapping for free.
- `capabilities()` is all-false (read-only); write-back is SP3.

## Config format (`tickets.toml`)

```toml
[[connection]]
name        = "caliban-ai-board"
provider    = "github-projects"
org         = "caliban-ai"
project     = 1
token_env   = "KANBAN_PROJECT_PAT"   # env var NAME, never the secret itself

[connection.state_map]               # Projects v2 Status name → category
default       = "open"
"Todo"        = "open"
"In Progress" = "in_progress"
"Blocked"     = "pending"
"Done"        = "done"
```

- An array of `[[connection]]` tables; jira/linear/gitlab connections can be
  added later without code changes beyond their registry entries.
- `token_env` names an environment variable; the secret never lives in the file.
- The registry maps `provider` (e.g. `"github-projects"`) → connector
  constructor, supplying org/project/token/state_map.

## Surfaces

- **Library:** `gonzalo_ticket_config::load(path)`, `Config::sources()`,
  `gonzalo_ticket::ingest(source, store, …)`.
- **CLI:**
  - `gonzalo ticket sync --config tickets.toml --root <store>` — fetch board →
    persist as Records.
  - `gonzalo ticket list [--root <store>]` — list persisted ticket keys (thin
    wrapper over existing `list`, filtered to the `tickets/` namespace).
  - `gonzalo ticket get --root <store> <uid>` — show one persisted ticket Record.
- **Daemon:** a `TicketSync` RPC in `gonzalo-proto` + handler in `gonzalo-server`
  running the same ingest engine against the server's store.

## Error handling

- **Mapping is total.** An unrecognized Status name falls back to
  `state_map.default`, so one odd card never fails a sync.
- **Transport/GraphQL errors** surface as `SourceError::Backend` and fail the
  sync as a whole (the board read must be complete to be trustworthy).
- **Config errors** (missing env var, malformed TOML, unknown provider) are a
  typed config error reported **before** any network call.

## Testing (ADR 0006 pattern)

- **Pure board mapping** (GraphQL JSON → `Ticket`) unit-tested against a recorded
  fixture — a new conformance variant **"github-projects (native-status)"**.
- **Ingest engine** tested against `InMemorySource` + a temp `FsStore`:
  round-trips Records and is idempotent on re-sync.
- **Config parsing** unit-tested: valid config, missing env var, unknown
  provider, malformed TOML.
- **Board source HTTP layer** exercised via recorded GraphQL fixtures, matching
  the existing connectors' approach.

## Out of scope (later sub-projects)

- **Write-back / moving cards** (`set_state` → Projects v2 GraphQL mutation):
  SP3. `capabilities()` stays all-false here.
- **Prospero orchestration** (pick ticket → spawn Caliban agent → move card on
  lifecycle): SP4.
- **Non-GitHub board connections:** the config array supports them, but only the
  `github-projects` provider is implemented in SP1.

## Consequences

- **Positive:** A runnable, config-driven read of the real shared board, with
  board column normalized into `State.category`; tickets become first-class
  `Record`s that compose with the vector/graph layers by shared key (ADR 0008).
  The registry + ingest engine are reused unchanged by SP3/SP4.
- **Negative:** Introduces a new crate (`gonzalo-ticket-config`) and a second,
  GraphQL-based source type in the GitHub crate — more surface than the REST
  issue importer. The default `state_map` must track the board's actual column
  names to be useful out of the box.
- **Revisit if:** the board grows a status model the flat `state_map` can't
  express; or Prospero (SP4) needs to consume tickets over a transport this
  foundation didn't anticipate.
