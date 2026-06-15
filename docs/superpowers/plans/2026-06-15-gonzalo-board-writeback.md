# Gonzalo Board Write-Back (SP3-board) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `GitHubProjectSource` move a card on the GitHub Projects v2 board via `set_state`, resolving a `StateCategory` to a board column, exposed through a `gonzalo ticket move` CLI command.

**Architecture:** A pure reverse-mapping helper on `StateMapping` (`gonzalo-ticket`) turns category→column (inverted `by_value` + optional overrides). `GitHubProjectSource` (`gonzalo-ticket-github`) gains `set_state`: a GraphQL lookup of the issue's board item (item id, project id, Status field id + options), then an `updateProjectV2ItemFieldValue` mutation. Config carries optional `set_targets`; the CLI adds `ticket move`.

**Tech Stack:** Rust 2024, `async-trait`, `reqwest` (GraphQL POST), `serde`/`serde_json`, `thiserror`, `clap`.

**Spec:** `docs/superpowers/specs/2026-06-15-gonzalo-board-writeback-design.md`

---

## File Structure

| File | Responsibility | Task |
|------|----------------|------|
| `crates/gonzalo-ticket/src/mapping.rs` (modify) | Add `ReverseError` + `StateMapping::column_for` (category→column). | 1 |
| `crates/gonzalo-ticket/src/lib.rs` (modify) | Export `ReverseError`. | 1 |
| `crates/gonzalo-ticket-github/src/project_source.rs` (modify) | `set_targets` field + `with_write_targets`; `capabilities().push = true`; `set_state`; pure builders `project_item_query` / `set_option_mutation`; pure extractor `resolve_item`; lookup structs. | 2,3 |
| `crates/gonzalo-ticket-config/src/lib.rs` (modify) | `Connection.set_targets`; build `BTreeMap<StateCategory,String>`; pass to source; make `parse_category` `pub`. | 4 |
| `crates/gonzalo-cli/src/lib.rs` (modify) | `ticket_move` command function + connection selection. | 5 |
| `crates/gonzalo-cli/src/main.rs` (modify) | `TicketCommands::Move` variant + dispatch. | 5 |
| `tickets.example.toml` (modify) | Document optional `[connection.set_targets]`. | 5 |

---

## Task 1: Reverse mapping in `gonzalo-ticket`

**Files:**
- Modify: `crates/gonzalo-ticket/src/mapping.rs`, `crates/gonzalo-ticket/src/lib.rs`
- Test: inline `#[cfg(test)]` in `mapping.rs`

- [ ] **Step 1: Write the failing test** — add to the `#[cfg(test)] mod tests` block in `crates/gonzalo-ticket/src/mapping.rs`

```rust
    fn board_mapping() -> StateMapping {
        // 1:1 columns like the caliban-ai board.
        let mut by_value = BTreeMap::new();
        by_value.insert("Backlog".into(), StateCategory::Backlog);
        by_value.insert("In progress".into(), StateCategory::InProgress);
        by_value.insert("Done".into(), StateCategory::Done);
        StateMapping { signal: StateSignal::NativeStatus, by_value, default: StateCategory::Open }
    }

    #[test]
    fn column_for_inverts_unique_mapping() {
        let m = board_mapping();
        let none: BTreeMap<StateCategory, String> = BTreeMap::new();
        assert_eq!(m.column_for(StateCategory::InProgress, &none).unwrap(), "In progress");
        assert_eq!(m.column_for(StateCategory::Done, &none).unwrap(), "Done");
    }

    #[test]
    fn column_for_unmapped_category_errors() {
        let m = board_mapping();
        let none: BTreeMap<StateCategory, String> = BTreeMap::new();
        // Nothing maps to Pending.
        assert_eq!(
            m.column_for(StateCategory::Pending, &none),
            Err(ReverseError::Unmapped(StateCategory::Pending))
        );
    }

    #[test]
    fn column_for_override_wins_and_resolves_ambiguity() {
        // Two columns share the Done category → ambiguous without an override.
        let mut by_value = BTreeMap::new();
        by_value.insert("Shipped".into(), StateCategory::Done);
        by_value.insert("Done".into(), StateCategory::Done);
        let m = StateMapping { signal: StateSignal::NativeStatus, by_value, default: StateCategory::Open };

        let none: BTreeMap<StateCategory, String> = BTreeMap::new();
        match m.column_for(StateCategory::Done, &none) {
            Err(ReverseError::Ambiguous(StateCategory::Done, cols)) => {
                assert_eq!(cols, vec!["Done".to_string(), "Shipped".to_string()]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }

        let mut overrides = BTreeMap::new();
        overrides.insert(StateCategory::Done, "Shipped".to_string());
        assert_eq!(m.column_for(StateCategory::Done, &overrides).unwrap(), "Shipped");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p gonzalo-ticket column_for`
Expected: FAIL — `column_for` / `ReverseError` not found.

- [ ] **Step 3: Implement** — add to `crates/gonzalo-ticket/src/mapping.rs`

At the top, ensure these imports exist (the file already uses `StateCategory` and `BTreeMap`):

```rust
// (existing) use gonzalo_domain::StateCategory;
// (existing) use std::collections::BTreeMap;
```

Add the error type (after the `StateMapping` struct definition):

```rust
/// Failure resolving a normalized category back to a board column for write-back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReverseError {
    /// No column maps to this category and no override was given.
    #[error("no column maps to category {0:?}")]
    Unmapped(StateCategory),
    /// More than one column maps to this category; an explicit override is required.
    #[error("category {0:?} is ambiguous across columns {1:?}; set an explicit set_targets override")]
    Ambiguous(StateCategory, Vec<String>),
}
```

Add the method inside `impl StateMapping { ... }` (next to `category_of`):

```rust
    /// Resolve a normalized `category` back to the board column name to write.
    ///
    /// `overrides` (category→column) win outright. Otherwise the column is the
    /// unique `by_value` key whose category equals `category`. The `default`
    /// category is a read-time fallback only and is never a write target.
    pub fn column_for(
        &self,
        category: StateCategory,
        overrides: &BTreeMap<StateCategory, String>,
    ) -> Result<String, ReverseError> {
        if let Some(col) = overrides.get(&category) {
            return Ok(col.clone());
        }
        let mut matches: Vec<String> = self
            .by_value
            .iter()
            .filter(|(_, c)| **c == category)
            .map(|(k, _)| k.clone())
            .collect();
        matches.sort(); // deterministic order for the ambiguity message
        match matches.len() {
            0 => Err(ReverseError::Unmapped(category)),
            1 => Ok(matches.pop().unwrap()),
            _ => Err(ReverseError::Ambiguous(category, matches)),
        }
    }
```

- [ ] **Step 4: Export `ReverseError`** in `crates/gonzalo-ticket/src/lib.rs`

Change the mapping re-export line:

```rust
pub use mapping::{FieldMapping, ReverseError, StateMapping, StateSignal};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-ticket column_for`
Expected: PASS (3 tests). Then `cargo clippy -p gonzalo-ticket --all-targets -- -D warnings` clean.

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-ticket/src/mapping.rs crates/gonzalo-ticket/src/lib.rs
git commit -m "feat(ticket): StateMapping::column_for — reverse category->column for write-back"
```

---

## Task 2: Pure GraphQL builders + extractor for board write-back

**Files:**
- Modify: `crates/gonzalo-ticket-github/src/project_source.rs`
- Test: inline `#[cfg(test)]` in `project_source.rs`

This task adds the **pure** (network-free) pieces only: the two GraphQL body builders, the lookup response structs, and the `resolve_item` extractor. Task 3 wires them into `set_state`.

- [ ] **Step 1: Write the failing test** — add to the `#[cfg(test)] mod tests` in `project_source.rs`

```rust
    #[test]
    fn project_item_query_carries_owner_repo_number() {
        let b = project_item_query("caliban-ai", "gonzalo", 19);
        assert_eq!(b["variables"]["owner"], "caliban-ai");
        assert_eq!(b["variables"]["repo"], "gonzalo");
        assert_eq!(b["variables"]["number"], 19);
        assert!(b["query"].as_str().unwrap().contains("projectItems"));
    }

    #[test]
    fn set_option_mutation_carries_four_ids() {
        let b = set_option_mutation("PROJ", "ITEM", "FIELD", "OPT");
        assert_eq!(b["variables"]["project"], "PROJ");
        assert_eq!(b["variables"]["item"], "ITEM");
        assert_eq!(b["variables"]["field"], "FIELD");
        assert_eq!(b["variables"]["option"], "OPT");
        assert!(b["query"].as_str().unwrap().contains("updateProjectV2ItemFieldValue"));
    }

    const ITEM_LOOKUP: &str = r#"{
      "data": { "repository": { "issue": { "projectItems": { "nodes": [
        {
          "id": "ITEM_OTHER",
          "project": {
            "id": "PROJ_OTHER", "number": 7,
            "field": { "id": "F_OTHER", "options": [{ "id": "o1", "name": "Done" }] }
          }
        },
        {
          "id": "ITEM_1",
          "project": {
            "id": "PROJ_1", "number": 1,
            "field": { "id": "FIELD_1", "options": [
              { "id": "opt_todo", "name": "Todo" },
              { "id": "opt_ip", "name": "In progress" },
              { "id": "opt_done", "name": "Done" }
            ] }
          }
        }
      ] } } } }
    }"#;

    #[test]
    fn resolve_item_picks_target_project_and_option() {
        let resp: ItemLookupResponse = serde_json::from_str(ITEM_LOOKUP).unwrap();
        let r = resolve_item(resp, 1, "In progress").unwrap();
        assert_eq!(r.item_id, "ITEM_1");
        assert_eq!(r.project_id, "PROJ_1");
        assert_eq!(r.field_id, "FIELD_1");
        assert_eq!(r.option_id, "opt_ip");
    }

    #[test]
    fn resolve_item_matches_option_case_insensitively() {
        let resp: ItemLookupResponse = serde_json::from_str(ITEM_LOOKUP).unwrap();
        let r = resolve_item(resp, 1, "in PROGRESS").unwrap();
        assert_eq!(r.option_id, "opt_ip");
    }

    #[test]
    fn resolve_item_errors_when_issue_not_on_board() {
        let resp: ItemLookupResponse = serde_json::from_str(ITEM_LOOKUP).unwrap();
        // project 99 is not among the issue's project items
        assert!(resolve_item(resp, 99, "Done").is_err());
    }

    #[test]
    fn resolve_item_errors_on_unknown_column() {
        let resp: ItemLookupResponse = serde_json::from_str(ITEM_LOOKUP).unwrap();
        assert!(resolve_item(resp, 1, "Nonexistent").is_err());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gonzalo-ticket-github resolve_item`
Expected: FAIL — items not defined.

- [ ] **Step 3: Implement the builders, structs, and extractor** in `crates/gonzalo-ticket-github/src/project_source.rs`

Add the lookup response structs (near the top, after the existing `use` lines):

```rust
use serde::Deserialize;

/// Resolved coordinates for a single Projects v2 card write.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ItemCoords {
    pub item_id: String,
    pub project_id: String,
    pub field_id: String,
    pub option_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ItemLookupResponse {
    #[serde(default)]
    pub data: Option<ItemLookupData>,
    #[serde(default)]
    pub errors: Vec<crate::project_mapping::GqlError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ItemLookupData {
    pub repository: Option<LookupRepo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LookupRepo {
    pub issue: Option<LookupIssue>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LookupIssue {
    #[serde(rename = "projectItems")]
    pub project_items: LookupItems,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LookupItems {
    pub nodes: Vec<LookupItem>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LookupItem {
    pub id: String,
    pub project: LookupProject,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LookupProject {
    pub id: String,
    pub number: u32,
    /// The project's Status single-select field (id + options). Read from the
    /// *project* (not the item's current value), so it's present even when the
    /// card has no status set yet. `None` if the project has no such field.
    pub field: Option<LookupField>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LookupField {
    pub id: String,
    pub options: Vec<LookupOption>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LookupOption {
    pub id: String,
    pub name: String,
}
```

Add the pure builders (near `graphql_body`):

```rust
/// GraphQL body that finds an issue's board item across its projects, with each
/// project's Status single-select field id + options. Pure / network-free.
pub(crate) fn project_item_query(owner: &str, repo: &str, number: u64) -> serde_json::Value {
    const QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    issue(number: $number) {
      projectItems(first: 20) {
        nodes {
          id
          project {
            id
            number
            field(name: "Status") {
              ... on ProjectV2SingleSelectField { id options { id name } }
            }
          }
        }
      }
    }
  }
}"#;
    serde_json::json!({
        "query": QUERY,
        "variables": { "owner": owner, "repo": repo, "number": number },
    })
}

/// GraphQL body that sets a card's single-select Status option. Pure.
pub(crate) fn set_option_mutation(
    project_id: &str,
    item_id: &str,
    field_id: &str,
    option_id: &str,
) -> serde_json::Value {
    const QUERY: &str = r#"
mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) {
  updateProjectV2ItemFieldValue(input: {
    projectId: $project, itemId: $item, fieldId: $field,
    value: { singleSelectOptionId: $option }
  }) { projectV2Item { id } }
}"#;
    serde_json::json!({
        "query": QUERY,
        "variables": {
            "project": project_id, "item": item_id, "field": field_id, "option": option_id
        },
    })
}
```

> **Note on the query shape:** the Status field id + options are read from the **project** (`project.field(name:"Status")`), not from the item's current `fieldValueByName` — the latter is `null` when a card has no status yet, which would make setting an unset card impossible. Reading the project's field definition is always available. `ProjectV2.field(name:)` returns an interface; the inline fragment `... on ProjectV2SingleSelectField { id options }` selects the single-select case. Keep the `LookupProject.field` struct and the `ITEM_LOOKUP` fixture in sync; if the live run shows different nesting, adjust only those — the extractor and wiring are unaffected.

Add the pure extractor:

```rust
/// Pick the issue's item on the project numbered `project_number`, then resolve
/// `column` to its single-select option id (case-insensitive). Surfaces GraphQL
/// `errors` and missing pieces as `Backend`.
pub(crate) fn resolve_item(
    resp: ItemLookupResponse,
    project_number: u32,
    column: &str,
) -> Result<ItemCoords> {
    if !resp.errors.is_empty() {
        let msg = resp.errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join("; ");
        return Err(SourceError::Backend(format!("github graphql: {msg}")));
    }
    let nodes = resp
        .data
        .and_then(|d| d.repository)
        .and_then(|r| r.issue)
        .map(|i| i.project_items.nodes)
        .ok_or_else(|| SourceError::Backend("issue not found".into()))?;
    let item = nodes
        .into_iter()
        .find(|n| n.project.number == project_number)
        .ok_or_else(|| {
            SourceError::Backend(format!("issue is not on project #{project_number}"))
        })?;
    let field = item
        .project
        .field
        .ok_or_else(|| SourceError::Backend("project has no Status single-select field".into()))?;
    let option = field
        .options
        .iter()
        .find(|o| o.name.eq_ignore_ascii_case(column))
        .ok_or_else(|| {
            SourceError::Backend(format!("no Status option named {column:?} on project #{project_number}"))
        })?;
    Ok(ItemCoords {
        item_id: item.id,
        project_id: item.project.id,
        field_id: field.id,
        option_id: option.id.clone(),
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-ticket-github resolve_item project_item_query set_option_mutation`
Expected: PASS (the 6 new tests). If a struct/fixture mismatch fails deserialization, align the `LookupField`/fixture nesting per the note above. Then `cargo clippy -p gonzalo-ticket-github --all-targets -- -D warnings` clean (the new pure items are all exercised by tests, so no dead_code).

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-ticket-github/src/project_source.rs
git commit -m "feat(ticket-github): pure GraphQL builders + item resolver for board write-back"
```

---

## Task 3: Wire `set_state` + capabilities on `GitHubProjectSource`

**Files:**
- Modify: `crates/gonzalo-ticket-github/src/project_source.rs`
- Test: inline `#[cfg(test)]` in `project_source.rs`

- [ ] **Step 1: Write the failing test** — add to the `#[cfg(test)] mod tests` in `project_source.rs`

```rust
    #[test]
    fn board_source_advertises_push_capability() {
        let src = GitHubProjectSource::new("caliban-ai", 1, "tok", mapping()).unwrap();
        assert!(src.capabilities().push);
        assert!(!src.capabilities().comments);
    }

    #[tokio::test]
    async fn set_state_rejects_category_with_no_column() {
        // mapping() has an empty by_value, so no column maps to Done → Unsupported.
        let src = GitHubProjectSource::new("caliban-ai", 1, "tok", mapping()).unwrap();
        let err = src.set_state("caliban-ai/gonzalo#1", StateCategory::Done).await.unwrap_err();
        assert!(matches!(err, SourceError::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn with_write_targets_stores_overrides() {
        let mut t = BTreeMap::new();
        t.insert(StateCategory::Done, "Shipped".to_string());
        let src = GitHubProjectSource::new("caliban-ai", 1, "tok", mapping())
            .unwrap()
            .with_write_targets(t);
        assert_eq!(src.set_targets.get(&StateCategory::Done).map(String::as_str), Some("Shipped"));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gonzalo-ticket-github set_state with_write_targets board_source_advertises`
Expected: FAIL — `with_write_targets` / `set_targets` field / push capability not present.

- [ ] **Step 3: Implement** in `crates/gonzalo-ticket-github/src/project_source.rs`

Add the import for the reverse type at the top:

```rust
use gonzalo_ticket::{
    Capabilities, Cursor, Page, Result, SourceError, StateMapping, TicketSource,
};
use gonzalo_domain::{StateCategory, Ticket};
use std::collections::BTreeMap;
```
(Merge with existing imports; `Ticket` is already imported — keep one. `StateCategory` and `BTreeMap` are new to the non-test scope.)

Add the field to the struct:

```rust
pub struct GitHubProjectSource {
    client: reqwest::Client,
    endpoint: String,
    org: String,
    project_number: u32,
    token: String,
    mapping: StateMapping,
    set_targets: BTreeMap<StateCategory, String>,
}
```

Initialize it in `new` (add `set_targets: BTreeMap::new(),` to the constructed `Self`), and add the builder method to `impl GitHubProjectSource`:

```rust
    /// Set the category→column overrides used by `set_state` for boards where
    /// two columns share a category (the reverse of `state_map` is ambiguous).
    pub fn with_write_targets(mut self, targets: BTreeMap<StateCategory, String>) -> Self {
        self.set_targets = targets;
        self
    }

    /// POST a GraphQL body and return the parsed JSON value, surfacing transport
    /// errors as `Backend`.
    async fn post(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.client
            .post(&self.endpoint)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?
            .json()
            .await
            .map_err(be)
    }
```

Update `capabilities()` and add `set_state` in the `impl TicketSource`:

```rust
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            push: true,
            ..Capabilities::default()
        }
    }

    async fn set_state(&self, uid: &str, target: StateCategory) -> Result<()> {
        // 1. Resolve the target category back to a board column.
        let column = self
            .mapping
            .column_for(target, &self.set_targets)
            .map_err(|e| SourceError::Unsupported(reverse_reason(&e)))?;

        // 2. uid → owner/repo#number.
        let (owner, repo, number) = parse_board_uid(uid)?;

        // 3. Look up the issue's item on this project + the column's option id.
        let lookup: ItemLookupResponse =
            serde_json::from_value(self.post(&project_item_query(&owner, &repo, number)).await?)
                .map_err(be)?;
        let coords = resolve_item(lookup, self.project_number, &column)?;

        // 4. Mutate. The mutation response is checked for top-level errors.
        let resp = self
            .post(&set_option_mutation(
                &coords.project_id,
                &coords.item_id,
                &coords.field_id,
                &coords.option_id,
            ))
            .await?;
        if let Some(errors) = resp.get("errors").and_then(|e| e.as_array())
            && !errors.is_empty()
        {
            let msg = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(SourceError::Backend(format!("github graphql: {msg}")));
        }
        Ok(())
    }
```

Add the helper functions (near `be`):

```rust
/// `SourceError::Unsupported` needs a `&'static str`; map the reverse-mapping
/// failure kind to a stable reason string (the detail is in the error's Display,
/// logged by callers).
fn reverse_reason(e: &gonzalo_ticket::ReverseError) -> &'static str {
    match e {
        gonzalo_ticket::ReverseError::Unmapped(_) => "set_state: no column maps to target category",
        gonzalo_ticket::ReverseError::Ambiguous(_, _) => {
            "set_state: target category is ambiguous; configure set_targets"
        }
    }
}

/// Parse a board uid `owner/repo#number` into its parts.
fn parse_board_uid(uid: &str) -> Result<(String, String, u64)> {
    let (repo_path, num) = uid
        .rsplit_once('#')
        .ok_or_else(|| SourceError::Backend(format!("expected owner/repo#number, got {uid}")))?;
    let (owner, repo) = repo_path
        .split_once('/')
        .ok_or_else(|| SourceError::Backend(format!("expected owner/repo#number, got {uid}")))?;
    let number = num
        .parse::<u64>()
        .map_err(|_| SourceError::Backend(format!("bad issue number in uid {uid}")))?;
    Ok((owner.to_string(), repo.to_string(), number))
}
```

Update the module doc comment at the top of the file to note write-back is now supported (replace the "Phase 1: `capabilities()` is all-false" sentence with a note that `set_state` moves the card via the Status field).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-ticket-github`
Expected: PASS (all prior + the 3 new). Then `cargo clippy -p gonzalo-ticket-github --all-targets -- -D warnings` clean. Note: `parse_board_uid` is used by `set_state`; if clippy flags an unused helper, ensure `set_state` references it.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-ticket-github/src/project_source.rs
git commit -m "feat(ticket-github): GitHubProjectSource::set_state — move a Projects v2 card"
```

---

## Task 4: Config `set_targets`

**Files:**
- Modify: `crates/gonzalo-ticket-config/src/lib.rs`
- Test: inline `#[cfg(test)]` in `lib.rs`

- [ ] **Step 1: Write the failing test** — add to the `#[cfg(test)] mod tests` in `gonzalo-ticket-config/src/lib.rs`

```rust
    const WITH_TARGETS: &str = r#"
[[connection]]
name      = "caliban-ai-board"
provider  = "github-projects"
org       = "caliban-ai"
project   = 1
token_env = "TEST_TICKET_TOKEN"

[connection.state_map]
default = "open"
"Done"  = "done"
"Shipped" = "done"

[connection.set_targets]
done = "Shipped"
"#;

    #[test]
    fn parses_set_targets_into_categories() {
        let cfg = parse(WITH_TARGETS).unwrap();
        let c = &cfg.connections[0];
        let targets = write_targets(c).unwrap();
        assert_eq!(targets.get(&StateCategory::Done).map(String::as_str), Some("Shipped"));
    }

    #[test]
    fn set_targets_with_bad_category_errors() {
        let text = WITH_TARGETS.replace("done = \"Shipped\"", "finished = \"Shipped\"");
        let cfg = parse(&text).unwrap();
        assert!(matches!(
            write_targets(&cfg.connections[0]),
            Err(ConfigError::BadCategory { .. })
        ));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gonzalo-ticket-config set_targets`
Expected: FAIL — `set_targets` field / `write_targets` not found.

- [ ] **Step 3: Implement** in `crates/gonzalo-ticket-config/src/lib.rs`

Add the field to `Connection`:

```rust
    /// Optional category→column overrides for write-back (`set_state`), for
    /// boards where two columns map to the same category. Keys are category
    /// names (same vocabulary as `state_map` values).
    #[serde(default)]
    pub set_targets: BTreeMap<String, String>,
```

Add the helper (near `state_mapping`):

```rust
/// Parse a connection's `set_targets` (category-name → column) into typed
/// categories.
pub fn write_targets(conn: &Connection) -> Result<BTreeMap<StateCategory, String>, ConfigError> {
    let mut out = BTreeMap::new();
    for (cat, column) in &conn.set_targets {
        let parsed = parse_category(cat).ok_or_else(|| ConfigError::BadCategory {
            conn: conn.name.clone(),
            value: cat.clone(),
        })?;
        out.insert(parsed, column.clone());
    }
    Ok(out)
}
```

Make `parse_category` public (the CLI reuses it): change `fn parse_category` to `pub fn parse_category`.

Wire the overrides into `build_source` — in the `"github-projects"` arm, after building the source:

```rust
        "github-projects" => {
            let mapping = state_mapping(conn)?;
            let targets = write_targets(conn)?;
            let src = GitHubProjectSource::new(&conn.org, conn.project, token, mapping)
                .map_err(|e| ConfigError::Source(e.to_string()))?
                .with_write_targets(targets);
            Ok(Box::new(src))
        }
```

Ensure `StateCategory` is imported in this file (it already is — used by `parse_category`).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-ticket-config`
Expected: PASS (prior + 2 new). Then `cargo clippy -p gonzalo-ticket-config --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-ticket-config/src/lib.rs
git commit -m "feat(ticket-config): set_targets (category->column) for board write-back"
```

---

## Task 5: CLI `ticket move`

**Files:**
- Modify: `crates/gonzalo-cli/src/lib.rs`, `crates/gonzalo-cli/src/main.rs`, `tickets.example.toml`
- Test: inline `#[cfg(test)]` in `lib.rs`

- [ ] **Step 1: Write the failing test** — add to the `#[cfg(test)] mod tests` in `gonzalo-cli/src/lib.rs`

```rust
    #[tokio::test]
    async fn ticket_move_unknown_category_errors() {
        let cfg = TempDir::new().unwrap();
        let cfg_path = cfg.path().join("tickets.toml");
        std::fs::write(
            &cfg_path,
            "[[connection]]\nname=\"b\"\nprovider=\"github-projects\"\norg=\"o\"\nproject=1\ntoken_env=\"X\"\n",
        )
        .unwrap();
        // "frozen" is not a valid category → error before any network call.
        let err = ticket_move(&cfg_path, None, "o/r#1", "frozen").await.unwrap_err();
        assert!(err.to_string().contains("category"), "got {err}");
    }

    #[tokio::test]
    async fn ticket_move_requires_connection_when_many() {
        let cfg = TempDir::new().unwrap();
        let cfg_path = cfg.path().join("tickets.toml");
        std::fs::write(
            &cfg_path,
            "[[connection]]\nname=\"a\"\nprovider=\"github-projects\"\norg=\"o\"\nproject=1\ntoken_env=\"X\"\n\
             [[connection]]\nname=\"b\"\nprovider=\"github-projects\"\norg=\"o\"\nproject=2\ntoken_env=\"Y\"\n",
        )
        .unwrap();
        let err = ticket_move(&cfg_path, None, "o/r#1", "done").await.unwrap_err();
        assert!(err.to_string().contains("connection"), "got {err}");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gonzalo-cli ticket_move`
Expected: FAIL — `ticket_move` not found.

- [ ] **Step 3: Implement** — add to `crates/gonzalo-cli/src/lib.rs`

Add imports near the existing ticket imports:

```rust
use gonzalo_ticket_config::{Connection, parse_category};
```

Add the command function (before the tests module):

```rust
// ─── ticket move ─────────────────────────────────────────────────────────────

/// Move a board card to the column for `category`. Selects the connection named
/// `connection`, or the sole connection if there is exactly one.
pub async fn ticket_move(
    config_path: &Path,
    connection: Option<&str>,
    uid: &str,
    category: &str,
) -> Result<()> {
    let cat = parse_category(category)
        .ok_or_else(|| anyhow::anyhow!("unknown state category {category:?}"))?;
    let config = Config::load(config_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let conn = select_connection(&config.connections, connection)?;
    let source = gonzalo_ticket_config::build_source(conn).map_err(|e| anyhow::anyhow!("{e}"))?;
    source
        .set_state(uid, cat)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// Pick the requested connection by name, or the only one if unambiguous.
fn select_connection<'a>(
    connections: &'a [Connection],
    name: Option<&str>,
) -> Result<&'a Connection> {
    match name {
        Some(n) => connections
            .iter()
            .find(|c| c.name == n)
            .ok_or_else(|| anyhow::anyhow!("no connection named {n:?}")),
        None => match connections {
            [one] => Ok(one),
            [] => Err(anyhow::anyhow!("no connections configured")),
            _ => Err(anyhow::anyhow!(
                "multiple connections configured; pass --connection <name>"
            )),
        },
    }
}
```

> `parse_category` was made `pub` in Task 4. `Config`, `Path`, `anyhow` are already imported in this file. `set_state` is a `TicketSource` trait method; `gonzalo_ticket::TicketSource` is in scope transitively via the boxed source — if the call doesn't resolve, add `use gonzalo_ticket::TicketSource;`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p gonzalo-cli ticket_move`
Expected: PASS (2 new).

- [ ] **Step 5: Wire the subcommand** in `crates/gonzalo-cli/src/main.rs`

Add `ticket_move` to the import:

```rust
use gonzalo_cli::{get, list, migrate, status, sync_stores, ticket_move, ticket_sync};
```

Add a `Move` variant to `TicketCommands`:

```rust
    /// Move a board card to the column for a normalized state category.
    Move {
        /// Path to the tickets TOML config.
        #[arg(long, default_value = "tickets.toml")]
        config: PathBuf,
        /// Connection name (optional when only one is configured).
        #[arg(long)]
        connection: Option<String>,
        /// Ticket uid (owner/repo#number).
        uid: String,
        /// Target category: triage|backlog|open|in_progress|pending|done|canceled.
        category: String,
    },
```

Add the dispatch arm inside `Commands::Ticket { command } => match command { ... }`:

```rust
            TicketCommands::Move {
                config,
                connection,
                uid,
                category,
            } => {
                ticket_move(&config, connection.as_deref(), &uid, &category).await?;
                println!("moved {uid} → {category}");
            }
```

- [ ] **Step 6: Document `set_targets` in `tickets.example.toml`**

Append this commented block to `tickets.example.toml`:

```toml

# Optional: write-back overrides for `gonzalo ticket move`. Only needed when two
# columns map to the same category (the reverse of state_map is ambiguous).
# Keys are category names; values are the board column to move a card to.
# [connection.set_targets]
# done = "Done"
# in_progress = "In progress"
```

- [ ] **Step 7: Verify the command is wired**

Run: `cargo run -p gonzalo-cli -- ticket move --help`
Expected: usage showing `--config`, `--connection`, `<UID>`, `<CATEGORY>`.

- [ ] **Step 8: Commit**

```bash
git add crates/gonzalo-cli tickets.example.toml
git commit -m "feat(cli): gonzalo ticket move — set a board card's column"
```

---

## Task 6: Full gate + docs

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the README ticket section** — in `README.md`, find the `## Tickets` section and the `gonzalo-ticket-github` capability-table row.

In the capability table, update the github row to note board write-back:

```markdown
| `gonzalo-ticket-github` `[ticket-github]` | GitHub connectors: `GitHubSource` (REST issues, read + write-back); `GitHubProjectSource` (Projects v2 board over GraphQL, read + card move) |
```

In the `## Tickets` prose, add a line after the sync examples:

```markdown
Move a card to a column (write-back):

```bash
gonzalo ticket move --config tickets.toml "caliban-ai/gonzalo#15" in_progress
```
```
(Use real triple-backticks in the file.)

- [ ] **Step 2: Run the full local gate** (mirrors CI)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features
scripts/coverage.sh
```
Expected: all pass; coverage ≥ 80%. If fmt fails, run `cargo fmt --all` and re-stage. If coverage dips, add focused pure tests (more `column_for` / `resolve_item` cases) — no network tests.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs(ticket): board write-back in README"
```

---

## Self-Review

**Spec coverage:**
- Reverse mapping (invert + override, error on ambiguity) → Task 1 (`column_for`, `ReverseError`). ✓
- `set_state` via lookup + mutation, GraphQL error surfacing → Tasks 2 (builders/extractor) + 3 (wiring). ✓
- `capabilities().push = true`, comments false → Task 3. ✓
- `with_write_targets` non-breaking builder → Task 3. ✓
- Config `set_targets` + `parse_category` pub + `build_source` wiring → Task 4. ✓
- CLI `ticket move` + connection selection + reuse `parse_category` → Task 5. ✓
- Example config doc + README → Tasks 5, 6. ✓
- Out of scope (daemon RPC, board comments, formal conformance) → not built. ✓
- Live verify (move + restore) → done via the verify skill after this plan, not a task here (destructive, manual). ✓

**Placeholder scan:** none — every step has concrete code. The one flagged risk (the exact GraphQL `field` nesting GitHub returns) is called out with a concrete fallback in Task 2's note, and the fixture+structs are self-consistent for the unit tests; the live verify confirms the real shape.

**Type consistency:** `column_for(category, &BTreeMap<StateCategory,String>) -> Result<String, ReverseError>` is defined in Task 1 and used identically in Task 3. `ItemCoords { item_id, project_id, field_id, option_id }` defined in Task 2, consumed in Task 3. `with_write_targets(BTreeMap<StateCategory,String>)` defined in Task 3, called in Task 4. `write_targets`/`parse_category` defined in Task 4, used in Tasks 4/5. `ticket_move(&Path, Option<&str>, &str, &str)` defined in Task 5, called in main.rs Task 5.

**Known risk (carried, not a placeholder):** the live Projects v2 `fieldValueByName → field` GraphQL nesting may differ from the fixture; Task 2's note gives the exact adjustment, and the final live verify (move + restore a real card) is the proof. If the shape differs, only the `LookupField` structs + fixture change — the extractor and wiring are unaffected.
