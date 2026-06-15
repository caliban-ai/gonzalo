//! A [`TicketSource`] over the GitHub **GraphQL** API, reading the org Projects
//! v2 board. `get`/`fetch_changed` are the read path: the board has no reliable
//! "changed since" filter, so `fetch_changed` pages the whole board to
//! completion and returns an empty `next` cursor; cheap re-sync is the ingest
//! engine's job (content-hash dedup).
//!
//! Write-back: `capabilities().push` is `true` and `set_state` moves a card by
//! resolving the target category back to a board column (via [`StateMapping`],
//! with per-source overrides from [`GitHubProjectSource::with_write_targets`])
//! and updating the project's Status single-select field on that issue's item.

use crate::project_mapping::{GqlItems, GqlResponse, item_to_ticket};
use async_trait::async_trait;
use gonzalo_domain::{StateCategory, Ticket};
use gonzalo_ticket::{Capabilities, Cursor, Page, Result, SourceError, StateMapping, TicketSource};
use serde::Deserialize;
use std::collections::BTreeMap;

const GRAPHQL_URL: &str = "https://api.github.com/graphql";

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
    /// The project's Status single-select field (id + options), read from the
    /// project definition so it is present even when the card has no status set.
    /// `None` if the project has no such field.
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

/// Reads issues on an org's Projects v2 board, with their Status column mapped
/// to a normalized state category via [`StateMapping`].
pub struct GitHubProjectSource {
    client: reqwest::Client,
    endpoint: String,
    org: String,
    project_number: u32,
    token: String,
    mapping: StateMapping,
    set_targets: BTreeMap<StateCategory, String>,
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
            set_targets: BTreeMap::new(),
        })
    }

    /// Set the category→column overrides used by `set_state` for boards where
    /// two columns share a category (the reverse of `state_map` is ambiguous).
    pub fn with_write_targets(mut self, targets: BTreeMap<StateCategory, String>) -> Self {
        self.set_targets = targets;
        self
    }

    /// POST a GraphQL body and return the parsed JSON, surfacing transport
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
            let items = items_or_error(parsed)?;
            out.extend(
                items
                    .nodes
                    .iter()
                    .filter_map(|n| item_to_ticket(n, &self.mapping)),
            );
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

/// Pull the items page out of a parsed GraphQL response, surfacing any
/// top-level GraphQL `errors` (GitHub returns these with HTTP 200 for bad
/// tokens, unknown orgs, or malformed queries) as a `Backend` error rather
/// than letting a `null` `data` become an opaque deserialize failure.
pub(crate) fn items_or_error(parsed: GqlResponse) -> Result<GqlItems> {
    if !parsed.errors.is_empty() {
        let msg = parsed
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(SourceError::Backend(format!("github graphql: {msg}")));
    }
    let data = parsed
        .data
        .ok_or_else(|| SourceError::Backend("github graphql: response had no data".into()))?;
    Ok(data.organization.project.items)
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

/// Pick the issue's item on the project numbered `project_number`, then resolve
/// `column` to its single-select option id (case-insensitive). Surfaces GraphQL
/// `errors` and missing pieces as `Backend`.
pub(crate) fn resolve_item(
    resp: ItemLookupResponse,
    project_number: u32,
    column: &str,
) -> Result<ItemCoords> {
    if !resp.errors.is_empty() {
        let msg = resp
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
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
            SourceError::Backend(format!(
                "no Status option named {column:?} on project #{project_number}"
            ))
        })?;
    Ok(ItemCoords {
        item_id: item.id,
        project_id: item.project.id,
        field_id: field.id,
        option_id: option.id.clone(),
    })
}

#[async_trait]
impl TicketSource for GitHubProjectSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            push: true,
            ..Capabilities::default()
        }
    }

    async fn fetch_changed(&self, _cursor: &Cursor) -> Result<Page> {
        let tickets = self.fetch_all().await?;
        Ok(Page {
            tickets,
            next: Cursor::default(),
        })
    }

    /// Find a ticket by uid. Note: this scans the **whole board** (every page)
    /// on each call — intended for occasional single lookups on a small board,
    /// not for calling in a loop. Bulk consumers should use `fetch_changed`.
    async fn get(&self, uid: &str) -> Result<Ticket> {
        self.fetch_all()
            .await?
            .into_iter()
            .find(|t| t.uid == uid)
            .ok_or_else(|| SourceError::Backend(format!("ticket {uid} not found on board")))
    }

    async fn set_state(&self, uid: &str, target: StateCategory) -> Result<()> {
        // Resolve the target category back to a board column. An unmapped
        // category is a genuine capability gap (`Unsupported`); an ambiguous one
        // is a fixable misconfiguration, so surface the conflicting columns
        // (`Backend`) to tell the operator what to put in `set_targets`.
        let column = self
            .mapping
            .column_for(target, &self.set_targets)
            .map_err(reverse_error)?;

        // uid (owner/repo#number) → parts. `number` here is the ISSUE number
        // (u64); `self.project_number` is the PROJECT number (u32).
        let (owner, repo, number) = parse_board_uid(uid)?;

        // Look up the issue's item on this project + the column's option id.
        let lookup: ItemLookupResponse = serde_json::from_value(
            self.post(&project_item_query(&owner, &repo, number))
                .await?,
        )
        .map_err(be)?;
        let coords = resolve_item(lookup, self.project_number, &column)?;

        // Mutate; check the mutation response for top-level GraphQL errors.
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
            let msg = if msg.is_empty() {
                format!("{} error(s) with no message", errors.len())
            } else {
                msg
            };
            return Err(SourceError::Backend(format!("github graphql: {msg}")));
        }
        Ok(())
    }
}

fn be<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Backend(e.to_string())
}

/// Map a reverse-mapping failure to a `SourceError`. `Unmapped` is a capability
/// gap (no column expresses the category) → `Unsupported`; `Ambiguous` is a
/// fixable misconfiguration, so carry the conflicting column names → `Backend`.
fn reverse_error(e: gonzalo_ticket::ReverseError) -> SourceError {
    match e {
        gonzalo_ticket::ReverseError::Unmapped(_) => {
            SourceError::Unsupported("set_state: no column maps to target category")
        }
        gonzalo_ticket::ReverseError::Ambiguous(cat, cols) => SourceError::Backend(format!(
            "set_state: category {cat:?} maps to multiple columns {cols:?}; configure set_targets"
        )),
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn constructs_a_source_from_org_and_number() {
        let src = GitHubProjectSource::new("caliban-ai", 1, "tok", mapping()).unwrap();
        assert_eq!(src.org, "caliban-ai");
        assert_eq!(src.project_number, 1);
        assert!(src.set_targets.is_empty());
    }

    #[test]
    fn graphql_errors_surface_as_backend() {
        let body = r#"{"data": null, "errors": [{"message": "Bad credentials"}]}"#;
        let parsed: crate::project_mapping::GqlResponse = serde_json::from_str(body).unwrap();
        let err = items_or_error(parsed).unwrap_err();
        match err {
            gonzalo_ticket::SourceError::Backend(m) => assert!(m.contains("Bad credentials")),
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn missing_data_without_errors_is_backend() {
        let body = r#"{"data": null}"#;
        let parsed: crate::project_mapping::GqlResponse = serde_json::from_str(body).unwrap();
        assert!(items_or_error(parsed).is_err());
    }

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
        assert!(
            b["query"]
                .as_str()
                .unwrap()
                .contains("updateProjectV2ItemFieldValue")
        );
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
        assert!(resolve_item(resp, 99, "Done").is_err());
    }

    #[test]
    fn resolve_item_errors_on_unknown_column() {
        let resp: ItemLookupResponse = serde_json::from_str(ITEM_LOOKUP).unwrap();
        assert!(resolve_item(resp, 1, "Nonexistent").is_err());
    }

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
        let err = src
            .set_state("caliban-ai/gonzalo#1", StateCategory::Done)
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn with_write_targets_stores_overrides() {
        let mut t = BTreeMap::new();
        t.insert(StateCategory::Done, "Shipped".to_string());
        let src = GitHubProjectSource::new("caliban-ai", 1, "tok", mapping())
            .unwrap()
            .with_write_targets(t);
        assert_eq!(
            src.set_targets
                .get(&StateCategory::Done)
                .map(String::as_str),
            Some("Shipped")
        );
    }
}
