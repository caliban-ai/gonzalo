//! A read-only [`TicketSource`] backed by the Jira Cloud REST v3 API.
//!
//! Authenticates with Atlassian basic auth (`email` + API token). Phase 1
//! imports via `fetch_changed` (enhanced `/search/jql`, token-paginated) and
//! `get`; `capabilities` reports `transitions_required` so a future write-back
//! knows Jira state changes go through transitions, not field sets. HTTP-level
//! behavior is exercised by the conformance suite's fixtures (#20); the mapping
//! is unit-tested in [`crate::mapping`].

use crate::mapping::{JiraIssue, issue_to_ticket};
use async_trait::async_trait;
use gonzalo_domain::Ticket;
use gonzalo_ticket::{Capabilities, Cursor, Page, Result, SourceError, StateMapping, TicketSource};
use serde::Deserialize;

const FIELDS: &[&str] = &[
    "summary",
    "description",
    "status",
    "issuetype",
    "priority",
    "assignee",
    "reporter",
    "labels",
    "project",
    "resolution",
];

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    issues: Vec<JiraIssue>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

/// Imports issues from a Jira Cloud site.
pub struct JiraSource {
    client: reqwest::Client,
    base: reqwest::Url,
    email: String,
    token: String,
    mapping: Option<StateMapping>,
    jql: String,
}

impl JiraSource {
    /// Connect to `site` (e.g. `https://acme.atlassian.net`) with an Atlassian
    /// account email and API token.
    pub fn new(site: &str, email: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let base = reqwest::Url::parse(site).map_err(be)?;
        let client = reqwest::Client::builder()
            .user_agent("gonzalo-ticket-jira")
            .build()
            .map_err(be)?;
        Ok(Self {
            client,
            base,
            email: email.into(),
            token: token.into(),
            mapping: None,
            jql: "order by updated asc".to_string(),
        })
    }

    /// Apply a per-connection [`StateMapping`] for status-name → category
    /// overrides (falls back to Jira's `statusCategory`).
    #[must_use]
    pub fn with_mapping(mut self, mapping: StateMapping) -> Self {
        self.mapping = Some(mapping);
        self
    }

    /// Restrict the import to issues matching `jql` (default: all, ordered by
    /// `updated` ascending).
    #[must_use]
    pub fn with_jql(mut self, jql: impl Into<String>) -> Self {
        self.jql = jql.into();
        self
    }

    fn url(&self, segments: &[&str]) -> Result<reqwest::Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| SourceError::Backend("site URL cannot be a base".into()))?
            .extend(segments);
        Ok(url)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.basic_auth(&self.email, Some(&self.token))
    }
}

#[async_trait]
impl TicketSource for JiraSource {
    fn capabilities(&self) -> Capabilities {
        // Phase 1: read-only. `transitions_required` records that a future
        // write-back must move state via transitions, not a field set.
        Capabilities {
            transitions_required: true,
            ..Capabilities::default()
        }
    }

    async fn fetch_changed(&self, cursor: &Cursor) -> Result<Page> {
        let url = self.url(&["rest", "api", "3", "search", "jql"])?;
        let mut body = serde_json::json!({
            "jql": self.jql,
            "fields": FIELDS,
            "maxResults": 100,
        });
        if let Some(token) = &cursor.0 {
            body["nextPageToken"] = serde_json::json!(token);
        }
        let resp = self
            .auth(self.client.post(url).json(&body))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        let search: SearchResponse = resp.json().await.map_err(be)?;
        let tickets = search
            .issues
            .iter()
            .map(|i| issue_to_ticket(i, self.mapping.as_ref()))
            .collect();
        Ok(Page {
            tickets,
            next: Cursor(search.next_page_token),
        })
    }

    async fn get(&self, uid: &str) -> Result<Ticket> {
        let mut url = self.url(&["rest", "api", "3", "issue", uid])?;
        url.query_pairs_mut()
            .append_pair("fields", &FIELDS.join(","));
        let resp = self
            .auth(self.client.get(url))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        let issue: JiraIssue = resp.json().await.map_err(be)?;
        Ok(issue_to_ticket(&issue, self.mapping.as_ref()))
    }
}

fn be<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Backend(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_search_and_issue_urls() {
        let src = JiraSource::new("https://acme.atlassian.net", "me@acme.co", "tok").unwrap();
        assert_eq!(
            src.url(&["rest", "api", "3", "search", "jql"])
                .unwrap()
                .as_str(),
            "https://acme.atlassian.net/rest/api/3/search/jql"
        );
        assert_eq!(
            src.url(&["rest", "api", "3", "issue", "ENG-42"])
                .unwrap()
                .as_str(),
            "https://acme.atlassian.net/rest/api/3/issue/ENG-42"
        );
    }

    #[test]
    fn capabilities_flag_transition_gating() {
        let src = JiraSource::new("https://acme.atlassian.net", "me@acme.co", "tok").unwrap();
        let caps = src.capabilities();
        assert!(caps.transitions_required);
        assert!(!caps.push);
    }

    #[test]
    fn rejects_bad_site_url() {
        assert!(JiraSource::new("not a url", "e", "t").is_err());
    }
}
