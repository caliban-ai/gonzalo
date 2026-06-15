//! A read-only [`TicketSource`] over the GitHub **GraphQL** API, reading the
//! org Projects v2 board. Phase 1: `capabilities()` is all-false. The board has
//! no reliable "changed since" filter, so `fetch_changed` pages the whole board
//! to completion and returns an empty `next` cursor; cheap re-sync is the
//! ingest engine's job (content-hash dedup).

use crate::project_mapping::{GqlItems, GqlResponse, item_to_ticket};
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

#[async_trait]
impl TicketSource for GitHubProjectSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default() // read-only in phase 1
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
}
