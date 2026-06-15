//! A read-only [`TicketSource`] backed by the Asana REST API.
//!
//! Authenticates with a personal access token (bearer). Phase 1 imports a
//! single project's tasks via `fetch_changed` (offset-paginated) and `get`. An
//! optional [`StateMapping`] selects the status signal (completed / section /
//! custom field; see [`crate::mapping`]). `capabilities` reports
//! `single_assignee` and `custom_fields`. HTTP-level behavior is exercised by
//! the conformance suite's fixtures (#20).

use crate::mapping::{AsanaTask, task_to_ticket};
use async_trait::async_trait;
use gonzalo_domain::{StateCategory, Ticket};
use gonzalo_ticket::{Capabilities, Cursor, Page, Result, SourceError, StateMapping, TicketSource};
use serde::Deserialize;

const DEFAULT_BASE: &str = "https://app.asana.com/api/1.0";

/// Fields requested explicitly — Asana returns a minimal object otherwise.
const OPT_FIELDS: &str = "name,notes,html_notes,completed,assignee.name,created_by.name,\
    memberships.project.name,memberships.project.gid,memberships.section.name,\
    memberships.section.gid,custom_fields.gid,custom_fields.enum_value.name,\
    custom_fields.display_value,tags.name,permalink_url";

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    data: Vec<AsanaTask>,
    #[serde(default)]
    next_page: Option<NextPage>,
}

#[derive(Debug, Deserialize)]
struct NextPage {
    #[serde(default)]
    offset: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OneResponse {
    data: AsanaTask,
}

/// Imports tasks from a single Asana project.
pub struct AsanaSource {
    client: reqwest::Client,
    base: reqwest::Url,
    project: String,
    token: String,
    mapping: Option<StateMapping>,
}

impl AsanaSource {
    /// Import tasks from the Asana `project` gid, authenticating with a PAT.
    pub fn new(project: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        Self::with_base(DEFAULT_BASE, project, token)
    }

    /// As [`AsanaSource::new`] but against a custom API base (e.g. a test
    /// server).
    pub fn with_base(
        base: &str,
        project: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("gonzalo-ticket-asana")
            .build()
            .map_err(be)?;
        Ok(Self {
            client,
            base: reqwest::Url::parse(base).map_err(be)?,
            project: project.into(),
            token: token.into(),
            mapping: None,
        })
    }

    /// Apply a per-connection [`StateMapping`] selecting the status signal
    /// (completed / section / custom field).
    #[must_use]
    pub fn with_mapping(mut self, mapping: StateMapping) -> Self {
        self.mapping = Some(mapping);
        self
    }

    fn url(&self, segments: &[&str]) -> Result<reqwest::Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| SourceError::Backend("base URL cannot be a base".into()))?
            .extend(segments);
        Ok(url)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.token)
    }
}

#[async_trait]
impl TicketSource for AsanaSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            single_assignee: true,
            custom_fields: true,
            push: true,
            comments: true,
            ..Capabilities::default()
        }
    }

    async fn fetch_changed(&self, cursor: &Cursor) -> Result<Page> {
        let mut url = self.url(&["tasks"])?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("project", &self.project);
            q.append_pair("opt_fields", OPT_FIELDS);
            q.append_pair("limit", "100");
            if let Some(offset) = &cursor.0 {
                q.append_pair("offset", offset);
            }
        }
        let resp = self
            .auth(self.client.get(url))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        let list: ListResponse = resp.json().await.map_err(be)?;
        let tickets = list
            .data
            .iter()
            .map(|t| task_to_ticket(t, self.mapping.as_ref()))
            .collect();
        Ok(Page {
            tickets,
            next: Cursor(list.next_page.and_then(|p| p.offset)),
        })
    }

    async fn get(&self, uid: &str) -> Result<Ticket> {
        let mut url = self.url(&["tasks", uid])?;
        url.query_pairs_mut().append_pair("opt_fields", OPT_FIELDS);
        let resp = self
            .auth(self.client.get(url))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        let one: OneResponse = resp.json().await.map_err(be)?;
        Ok(task_to_ticket(&one.data, self.mapping.as_ref()))
    }

    async fn set_state(&self, uid: &str, target: StateCategory) -> Result<()> {
        // The portable Asana write is the `completed` flag: terminal categories
        // complete the task, others reopen it. (Section / custom-field moves are
        // a future addition.)
        let completed = matches!(target, StateCategory::Done | StateCategory::Canceled);
        let url = self.url(&["tasks", uid])?;
        self.auth(
            self.client
                .put(url)
                .json(&serde_json::json!({ "data": { "completed": completed } })),
        )
        .send()
        .await
        .map_err(be)?
        .error_for_status()
        .map_err(be)?;
        Ok(())
    }

    async fn comment(&self, uid: &str, body: &str) -> Result<()> {
        let url = self.url(&["tasks", uid, "stories"])?;
        self.auth(
            self.client
                .post(url)
                .json(&serde_json::json!({ "data": { "text": body } })),
        )
        .send()
        .await
        .map_err(be)?
        .error_for_status()
        .map_err(be)?;
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
    fn builds_task_urls() {
        let src = AsanaSource::new("1201", "tok").unwrap();
        assert_eq!(
            src.url(&["tasks"]).unwrap().as_str(),
            "https://app.asana.com/api/1.0/tasks"
        );
        assert_eq!(
            src.url(&["tasks", "1201"]).unwrap().as_str(),
            "https://app.asana.com/api/1.0/tasks/1201"
        );
    }

    #[test]
    fn capabilities_reflect_asana_shape() {
        let caps = AsanaSource::new("1201", "tok").unwrap().capabilities();
        assert!(caps.single_assignee);
        assert!(caps.custom_fields);
        assert!(caps.push);
        assert!(caps.comments);
    }
}
