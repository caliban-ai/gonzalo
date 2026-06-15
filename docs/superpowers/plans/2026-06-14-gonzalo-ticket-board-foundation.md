# Gonzalo Ticket Board Foundation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Gonzalo a config-driven path that reads the shared `caliban-ai` GitHub Projects v2 board and persists its items as ticket `Record`s carrying their board column as a normalized `State.category`, exposed via library, CLI, and daemon.

**Architecture:** A pure ingest engine in `gonzalo-ticket` (source → Records, idempotent via content hash) is reused by all surfaces. A new GraphQL `GitHubProjectSource` lives in `gonzalo-ticket-github`. A new `gonzalo-ticket-config` crate parses a multi-connection TOML file and is the registry that turns each connection into a `Box<dyn TicketSource>` — it sits *above* the connector crates to avoid a dependency cycle. The CLI and daemon both call the registry + ingest engine.

**Tech Stack:** Rust 2024, `async-trait`, `reqwest` (rustls), `serde`/`serde_json`, `toml`, `thiserror`, `tokio`, `tonic`/`prost` (gRPC), `axum` (HTTP).

**Spec:** `docs/superpowers/specs/2026-06-14-gonzalo-ticket-board-foundation-design.md`

---

## File Structure

| File | Responsibility | Task |
|------|----------------|------|
| `crates/gonzalo-ticket/src/ingest.rs` (create) | Ingest engine: pull a `TicketSource`, upsert each `Ticket` as a `Record` with optimistic concurrency, count outcomes. No connector deps. | 1 |
| `crates/gonzalo-ticket/src/lib.rs` (modify) | Export `ingest`, `IngestSummary`, `IngestError`. | 1 |
| `crates/gonzalo-ticket/Cargo.toml` (modify) | Add `serde`; dev-deps `gonzalo-store-fs`, `tempfile`. | 1 |
| `crates/gonzalo-ticket-github/src/project_mapping.rs` (create) | Pure GraphQL-JSON → `Ticket` mapping for Projects v2 items; skips drafts/PRs; resolves status via `StateMapping`. | 2 |
| `crates/gonzalo-ticket-github/src/project_source.rs` (create) | `GitHubProjectSource`: GraphQL HTTP `TicketSource`, paginates the board internally. | 3 |
| `crates/gonzalo-ticket-github/src/lib.rs` (modify) | Declare new modules; export `GitHubProjectSource`. | 2,3 |
| `crates/gonzalo-ticket-config/` (create crate) | Parse multi-connection TOML; registry `provider` → `Box<dyn TicketSource>`. | 4 |
| `Cargo.toml` (modify) | Add `gonzalo-ticket-config` member + workspace deps `gonzalo-ticket-config`, `toml`. | 4 |
| `crates/gonzalo-cli/src/lib.rs` (modify) | `ticket_sync` command function. | 5 |
| `crates/gonzalo-cli/src/main.rs` (modify) | `ticket sync/list/get` subcommands. | 5 |
| `crates/gonzalo-cli/Cargo.toml` (modify) | Add `gonzalo-ticket`, `gonzalo-ticket-config`. | 5 |
| `crates/gonzalo-proto/proto/gonzalo.proto` (modify) | `TicketSync` RPC + messages. | 6 |
| `crates/gonzalo-server/src/service.rs` (modify) | `Service::ticket_sync`. | 6 |
| `crates/gonzalo-server/src/grpc.rs` (modify) | gRPC `ticket_sync` handler. | 6 |
| `crates/gonzalo-server/src/http.rs` (modify) | HTTP `POST /v1/tickets/sync` handler. | 6 |
| `crates/gonzalo-server/Cargo.toml` (modify) | Add `gonzalo-ticket`, `gonzalo-ticket-config`, `serde_json`. | 6 |

**Design note on pagination:** Projects v2 has no reliable "changed since" filter, so the board source pages **internally to completion** on each `fetch_changed` and returns `next: Cursor::default()`. Re-sync stays cheap because the ingest engine writes only when a ticket's body hash changed. This is a deliberate, documented simplification of the spec's "cursor-threaded" wording — same end state (full board read), simpler control flow.

---

## Task 1: Ingest engine in `gonzalo-ticket`

**Files:**
- Create: `crates/gonzalo-ticket/src/ingest.rs`
- Modify: `crates/gonzalo-ticket/src/lib.rs`, `crates/gonzalo-ticket/Cargo.toml`
- Test: inline `#[cfg(test)]` in `crates/gonzalo-ticket/src/ingest.rs`

- [ ] **Step 1: Add deps to `crates/gonzalo-ticket/Cargo.toml`**

Add `serde` to `[dependencies]` (for `IngestSummary` derives) and the store + tempfile dev-deps:

```toml
[dependencies]
gonzalo-core   = { workspace = true }
gonzalo-domain = { workspace = true }
async-trait    = { workspace = true }
thiserror      = { workspace = true }
serde          = { workspace = true }

[dev-dependencies]
tokio            = { workspace = true }
gonzalo-store-fs = { workspace = true }
tempfile         = { workspace = true }
```

- [ ] **Step 2: Write the failing test** in a new file `crates/gonzalo-ticket/src/ingest.rs`

```rust
//! The ingest engine: pull a [`TicketSource`] and persist each [`Ticket`] as a
//! `Record`, using optimistic concurrency (ADR 0005). Re-sync is idempotent —
//! unchanged tickets (same body hash) are skipped, so a full board re-scan is
//! cheap. Depends only on the trait + `Store`, never on a concrete connector.

use crate::{TicketSource, record_key};
use gonzalo_core::{
    ContentHash, Identity, Meta, PutResult, Record, Revision, Store,
};
use gonzalo_domain::{RecordCodec, Ticket};
use std::collections::BTreeMap;

/// How many tickets a sync created, updated, or left untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IngestSummary {
    pub imported: usize,
    pub updated: usize,
    pub unchanged: usize,
}

/// Failures during ingest.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("ticket source: {0}")]
    Source(#[from] crate::SourceError),
    #[error("store: {0}")]
    Store(#[from] gonzalo_core::CoreError),
    #[error("write conflict on {0}")]
    Conflict(String),
}

/// Pull all changed tickets from `source` and upsert them into `store`,
/// attributing writes to `author`.
pub async fn ingest(
    source: &dyn TicketSource,
    store: &dyn Store,
    author: &str,
) -> Result<IngestSummary, IngestError> {
    let mut summary = IngestSummary::default();
    let mut cursor = crate::Cursor::default();
    loop {
        let page = source.fetch_changed(&cursor).await?;
        for ticket in &page.tickets {
            match upsert(store, ticket, author).await? {
                Outcome::Imported => summary.imported += 1,
                Outcome::Updated => summary.updated += 1,
                Outcome::Unchanged => summary.unchanged += 1,
            }
        }
        if page.next.0.is_none() || page.next == cursor {
            break;
        }
        cursor = page.next;
    }
    Ok(summary)
}

enum Outcome {
    Imported,
    Updated,
    Unchanged,
}

async fn upsert(store: &dyn Store, ticket: &Ticket, author: &str) -> Result<Outcome, IngestError> {
    let key = record_key(ticket);
    let body = ticket.to_body()?;
    let new_hash = ContentHash::of(body.bytes());

    let existing = store.get(&key).await?;
    if let Some(rec) = &existing {
        if rec.revision.hash == new_hash {
            return Ok(Outcome::Unchanged);
        }
    }

    let expected: Option<Revision> = existing.as_ref().map(|r| r.revision.clone());
    let revision = match &expected {
        Some(prev) => prev.next(body.bytes()),
        None => Revision::initial(body.bytes()),
    };
    let record = Record {
        key: key.clone(),
        kind: Ticket::KIND,
        revision,
        parent: expected.clone(),
        body,
        meta: Meta {
            author: Identity::new(author),
            origin_system: "ticket-ingest".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: vec![],
    };
    match store.put(record, expected).await? {
        PutResult::Committed(_) => Ok(if existing.is_some() {
            Outcome::Updated
        } else {
            Outcome::Imported
        }),
        PutResult::Conflict(_) => Err(IngestError::Conflict(format!("{key}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemorySource;
    use gonzalo_domain::{BodyFormat, Provider, State, StateCategory, TicketBody};
    use gonzalo_store_fs::FsStore;

    fn ticket(uid: &str, title: &str) -> Ticket {
        Ticket {
            provider: Provider::GitHub,
            uid: uid.into(),
            display: "#1".into(),
            item_type: "issue".into(),
            title: title.into(),
            state: State {
                category: StateCategory::Open,
                resolution: None,
                raw_name: "Todo".into(),
                raw_id: None,
            },
            priority: None,
            actors: vec![],
            labels: vec![],
            containers: vec![],
            links: vec![],
            body: TicketBody {
                markdown: String::new(),
                format: BodyFormat::Markdown,
                raw: None,
            },
            fields: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn imports_then_is_idempotent_then_updates() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStore::new(dir.path());

        // First sync: two new tickets imported.
        let src = InMemorySource::new(vec![ticket("a", "A"), ticket("b", "B")]);
        let s1 = ingest(&src, &store, "tester").await.unwrap();
        assert_eq!(s1, IngestSummary { imported: 2, updated: 0, unchanged: 0 });

        // Re-sync identical: everything unchanged, no writes.
        let s2 = ingest(&src, &store, "tester").await.unwrap();
        assert_eq!(s2, IngestSummary { imported: 0, updated: 0, unchanged: 2 });

        // One ticket's title changed: exactly one update.
        let src2 = InMemorySource::new(vec![ticket("a", "A2"), ticket("b", "B")]);
        let s3 = ingest(&src2, &store, "tester").await.unwrap();
        assert_eq!(s3, IngestSummary { imported: 0, updated: 1, unchanged: 1 });
    }
}
```

- [ ] **Step 3: Wire the module into `crates/gonzalo-ticket/src/lib.rs`**

Add the module declaration after the existing `pub mod source;` line:

```rust
pub mod ingest;
```

And add to the existing `pub use` block:

```rust
pub use ingest::{IngestError, IngestSummary, ingest};
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p gonzalo-ticket ingest`
Expected: PASS (`imports_then_is_idempotent_then_updates`). If `InMemorySource::new` signature differs, it takes `Vec<Ticket>` (confirmed in `mock.rs`).

- [ ] **Step 5: Commit**

```bash
git add crates/gonzalo-ticket/Cargo.toml crates/gonzalo-ticket/src/ingest.rs crates/gonzalo-ticket/src/lib.rs
git commit -m "feat(ticket): ingest engine — source to Records, idempotent via content hash"
```

---

## Task 2: Projects v2 pure mapping in `gonzalo-ticket-github`

**Files:**
- Create: `crates/gonzalo-ticket-github/src/project_mapping.rs`
- Modify: `crates/gonzalo-ticket-github/src/lib.rs`
- Test: inline `#[cfg(test)]` in `project_mapping.rs`

- [ ] **Step 1: Write the failing test** in a new file `crates/gonzalo-ticket-github/src/project_mapping.rs`

```rust
//! Pure mapping from GitHub Projects v2 GraphQL item JSON to the canonical
//! [`Ticket`]. The board's Status column is a *native single-select field*
//! (`StateSignal::NativeStatus`), so unlike the REST issue connector this takes
//! a per-connection [`StateMapping`] (ADR 0010). Draft items and pull requests
//! have no stable issue uid and are skipped (the caller `filter_map`s `None`).

use gonzalo_domain::{
    Actor, ActorRole, BodyFormat, Container, Provider, State, Ticket, TicketBody,
};
use gonzalo_ticket::StateMapping;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub(crate) struct GqlResponse {
    pub data: GqlData,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlData {
    pub organization: GqlOrg,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlOrg {
    #[serde(rename = "projectV2")]
    pub project: GqlProject,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlProject {
    pub items: GqlItems,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlItems {
    #[serde(rename = "pageInfo")]
    pub page_info: GqlPageInfo,
    pub nodes: Vec<GqlItem>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlPageInfo {
    #[serde(rename = "hasNextPage")]
    pub has_next_page: bool,
    #[serde(rename = "endCursor")]
    pub end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlItem {
    /// The "Status" single-select field value; `None` when the card has no
    /// status set, or `name: None` when the field isn't a single-select.
    #[serde(rename = "fieldValueByName")]
    pub status: Option<GqlStatus>,
    pub content: Option<GqlContent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlStatus {
    pub name: Option<String>,
    #[serde(rename = "optionId")]
    pub option_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlContent {
    #[serde(rename = "__typename")]
    pub typename: String,
    pub number: Option<u64>,
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    pub repository: Option<GqlRepo>,
    #[serde(default)]
    pub labels: Option<GqlNodes<GqlLabel>>,
    #[serde(default)]
    pub assignees: Option<GqlNodes<GqlUser>>,
    #[serde(default)]
    pub author: Option<GqlUser>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlRepo {
    #[serde(rename = "nameWithOwner")]
    pub name_with_owner: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlNodes<T> {
    pub nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlLabel {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlUser {
    pub login: String,
}

/// Map one board item to a [`Ticket`], or `None` if it is a draft / PR / has no
/// linked issue. `mapping` resolves the Status column name to a category.
pub(crate) fn item_to_ticket(item: &GqlItem, mapping: &StateMapping) -> Option<Ticket> {
    let content = item.content.as_ref()?;
    if content.typename != "Issue" {
        return None; // draft cards and pull requests are skipped
    }
    let number = content.number?;
    let repo = content.repository.as_ref()?.name_with_owner.clone();

    let raw_status = item
        .status
        .as_ref()
        .and_then(|s| s.name.clone())
        .unwrap_or_default();
    let category = mapping.category_of(&raw_status);

    let mut actors: Vec<Actor> = content
        .assignees
        .as_ref()
        .map(|a| {
            a.nodes
                .iter()
                .map(|u| Actor {
                    role: ActorRole::Assignee,
                    handle: u.login.clone(),
                    display: None,
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(u) = &content.author {
        actors.push(Actor {
            role: ActorRole::Submitter,
            handle: u.login.clone(),
            display: None,
        });
    }

    Some(Ticket {
        provider: Provider::GitHub,
        uid: format!("{repo}#{number}"),
        display: format!("#{number}"),
        item_type: "issue".into(),
        title: content.title.clone().unwrap_or_default(),
        state: State {
            category,
            resolution: None,
            raw_name: raw_status,
            raw_id: item.status.as_ref().and_then(|s| s.option_id.clone()),
        },
        priority: None,
        actors,
        labels: content
            .labels
            .as_ref()
            .map(|l| l.nodes.iter().map(|x| x.name.clone()).collect())
            .unwrap_or_default(),
        containers: vec![Container {
            kind: "repo".into(),
            id: repo,
            name: None,
            primary: true,
        }],
        links: vec![],
        body: TicketBody {
            markdown: content.body.clone().unwrap_or_default(),
            format: BodyFormat::Markdown,
            raw: None,
        },
        fields: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_domain::StateCategory;
    use gonzalo_ticket::StateSignal;

    fn board_mapping() -> StateMapping {
        let mut by_value = BTreeMap::new();
        by_value.insert("Todo".into(), StateCategory::Open);
        by_value.insert("In Progress".into(), StateCategory::InProgress);
        by_value.insert("Blocked".into(), StateCategory::Pending);
        by_value.insert("Done".into(), StateCategory::Done);
        StateMapping {
            signal: StateSignal::NativeStatus,
            by_value,
            default: StateCategory::Open,
        }
    }

    const PAGE: &str = r#"{
      "data": { "organization": { "projectV2": { "items": {
        "pageInfo": { "hasNextPage": false, "endCursor": "Y3Vyc29yOjE=" },
        "nodes": [
          {
            "fieldValueByName": { "name": "In Progress", "optionId": "abc123" },
            "content": {
              "__typename": "Issue",
              "number": 18,
              "title": "ticket capability layer",
              "body": "do the thing",
              "repository": { "nameWithOwner": "caliban-ai/gonzalo" },
              "labels": { "nodes": [{ "name": "kind/feat" }] },
              "assignees": { "nodes": [{ "login": "johnford2002" }] },
              "author": { "login": "reporter1" }
            }
          },
          {
            "fieldValueByName": null,
            "content": { "__typename": "DraftIssue", "title": "just a draft" }
          },
          {
            "fieldValueByName": { "name": "Done", "optionId": "z9" },
            "content": { "__typename": "PullRequest", "number": 21, "title": "a PR",
              "repository": { "nameWithOwner": "caliban-ai/gonzalo" } }
          }
        ]
      } } } }
    }"#;

    #[test]
    fn maps_issue_items_and_skips_drafts_and_prs() {
        let resp: GqlResponse = serde_json::from_str(PAGE).unwrap();
        let m = board_mapping();
        let tickets: Vec<Ticket> = resp
            .data
            .organization
            .project
            .items
            .nodes
            .iter()
            .filter_map(|n| item_to_ticket(n, &m))
            .collect();

        assert_eq!(tickets.len(), 1, "draft and PR are skipped");
        let t = &tickets[0];
        assert_eq!(t.uid, "caliban-ai/gonzalo#18");
        assert_eq!(t.display, "#18");
        assert_eq!(t.state.category, StateCategory::InProgress);
        assert_eq!(t.state.raw_name, "In Progress");
        assert_eq!(t.state.raw_id.as_deref(), Some("abc123"));
        assert_eq!(t.labels, vec!["kind/feat"]);
        assert_eq!(t.actors[0].role, ActorRole::Assignee);
        assert_eq!(t.actors[0].handle, "johnford2002");
        assert!(t.actors.iter().any(|a| a.role == ActorRole::Submitter));
        assert_eq!(t.containers[0].id, "caliban-ai/gonzalo");
    }

    #[test]
    fn unknown_status_falls_back_to_default() {
        let item = GqlItem {
            status: Some(GqlStatus { name: Some("Weird".into()), option_id: None }),
            content: Some(GqlContent {
                typename: "Issue".into(),
                number: Some(7),
                title: Some("x".into()),
                body: None,
                repository: Some(GqlRepo { name_with_owner: "caliban-ai/gonzalo".into() }),
                labels: None,
                assignees: None,
                author: None,
            }),
        };
        let t = item_to_ticket(&item, &board_mapping()).unwrap();
        assert_eq!(t.state.category, StateCategory::Open); // mapping default
    }
}
```

- [ ] **Step 2: Declare the module in `crates/gonzalo-ticket-github/src/lib.rs`**

Add after the existing `mod mapping;` line:

```rust
mod project_mapping;
```

(The public `GitHubProjectSource` export is added in Task 3.)

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p gonzalo-ticket-github project_mapping`
Expected: PASS — `maps_issue_items_and_skips_drafts_and_prs`, `unknown_status_falls_back_to_default`.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-ticket-github/src/project_mapping.rs crates/gonzalo-ticket-github/src/lib.rs
git commit -m "feat(ticket-github): pure Projects v2 item -> Ticket mapping"
```

---

## Task 3: `GitHubProjectSource` GraphQL source

**Files:**
- Create: `crates/gonzalo-ticket-github/src/project_source.rs`
- Modify: `crates/gonzalo-ticket-github/src/lib.rs`
- Test: inline `#[cfg(test)]` in `project_source.rs`

- [ ] **Step 1: Write the failing test** in a new file `crates/gonzalo-ticket-github/src/project_source.rs`

```rust
//! A read-only [`TicketSource`] over the GitHub **GraphQL** API, reading the
//! org Projects v2 board. Phase 1: `capabilities()` is all-false. The board has
//! no reliable "changed since" filter, so `fetch_changed` pages the whole board
//! to completion and returns an empty `next` cursor; cheap re-sync is the
//! ingest engine's job (content-hash dedup).

use crate::project_mapping::{GqlResponse, item_to_ticket};
use async_trait::async_trait;
use gonzalo_domain::Ticket;
use gonzalo_ticket::{Capabilities, Cursor, Page, Result, SourceError, StateMapping, TicketSource};

const GRAPHQL_URL: &str = "https://api.github.com/graphql";

/// Reads issues on an org's Projects v2 board, with their Status column mapped
/// to a normalized state category via [`StateMapping`].
pub struct GitHubProjectSource {
    client: reqwest::Client,
    endpoint: String,
    org: String,
    project_number: u32,
    token: String,
    mapping: StateMapping,
}

impl GitHubProjectSource {
    /// Create a board source for `org` / project `number`, authenticating with
    /// `token`, resolving Status via `mapping`.
    pub fn new(
        org: impl Into<String>,
        number: u32,
        token: impl Into<String>,
        mapping: StateMapping,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("gonzalo-ticket-github")
            .build()
            .map_err(be)?;
        Ok(Self {
            client,
            endpoint: GRAPHQL_URL.to_string(),
            org: org.into(),
            project_number: number,
            token: token.into(),
            mapping,
        })
    }

    /// Page the whole board into a flat list of tickets.
    async fn fetch_all(&self) -> Result<Vec<Ticket>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let body = graphql_body(&self.org, self.project_number, after.as_deref());
            let resp = self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await
                .map_err(be)?
                .error_for_status()
                .map_err(be)?;
            let parsed: GqlResponse = resp.json().await.map_err(be)?;
            let items = parsed.data.organization.project.items;
            out.extend(items.nodes.iter().filter_map(|n| item_to_ticket(n, &self.mapping)));
            if items.page_info.has_next_page {
                after = items.page_info.end_cursor;
                if after.is_none() {
                    break; // defensive: hasNextPage but no cursor
                }
            } else {
                break;
            }
        }
        Ok(out)
    }
}

/// Build the GraphQL request body (query + variables). Pure, so it is unit-
/// testable without a network.
pub(crate) fn graphql_body(org: &str, number: u32, after: Option<&str>) -> serde_json::Value {
    const QUERY: &str = r#"
query($org: String!, $number: Int!, $cursor: String) {
  organization(login: $org) {
    projectV2(number: $number) {
      items(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          fieldValueByName(name: "Status") {
            ... on ProjectV2ItemFieldSingleSelectValue { name optionId }
          }
          content {
            __typename
            ... on Issue {
              number title body
              repository { nameWithOwner }
              labels(first: 20) { nodes { name } }
              assignees(first: 10) { nodes { login } }
              author { login }
            }
          }
        }
      }
    }
  }
}"#;
    serde_json::json!({
        "query": QUERY,
        "variables": { "org": org, "number": number, "cursor": after },
    })
}

#[async_trait]
impl TicketSource for GitHubProjectSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default() // read-only in phase 1
    }

    async fn fetch_changed(&self, _cursor: &Cursor) -> Result<Page> {
        let tickets = self.fetch_all().await?;
        Ok(Page { tickets, next: Cursor::default() })
    }

    async fn get(&self, uid: &str) -> Result<Ticket> {
        self.fetch_all()
            .await?
            .into_iter()
            .find(|t| t.uid == uid)
            .ok_or_else(|| SourceError::Backend(format!("ticket {uid} not found on board")))
    }
}

fn be<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Backend(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_domain::StateCategory;
    use std::collections::BTreeMap;

    fn mapping() -> StateMapping {
        StateMapping {
            signal: gonzalo_ticket::StateSignal::NativeStatus,
            by_value: BTreeMap::new(),
            default: StateCategory::Open,
        }
    }

    #[test]
    fn graphql_body_carries_org_number_and_cursor() {
        let b = graphql_body("caliban-ai", 1, Some("CUR"));
        assert_eq!(b["variables"]["org"], "caliban-ai");
        assert_eq!(b["variables"]["number"], 1);
        assert_eq!(b["variables"]["cursor"], "CUR");
        assert!(b["query"].as_str().unwrap().contains("projectV2"));
    }

    #[test]
    fn null_cursor_serializes_for_first_page() {
        let b = graphql_body("caliban-ai", 1, None);
        assert!(b["variables"]["cursor"].is_null());
    }

    #[test]
    fn constructs_a_read_only_source() {
        let src = GitHubProjectSource::new("caliban-ai", 1, "tok", mapping()).unwrap();
        assert_eq!(src.org, "caliban-ai");
        assert_eq!(src.project_number, 1);
        assert!(!src.capabilities().push);
    }
}
```

- [ ] **Step 2: Export the source from `crates/gonzalo-ticket-github/src/lib.rs`**

Add the module declaration next to the others and extend the public export:

```rust
mod project_source;

pub use project_source::GitHubProjectSource;
```

(Keep the existing `pub use source::GitHubSource;`.)

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-ticket-github project_source`
Expected: PASS — `graphql_body_carries_org_number_and_cursor`, `null_cursor_serializes_for_first_page`, `constructs_a_read_only_source`.

- [ ] **Step 4: Commit**

```bash
git add crates/gonzalo-ticket-github/src/project_source.rs crates/gonzalo-ticket-github/src/lib.rs
git commit -m "feat(ticket-github): GitHubProjectSource — read-only Projects v2 board over GraphQL"
```

---

## Task 4: `gonzalo-ticket-config` crate (config + registry)

**Files:**
- Create: `crates/gonzalo-ticket-config/Cargo.toml`, `crates/gonzalo-ticket-config/src/lib.rs`
- Modify: `Cargo.toml` (workspace members + deps)
- Test: inline `#[cfg(test)]` in `lib.rs`

- [ ] **Step 1: Add the `toml` workspace dependency** in the root `Cargo.toml` under `[workspace.dependencies]`

```toml
toml = "0.8"
```

- [ ] **Step 2: Register the new crate** in the root `Cargo.toml`

Add to `members` (next to the other ticket crates):

```toml
    "crates/gonzalo-ticket-config",
```

Add to `[workspace.dependencies]` (next to the other ticket entries):

```toml
gonzalo-ticket-config = { path = "crates/gonzalo-ticket-config" }
```

- [ ] **Step 3: Create `crates/gonzalo-ticket-config/Cargo.toml`**

```toml
[package]
name = "gonzalo-ticket-config"
description = "Multi-connection ticket config + provider registry for gonzalo"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true
rust-version.workspace = true
repository.workspace = true

[dependencies]
gonzalo-domain        = { workspace = true }
gonzalo-ticket        = { workspace = true }
gonzalo-ticket-github = { workspace = true }
serde      = { workspace = true }
toml       = { workspace = true }
thiserror  = { workspace = true }

[lints]
workspace = true
```

- [ ] **Step 4: Write the failing test + implementation** in `crates/gonzalo-ticket-config/src/lib.rs`

```rust
//! Multi-connection ticket config and the provider registry (ADR 0010).
//!
//! A `tickets.toml` holds an array of `[[connection]]` tables. This crate parses
//! it and is the **registry** that turns each connection into a
//! `Box<dyn TicketSource>` — it sits *above* the connector crates, which would
//! otherwise be a dependency cycle (they depend on `gonzalo-ticket` for the
//! trait). Secrets are referenced by env-var name, never stored in the file.

use gonzalo_domain::StateCategory;
use gonzalo_ticket::{StateMapping, StateSignal, TicketSource};
use gonzalo_ticket_github::GitHubProjectSource;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Top-level config: a list of connections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(rename = "connection", default)]
    pub connections: Vec<Connection>,
}

/// One ticket connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub name: String,
    /// Provider key in the registry, e.g. `"github-projects"`.
    pub provider: String,
    pub org: String,
    pub project: u32,
    /// Name of the env var holding the access token (never the token itself).
    pub token_env: String,
    /// Status-name → category map. The reserved key `"default"` sets the
    /// fallback category; all other keys are board column names.
    #[serde(default)]
    pub state_map: BTreeMap<String, String>,
}

/// Config / registry failures.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config {0}: {1}")]
    Read(String, String),
    #[error("parsing config: {0}")]
    Parse(String),
    #[error("connection {conn}: env var {var} is not set")]
    MissingEnv { conn: String, var: String },
    #[error("connection {conn}: unknown provider {provider}")]
    UnknownProvider { conn: String, provider: String },
    #[error("connection {conn}: unknown state category {value:?}")]
    BadCategory { conn: String, value: String },
    #[error("building source: {0}")]
    Source(String),
}

/// Load and parse a `tickets.toml` from disk.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Read(path.display().to_string(), e.to_string()))?;
    parse(&text)
}

/// Parse config from a TOML string.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
}

impl Config {
    /// Build a live `TicketSource` for each connection.
    pub fn sources(&self) -> Result<Vec<(String, Box<dyn TicketSource>)>, ConfigError> {
        self.connections
            .iter()
            .map(|c| Ok((c.name.clone(), build_source(c)?)))
            .collect()
    }
}

/// The registry: map a connection's `provider` to a constructed source.
pub fn build_source(conn: &Connection) -> Result<Box<dyn TicketSource>, ConfigError> {
    let token = std::env::var(&conn.token_env).map_err(|_| ConfigError::MissingEnv {
        conn: conn.name.clone(),
        var: conn.token_env.clone(),
    })?;
    match conn.provider.as_str() {
        "github-projects" => {
            let mapping = state_mapping(conn)?;
            let src = GitHubProjectSource::new(&conn.org, conn.project, token, mapping)
                .map_err(|e| ConfigError::Source(e.to_string()))?;
            Ok(Box::new(src))
        }
        other => Err(ConfigError::UnknownProvider {
            conn: conn.name.clone(),
            provider: other.to_string(),
        }),
    }
}

fn state_mapping(conn: &Connection) -> Result<StateMapping, ConfigError> {
    let mut by_value = BTreeMap::new();
    let mut default = StateCategory::Open;
    for (k, v) in &conn.state_map {
        let cat = parse_category(v).ok_or_else(|| ConfigError::BadCategory {
            conn: conn.name.clone(),
            value: v.clone(),
        })?;
        if k == "default" {
            default = cat;
        } else {
            by_value.insert(k.clone(), cat);
        }
    }
    Ok(StateMapping { signal: StateSignal::NativeStatus, by_value, default })
}

fn parse_category(s: &str) -> Option<StateCategory> {
    Some(match s {
        "triage" => StateCategory::Triage,
        "backlog" => StateCategory::Backlog,
        "open" => StateCategory::Open,
        "in_progress" => StateCategory::InProgress,
        "pending" => StateCategory::Pending,
        "done" => StateCategory::Done,
        "canceled" => StateCategory::Canceled,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[connection]]
name      = "caliban-ai-board"
provider  = "github-projects"
org       = "caliban-ai"
project   = 1
token_env = "TEST_TICKET_TOKEN"

[connection.state_map]
default       = "open"
"Todo"        = "open"
"In Progress" = "in_progress"
"Done"        = "done"
"#;

    #[test]
    fn parses_a_connection() {
        let cfg = parse(SAMPLE).unwrap();
        assert_eq!(cfg.connections.len(), 1);
        let c = &cfg.connections[0];
        assert_eq!(c.provider, "github-projects");
        assert_eq!(c.org, "caliban-ai");
        assert_eq!(c.project, 1);
        assert_eq!(c.state_map.get("In Progress").map(String::as_str), Some("in_progress"));
    }

    #[test]
    fn state_mapping_pulls_out_default_and_entries() {
        let cfg = parse(SAMPLE).unwrap();
        let m = state_mapping(&cfg.connections[0]).unwrap();
        assert_eq!(m.signal, StateSignal::NativeStatus);
        assert_eq!(m.default, StateCategory::Open);
        assert_eq!(m.category_of("In Progress"), StateCategory::InProgress);
        assert_eq!(m.category_of("Done"), StateCategory::Done);
        assert_eq!(m.category_of("Nonexistent"), StateCategory::Open);
    }

    #[test]
    fn missing_env_var_is_reported() {
        // Ensure the var is unset for this test.
        unsafe { std::env::remove_var("TEST_TICKET_TOKEN") };
        let cfg = parse(SAMPLE).unwrap();
        let err = build_source(&cfg.connections[0]).unwrap_err();
        assert!(matches!(err, ConfigError::MissingEnv { .. }));
    }

    #[test]
    fn unknown_provider_is_reported() {
        let text = SAMPLE.replace("github-projects", "bogus-tracker");
        let cfg = parse(&text).unwrap();
        // Token must be present so we reach the provider match.
        unsafe { std::env::set_var("TEST_TICKET_TOKEN", "x") };
        let err = build_source(&cfg.connections[0]).unwrap_err();
        assert!(matches!(err, ConfigError::UnknownProvider { .. }));
        unsafe { std::env::remove_var("TEST_TICKET_TOKEN") };
    }

    #[test]
    fn bad_category_is_reported() {
        let text = SAMPLE.replace(r#""Done"        = "done""#, r#""Done"        = "finished""#);
        let cfg = parse(&text).unwrap();
        let err = state_mapping(&cfg.connections[0]).unwrap_err();
        assert!(matches!(err, ConfigError::BadCategory { .. }));
    }
}
```

> **Note on `std::env::set_var`/`remove_var`:** these are `unsafe` in edition 2024 (the workspace edition). The `unsafe { }` blocks above are required and correct.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p gonzalo-ticket-config`
Expected: PASS — all five tests. If env-var tests are flaky under parallel runs, run with `--test-threads=1`; the var name `TEST_TICKET_TOKEN` is unique to this module to minimize collisions.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/gonzalo-ticket-config
git commit -m "feat(ticket-config): multi-connection TOML config + provider registry"
```

---

## Task 5: CLI `ticket` subcommands

**Files:**
- Modify: `crates/gonzalo-cli/Cargo.toml`, `crates/gonzalo-cli/src/lib.rs`, `crates/gonzalo-cli/src/main.rs`
- Test: inline `#[cfg(test)]` in `lib.rs`

- [ ] **Step 1: Add deps to `crates/gonzalo-cli/Cargo.toml`** `[dependencies]`

```toml
gonzalo-ticket        = { workspace = true }
gonzalo-ticket-config = { workspace = true }
```

- [ ] **Step 2: Write the failing test + implementation** — append to `crates/gonzalo-cli/src/lib.rs`

Add these imports at the top (the file already imports several `gonzalo_core` items and `FsStore`):

```rust
use gonzalo_ticket::IngestSummary;
use gonzalo_ticket_config::Config;
```

Add the command function (anywhere among the other `pub async fn`s, e.g. before the tests module):

```rust
// ─── ticket sync ───────────────────────────────────────────────────────────

/// Per-connection ingest result.
pub struct TicketSyncReport {
    pub connection: String,
    pub summary: IngestSummary,
}

/// Load the ticket config, build each connection's source, and ingest its
/// tickets into the fs store at `root`.
pub async fn ticket_sync(
    config_path: &Path,
    root: &Path,
    author: &str,
) -> Result<Vec<TicketSyncReport>> {
    let config = Config::load(config_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let store = FsStore::new(root);
    let mut reports = Vec::new();
    for (name, source) in config.sources().map_err(|e| anyhow::anyhow!("{e}"))? {
        let summary = gonzalo_ticket::ingest(source.as_ref(), &store, author)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        reports.push(TicketSyncReport { connection: name, summary });
    }
    Ok(reports)
}
```

> `Config::load` — add an inherent `load` to the config crate so the call site reads naturally. In `crates/gonzalo-ticket-config/src/lib.rs`, add inside `impl Config`:
> ```rust
> /// Convenience: load and parse from a path.
> pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
>     load(path)
> }
> ```
> (This shadows nothing — the free `load` function remains; the method just delegates.)

Add a test in the existing `#[cfg(test)] mod tests` block in `lib.rs`. It uses the in-tree `InMemorySource` indirectly is not possible from the CLI; instead test the empty-config path (no connections → no reports), which exercises parse + store wiring without network:

```rust
#[tokio::test]
async fn ticket_sync_with_no_connections_returns_no_reports() {
    let root = TempDir::new().unwrap();
    let cfg = TempDir::new().unwrap();
    let cfg_path = cfg.path().join("tickets.toml");
    std::fs::write(&cfg_path, "").unwrap(); // empty config = zero connections

    let reports = ticket_sync(&cfg_path, root.path(), "tester").await.unwrap();
    assert!(reports.is_empty());
}
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p gonzalo-cli ticket_sync_with_no_connections`
Expected: PASS.

- [ ] **Step 4: Wire the subcommands in `crates/gonzalo-cli/src/main.rs`**

Add to the imports:

```rust
use gonzalo_cli::ticket_sync;
```

Add a `Ticket` variant to the `Commands` enum:

```rust
    /// Read external ticket boards into the store, and inspect imported tickets.
    Ticket {
        #[command(subcommand)]
        command: TicketCommands,
    },
```

Add the subcommand enum after the `Commands` enum:

```rust
#[derive(Subcommand)]
enum TicketCommands {
    /// Sync all configured ticket connections into the store.
    Sync {
        /// Path to the tickets TOML config.
        #[arg(long, default_value = "tickets.toml")]
        config: PathBuf,
        /// Root directory of the fs store.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Author recorded on imported records.
        #[arg(long, default_value = "gonzalo-cli")]
        author: String,
    },
    /// List imported ticket record keys.
    List {
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Show one imported ticket record by uid (e.g. "caliban-ai/gonzalo#15").
    Get {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Ticket uid (owner/repo#number).
        uid: String,
    },
}
```

Add the match arm in `main` (alongside the existing arms):

```rust
        Commands::Ticket { command } => match command {
            TicketCommands::Sync { config, root, author } => {
                let reports = ticket_sync(&config, &root, &author).await?;
                if reports.is_empty() {
                    println!("(no connections configured)");
                }
                for r in reports {
                    println!(
                        "{}: imported {} updated {} unchanged {}",
                        r.connection,
                        r.summary.imported,
                        r.summary.updated,
                        r.summary.unchanged
                    );
                }
            }
            TicketCommands::List { root } => {
                let keys = list(&root, Some("tickets".into()), None).await?;
                if keys.is_empty() {
                    println!("(no tickets)");
                } else {
                    for k in keys {
                        println!("{k}");
                    }
                }
            }
            TicketCommands::Get { root, uid } => {
                match get(&root, "tickets", "github", &uid).await? {
                    Some(record) => println!("{}", serde_json::to_string_pretty(&record)?),
                    None => println!("not found"),
                }
            }
        },
```

- [ ] **Step 5: Verify it builds and the CLI shows the new command**

Run: `cargo run -p gonzalo-cli -- ticket --help`
Expected: help text listing `sync`, `list`, `get`.

- [ ] **Step 6: Commit**

```bash
git add crates/gonzalo-cli crates/gonzalo-ticket-config/src/lib.rs
git commit -m "feat(cli): gonzalo ticket sync/list/get"
```

---

## Task 6: Daemon `TicketSync` RPC (gRPC + HTTP)

**Files:**
- Modify: `crates/gonzalo-proto/proto/gonzalo.proto`, `crates/gonzalo-server/src/service.rs`, `crates/gonzalo-server/src/grpc.rs`, `crates/gonzalo-server/src/http.rs`, `crates/gonzalo-server/Cargo.toml`
- Test: inline `#[cfg(test)]` in `service.rs`

- [ ] **Step 1: Add the RPC to `crates/gonzalo-proto/proto/gonzalo.proto`**

Add the rpc inside `service Gonzalo { … }`:

```proto
  rpc TicketSync(TicketSyncRequest) returns (TicketSyncResponse);
```

Add the messages at the end of the file:

```proto
message TicketSyncRequest {
  // JSON of gonzalo_ticket_config::Connection.
  bytes connection_json = 1;
}

message TicketSyncResponse {
  uint64 imported = 1;
  uint64 updated = 2;
  uint64 unchanged = 3;
}
```

- [ ] **Step 2: Add deps to `crates/gonzalo-server/Cargo.toml`** `[dependencies]`

```toml
gonzalo-ticket        = { workspace = true }
gonzalo-ticket-config = { workspace = true }
serde_json    = { workspace = true }
```

(If `serde_json` is already present, leave the existing line.)

- [ ] **Step 3: Write the failing test + implementation** in `crates/gonzalo-server/src/service.rs`

Extend the imports:

```rust
use gonzalo_ticket::IngestSummary;
use gonzalo_ticket_config::Connection;
```

Add the method inside `impl Service`:

```rust
    /// Build a source for `conn` from the registry and ingest its tickets into
    /// the backing store. Errors are flattened to strings at this boundary so
    /// both transports can surface them uniformly.
    pub async fn ticket_sync(
        &self,
        conn: &Connection,
        author: &str,
    ) -> std::result::Result<IngestSummary, String> {
        let source = gonzalo_ticket_config::build_source(conn).map_err(|e| e.to_string())?;
        gonzalo_ticket::ingest(source.as_ref(), self.store.as_ref(), author)
            .await
            .map_err(|e| e.to_string())
    }
```

Add a test module at the end of `service.rs` (it currently has none). It uses an `InMemorySource` is not reachable here, so test the **error path** — an unknown provider returns `Err` without touching the network:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_store_fs::FsStore;
    use gonzalo_ticket_config::Connection;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[tokio::test]
    async fn ticket_sync_rejects_unknown_provider() {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::new(Arc::new(FsStore::new(dir.path())));
        // Token must exist so we reach the provider match.
        unsafe { std::env::set_var("SVC_TEST_TOKEN", "x") };
        let conn = Connection {
            name: "bad".into(),
            provider: "nope".into(),
            org: "caliban-ai".into(),
            project: 1,
            token_env: "SVC_TEST_TOKEN".into(),
            state_map: BTreeMap::new(),
        };
        let err = svc.ticket_sync(&conn, "tester").await.unwrap_err();
        assert!(err.contains("unknown provider"));
        unsafe { std::env::remove_var("SVC_TEST_TOKEN") };
    }
}
```

Add `tempfile` to `crates/gonzalo-server/Cargo.toml` `[dev-dependencies]` if not present:

```toml
[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 4: Run the service test to verify it passes**

Run: `cargo test -p gonzalo-server ticket_sync_rejects_unknown_provider`
Expected: PASS.

- [ ] **Step 5: Wire the gRPC handler in `crates/gonzalo-server/src/grpc.rs`**

Extend the proto import to include the new message types:

```rust
use gonzalo_proto::v1::{
    GetRequest, GetResponse, ListRequest, ListResponse, PutRequest, PutResponse,
    TicketSyncRequest, TicketSyncResponse,
    gonzalo_server::{Gonzalo, GonzaloServer},
};
```

Add the handler inside `impl Gonzalo for GrpcAdapter`:

```rust
    async fn ticket_sync(
        &self,
        req: Request<TicketSyncRequest>,
    ) -> Result<Response<TicketSyncResponse>, Status> {
        let r = req.into_inner();
        let conn: gonzalo_ticket_config::Connection =
            serde_json::from_slice(&r.connection_json).map_err(internal)?;
        let summary = self
            .service
            .ticket_sync(&conn, "gonzalod")
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(TicketSyncResponse {
            imported: summary.imported as u64,
            updated: summary.updated as u64,
            unchanged: summary.unchanged as u64,
        }))
    }
```

- [ ] **Step 6: Wire the HTTP handler in `crates/gonzalo-server/src/http.rs`**

Add a route to the router in `router(...)` (before `.with_state(...)`):

```rust
        .route("/v1/tickets/sync", axum::routing::post(ticket_sync))
```

Add the handler function (near the others):

```rust
async fn ticket_sync(
    State(svc): State<Arc<Service>>,
    Json(conn): Json<gonzalo_ticket_config::Connection>,
) -> Response {
    match svc.ticket_sync(&conn, "gonzalod").await {
        Ok(summary) => (StatusCode::OK, Json(summary)).into_response(),
        Err(e) => server_error(e),
    }
}
```

(`IngestSummary` derives `Serialize`, so `Json(summary)` works.)

- [ ] **Step 7: Verify the whole workspace builds**

Run: `cargo build --workspace --all-targets --all-features`
Expected: clean build. The proto change regenerates the tonic service trait, which now requires the `ticket_sync` method — already added in Step 5.

- [ ] **Step 8: Commit**

```bash
git add crates/gonzalo-proto/proto/gonzalo.proto crates/gonzalo-server
git commit -m "feat(daemon): TicketSync RPC over gRPC + HTTP, reusing the ingest engine"
```

---

## Task 7: Docs, example config, and full verification gate

**Files:**
- Create: `tickets.example.toml`
- Modify: `README.md` (ticket section) — only if a ticket section is absent; otherwise extend it.

- [ ] **Step 1: Create `tickets.example.toml`** at the repo root

```toml
# Example ticket config for the shared caliban-ai Kanban board (GitHub Projects v2 #1).
# Copy to tickets.toml and set KANBAN_PROJECT_PAT in your environment.
[[connection]]
name      = "caliban-ai-board"
provider  = "github-projects"
org       = "caliban-ai"
project   = 1
token_env = "KANBAN_PROJECT_PAT"

[connection.state_map]
default       = "open"
"Todo"        = "open"
"In Progress" = "in_progress"
"Blocked"     = "pending"
"Done"        = "done"
```

- [ ] **Step 2: Add a short usage note to `README.md`**

Add a subsection (place it near the existing CLI usage docs):

```markdown
### Tickets

Sync the shared caliban-ai board into a store and inspect the imported tickets:

```bash
export KANBAN_PROJECT_PAT=ghp_...           # PAT with read:project + repo
cp tickets.example.toml tickets.toml
gonzalo ticket sync --config tickets.toml --root ./store
gonzalo ticket list --root ./store
gonzalo ticket get  --root ./store "caliban-ai/gonzalo#15"
```

The daemon exposes the same operation: `POST /v1/tickets/sync` with a JSON
connection body, or the `TicketSync` gRPC.
```

- [ ] **Step 3: Run the full local verification gate** (mirrors CI; see `~/.claude/CLAUDE.md`)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features
scripts/coverage.sh
```

Expected: all pass. If `fmt --check` fails, run `cargo fmt --all` and re-stage. If coverage dips below the gate, add focused tests (the ingest engine and mapping are the highest-value targets).

- [ ] **Step 4: Commit**

```bash
git add tickets.example.toml README.md
git commit -m "docs(ticket): example board config + ticket usage in README"
```

---

## Self-Review

**Spec coverage:**
- "Org board (Projects v2, GraphQL)" → Tasks 2, 3 (`GitHubProjectSource`, `graphql_body`). ✓
- "Status → State.category via StateMapping/NativeStatus" → Task 2 mapping + Task 4 `state_mapping`. ✓
- "uid = nameWithOwner#number" → Task 2 `item_to_ticket`. ✓
- "Draft items skipped" → Task 2 (`typename != "Issue"` → `None`). ✓
- "Multi-connection array config; token_env" → Task 4 `Config`/`Connection`. ✓
- "Registry above connectors (no cycle)" → Task 4 `build_source`. ✓
- "Ingest engine, idempotent" → Task 1 (content-hash dedup). ✓
- "Surfaces: library + CLI + daemon" → Task 1/4 (lib), Task 5 (CLI), Task 6 (daemon). ✓
- "Mapping total; transport errors fail sync; config errors before network" → Task 2 (default fallback), Task 1 (`?` on source/store), Task 4 (`ConfigError` pre-network). ✓
- "Testing: pure mapping fixture, ingest vs InMemorySource+FsStore, config parsing" → Tasks 2, 1, 4. ✓
- Out of scope (write-back, Prospero) → not implemented; `capabilities()` stays all-false (Task 3). ✓

**Deviation from spec (documented):** pagination is internal to `fetch_changed` (returns empty `next`) rather than cursor-threaded through the ingest loop — Projects v2 lacks a "changed since" filter, and the hash-dedup makes full re-scan cheap. Same end state. Noted in the File Structure section.

**Placeholder scan:** none — every code step has complete code.

**Type consistency:** `IngestSummary { imported, updated, unchanged }` is defined once (Task 1) and consumed identically in CLI (Task 5) and daemon (Task 6). `Connection`/`Config`/`build_source`/`state_mapping` signatures defined in Task 4 match their uses in Tasks 5–6. `item_to_ticket(&GqlItem, &StateMapping) -> Option<Ticket>` defined and used consistently across Tasks 2–3. `GqlResponse`/`GqlItems` field names match between mapping (Task 2) and source (Task 3).
