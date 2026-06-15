//! A read-only [`TicketSource`] backed by the Linear GraphQL API.
//!
//! Authenticates with a Linear API key in the `Authorization` header. Phase 1
//! imports via `fetch_changed` (cursor-paginated `issues` query) and `get`.
//! HTTP-level behavior is exercised by the conformance suite's fixtures (#20);
//! the mapping is unit-tested in [`crate::mapping`].

use crate::mapping::{LinearIssue, issue_to_ticket};
use async_trait::async_trait;
use gonzalo_domain::{StateCategory, Ticket};
use gonzalo_ticket::{Capabilities, Cursor, Page, Result, SourceError, StateMapping, TicketSource};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// The Linear workflow-state `type` a target [`StateCategory`] maps onto.
fn target_state_type(target: StateCategory) -> &'static str {
    match target {
        StateCategory::Triage => "triage",
        StateCategory::Backlog => "backlog",
        StateCategory::InProgress | StateCategory::Pending => "started",
        StateCategory::Done => "completed",
        StateCategory::Canceled => "canceled",
        // Open
        _ => "unstarted",
    }
}

#[derive(Debug, Deserialize)]
struct IssueTeamData {
    issue: Option<IssueTeam>,
}

#[derive(Debug, Deserialize)]
struct IssueTeam {
    team: Team,
}

#[derive(Debug, Deserialize)]
struct Team {
    states: StateNodes,
}

#[derive(Debug, Deserialize)]
struct StateNodes {
    nodes: Vec<WorkflowState>,
}

#[derive(Debug, Deserialize)]
struct WorkflowState {
    id: String,
    #[serde(rename = "type")]
    type_: String,
}

const ENDPOINT: &str = "https://api.linear.app/graphql";

/// The issue fields selected by both queries.
const ISSUE_FIELDS: &str = "id identifier title description priority \
    state { name type } assignee { displayName } creator { displayName } \
    labels { nodes { name } } team { key name } project { name }";

#[derive(Debug, Deserialize)]
struct GqlResponse<T> {
    #[serde(default = "Option::default")]
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Debug, Deserialize)]
struct GqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct IssuesData {
    issues: IssueConnection,
}

#[derive(Debug, Deserialize)]
struct IssueConnection {
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
    nodes: Vec<LinearIssue>,
}

#[derive(Debug, Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IssueData {
    issue: Option<LinearIssue>,
}

/// Imports issues from a Linear workspace.
pub struct LinearSource {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    api_key: String,
    mapping: Option<StateMapping>,
}

impl LinearSource {
    /// Connect with a Linear API key (sent verbatim in the `Authorization`
    /// header, per Linear's personal-API-key scheme).
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_endpoint(ENDPOINT, api_key)
    }

    /// Connect against a custom GraphQL endpoint (e.g. a test server).
    pub fn with_endpoint(endpoint: &str, api_key: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("gonzalo-ticket-linear")
            .build()
            .map_err(be)?;
        Ok(Self {
            client,
            endpoint: reqwest::Url::parse(endpoint).map_err(be)?,
            api_key: api_key.into(),
            mapping: None,
        })
    }

    /// Apply a per-connection [`StateMapping`] for state-name → category
    /// overrides (falls back to Linear's state `type`).
    #[must_use]
    pub fn with_mapping(mut self, mapping: StateMapping) -> Self {
        self.mapping = Some(mapping);
        self
    }

    async fn query<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        let resp = self
            .client
            .post(self.endpoint.clone())
            .header(reqwest::header::AUTHORIZATION, &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        let gql: GqlResponse<T> = resp.json().await.map_err(be)?;
        if !gql.errors.is_empty() {
            let msg = gql
                .errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(SourceError::Backend(format!("linear graphql: {msg}")));
        }
        gql.data
            .ok_or_else(|| SourceError::Backend("linear graphql: empty data".into()))
    }
}

#[async_trait]
impl TicketSource for LinearSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            push: true,
            comments: true,
            ..Capabilities::default()
        }
    }

    async fn fetch_changed(&self, cursor: &Cursor) -> Result<Page> {
        let query = format!(
            "query($after: String) {{ issues(first: 100, after: $after) {{ \
             pageInfo {{ hasNextPage endCursor }} nodes {{ {ISSUE_FIELDS} }} }} }}"
        );
        let data: IssuesData = self
            .query(&query, serde_json::json!({ "after": cursor.0 }))
            .await?;
        let tickets = data
            .issues
            .nodes
            .iter()
            .map(|i| issue_to_ticket(i, self.mapping.as_ref()))
            .collect();
        let next = if data.issues.page_info.has_next_page {
            Cursor(data.issues.page_info.end_cursor)
        } else {
            Cursor::default()
        };
        Ok(Page { tickets, next })
    }

    async fn get(&self, uid: &str) -> Result<Ticket> {
        let query = format!("query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} }} }}");
        let data: IssueData = self.query(&query, serde_json::json!({ "id": uid })).await?;
        let issue = data
            .issue
            .ok_or_else(|| SourceError::Backend(format!("no linear issue {uid}")))?;
        Ok(issue_to_ticket(&issue, self.mapping.as_ref()))
    }

    async fn set_state(&self, uid: &str, target: StateCategory) -> Result<()> {
        // Linear states are workspace-defined; resolve a state of the issue's
        // team whose `type` matches the target, then update the issue to it.
        let states_query =
            "query($id: String!) { issue(id: $id) { team { states { nodes { id type } } } } }";
        let data: IssueTeamData = self
            .query(states_query, serde_json::json!({ "id": uid }))
            .await?;
        let team = data
            .issue
            .ok_or_else(|| SourceError::Backend(format!("no linear issue {uid}")))?
            .team;
        let want = target_state_type(target);
        let state = team
            .states
            .nodes
            .iter()
            .find(|s| s.type_ == want)
            .ok_or_else(|| {
                SourceError::Backend(format!("team has no workflow state of type '{want}'"))
            })?;

        let mutation = "mutation($id: String!, $sid: String!) { issueUpdate(id: $id, input: { stateId: $sid }) { success } }";
        let _: serde_json::Value = self
            .query(mutation, serde_json::json!({ "id": uid, "sid": state.id }))
            .await?;
        Ok(())
    }

    async fn comment(&self, uid: &str, body: &str) -> Result<()> {
        let mutation = "mutation($id: String!, $body: String!) { commentCreate(input: { issueId: $id, body: $body }) { success } }";
        let _: serde_json::Value = self
            .query(mutation, serde_json::json!({ "id": uid, "body": body }))
            .await?;
        Ok(())
    }
}

fn be<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Backend(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_and_write_capabilities() {
        let src = LinearSource::new("lin_api_xxx").unwrap();
        assert!(src.capabilities().push);
        assert!(src.capabilities().comments);
        assert_eq!(src.endpoint.as_str(), "https://api.linear.app/graphql");
    }

    #[test]
    fn parses_graphql_errors_envelope() {
        // Round-trips the error envelope shape the connector surfaces.
        let resp: GqlResponse<IssuesData> =
            serde_json::from_str(r#"{"errors":[{"message":"bad"}]}"#).unwrap();
        assert!(resp.data.is_none());
        assert_eq!(resp.errors[0].message, "bad");
    }
}
