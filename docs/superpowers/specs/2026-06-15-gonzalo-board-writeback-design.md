# SP3-board — Projects v2 status write-back (design)

- **Date:** 2026-06-15
- **Status:** approved
- **Sub-project of:** the "full loop" — Caliban via Prospero, with a Gonzalo
  ticket config, driving the shared caliban-ai Projects v2 board.
- **Builds on:** SP1 board foundation (`097ee16`, gonzalo PR #31) and the
  phase-2 write-back already landed for the issue connectors (PR #27).

## Goal

Let Gonzalo **move a card** on the shared GitHub Projects v2 #1 board:
implement `set_state` on `GitHubProjectSource` as a Projects v2
`updateProjectV2ItemFieldValue` mutation, resolving a normalized
`StateCategory` back to a board column. Expose it through the CLI. This is the
write half of "driving the board"; the read half shipped in SP1.

## Context

- `GitHubProjectSource` (SP1) is read-only: `capabilities()` all-false, status
  read via a `StateMapping { signal: NativeStatus, by_value, default }`
  (column-name → category).
- PR #27 implemented `set_state`/`comment` for the *issue* connectors
  (`GitHubSource` REST, jira, linear, gitlab, asana). Moving a **board card** is
  a distinct operation (Projects v2 GraphQL mutation), not issue open/closed.
- The `TicketSource` trait signature is fixed: `set_state(&self, uid: &str,
  target: StateCategory)`. So the board source must resolve `StateCategory` →
  column internally.

Locked decisions (brainstorm 2026-06-15):

- **Reverse mapping:** invert the existing `state_map` (category→column)
  automatically; allow an optional `[connection.set_targets]` (category→column)
  override that wins; error clearly when a category is ambiguous (two columns,
  no override) or unmapped.
- **Scope:** source `set_state` + flipped `capabilities()`, plus a CLI
  `gonzalo ticket move`. Defer the daemon RPC.

## Architecture

### 1. Reverse mapping (`gonzalo-ticket`, pure)

Add to `gonzalo-ticket/src/mapping.rs`:

```rust
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReverseError {
    #[error("no column maps to category {0:?}")]
    Unmapped(StateCategory),
    #[error("category {0:?} is ambiguous across columns {1:?}; set an explicit set_targets override")]
    Ambiguous(StateCategory, Vec<String>),
}

impl StateMapping {
    /// Resolve a normalized category back to a board column name. `overrides`
    /// (category→column) win outright; otherwise the column is the unique
    /// `by_value` entry whose category matches. Errors on ambiguity / no match.
    pub fn column_for(
        &self,
        category: StateCategory,
        overrides: &BTreeMap<StateCategory, String>,
    ) -> Result<String, ReverseError> { ... }
}
```

Resolution order: `overrides.get(category)` → else the single `by_value` key
whose value == category (case preserved) → `Ambiguous` if >1 → `Unmapped` if 0.
`default` is **not** a write target (it's a read fallback only).

### 2. Source write-back (`gonzalo-ticket-github`)

`GitHubProjectSource` gains:

- A field `set_targets: BTreeMap<StateCategory, String>` (default empty) and a
  non-breaking builder `pub fn with_write_targets(mut self, targets) -> Self`
  so `new(...)` stays stable.
- `capabilities()` → `Capabilities { push: true, ..default() }`.
- `set_state(uid, target)`:
  1. `column = self.mapping.column_for(target, &self.set_targets)` (map
     `ReverseError` → `SourceError::Unsupported`/`Backend`).
  2. Parse `uid` → `owner/repo#number` (reuse the issue-number parse).
  3. GraphQL **lookup** query (pure builder `project_item_query(owner, repo,
     number)`): the issue's `projectItems`, each with `id`, `project { id
     number }`, and the Status field `... on ProjectV2SingleSelectField { id
     options { id name } }`. Pick the node where `project.number ==
     self.project_number`.
  4. Pure extractor `resolve_item(resp, project_number, column) ->
     Result<(item_id, project_id, field_id, option_id)>`: errors if the issue
     isn't on this board, the project has no Status single-select field, or the
     column isn't an option (case-insensitive match, mirroring read).
  5. GraphQL **mutation** (pure builder `set_option_mutation(project_id,
     item_id, field_id, option_id)`): `updateProjectV2ItemFieldValue(input:{...,
     value:{ singleSelectOptionId }}) { projectV2Item { id } }`.
  6. Reuse the SP1 `items_or_error`-style top-level `errors` handling for both
     the lookup and the mutation.

`get`/`fetch_changed`/the read query are unchanged.

### 3. Config (`gonzalo-ticket-config`)

- `Connection` gains `#[serde(default)] set_targets: BTreeMap<String, String>`
  (category-name → column-name).
- A helper builds `BTreeMap<StateCategory, String>` from it, parsing category
  names with the existing `parse_category` (unknown → `ConfigError::BadCategory`).
- `build_source` calls `GitHubProjectSource::new(...).with_write_targets(...)`.

### 4. CLI (`gonzalo-cli`)

`gonzalo ticket move --config <cfg> [--connection <name>] <uid> <category>`:

- Load config; select the connection by `--connection`, or the sole connection
  if exactly one (error if 0 or >1 without `--connection`).
- `build_source(conn)` → `set_state(uid, parse_category(category)?)`.
- Print `moved <uid> → <category>` on success.

`parse_category` is currently private to `gonzalo-ticket-config`; expose it
(`pub`) so the CLI reuses one parser rather than duplicating the category
strings.

## Error handling

- Reverse-map failures (`Unmapped`/`Ambiguous`) → a clear `SourceError`
  (`Unsupported` for unmapped category, `Backend` carrying the ambiguity hint).
- Issue not on the board / no Status field / unknown column → `SourceError::Backend`
  with a message naming the uid / column.
- GraphQL top-level `errors` (bad token, perms) → `Backend`, reusing the SP1
  pattern (HTTP-200-with-errors must not become an opaque serde failure).

## Testing

- **Reverse mapping** (`gonzalo-ticket`): unique inversion; override wins;
  ambiguous→`Ambiguous`; unmapped→`Unmapped`; default-is-not-a-target.
- **Pure GraphQL builders**: `project_item_query` carries owner/repo/number;
  `set_option_mutation` carries the four ids.
- **Pure extractor** `resolve_item`: fixture with the issue on the target
  project → correct ids; issue on a *different* project number → error; missing
  Status field → error; unknown column → error; case-insensitive option match.
- **Config**: `set_targets` parses into `BTreeMap<StateCategory,String>`;
  bad category → `BadCategory`.
- **CLI**: connection selection (0/1/many) and category-parse error paths
  (network-free).
- **Live verify** (final, manual via the skill): move a real caliban-ai card to
  a new column and move it back — a destructive path, exercised carefully
  (move + restore), not a unit test.

## Out of scope

- Daemon RPC / HTTP route for `set_state` (a later thin add, mirroring SP1's
  `TicketSync`).
- `comment` on the board source (stays `Unsupported`; issue comments live on the
  REST `GitHubSource`).
- Adding the board source to the formal conformance suite (focused unit/fixture
  tests cover the new logic; conformance can fold it in later).

## Consequences

- **Positive:** Completes the board write path — Gonzalo can now both read and
  move cards, which is exactly what SP4 (Prospero) needs to reflect agent
  progress on the board. Reverse mapping is pure and testable; zero new config
  for the 1:1 caliban-ai board, with an escape hatch for richer boards.
- **Negative:** `set_state` makes two GraphQL round-trips (lookup then mutate)
  per card — acceptable for orchestration-rate writes. The reverse mapping adds
  a second mapping concept (read column→category, write category→column) users
  must keep coherent.
- **Revisit if:** boards routinely map many columns to one category (make
  `set_targets` required, or carry the source column on the ticket for
  round-trip); or SP4 needs batch/transactional card moves.
