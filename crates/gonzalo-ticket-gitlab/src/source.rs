//! A read-only [`TicketSource`] backed by the GitLab REST v4 API.
//!
//! Authenticates with a personal/project access token (`PRIVATE-TOKEN` header).
//! Phase 1 imports a single project's issues via `fetch_changed` (page-numbered,
//! advanced by the `x-next-page` header) and `get`. An optional
//! [`StateMapping`] with a `ScopedLabel` signal drives the category from
//! `workflow::`-style labels (see [`crate::mapping`]). HTTP-level behavior is
//! exercised by the conformance suite's fixtures (#20).

use crate::mapping::{GlIssue, issue_to_ticket};
use async_trait::async_trait;
use gonzalo_domain::Ticket;
use gonzalo_ticket::{Capabilities, Cursor, Page, Result, SourceError, StateMapping, TicketSource};

const DEFAULT_BASE: &str = "https://gitlab.com";

/// Imports issues from a single GitLab project.
pub struct GitLabSource {
    client: reqwest::Client,
    base: reqwest::Url,
    project: String,
    token: String,
    mapping: Option<StateMapping>,
}

impl GitLabSource {
    /// Import from `project` (full path, e.g. `group/sub/proj`) on gitlab.com.
    pub fn new(project: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        Self::with_base(DEFAULT_BASE, project, token)
    }

    /// As [`GitLabSource::new`] but against a self-managed instance `base`.
    pub fn with_base(
        base: &str,
        project: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("gonzalo-ticket-gitlab")
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

    /// Apply a per-connection [`StateMapping`] (e.g. a `ScopedLabel` policy for
    /// `workflow::` labels).
    #[must_use]
    pub fn with_mapping(mut self, mapping: StateMapping) -> Self {
        self.mapping = Some(mapping);
        self
    }

    /// `.../api/v4/projects/<url-encoded project>/issues[/<trailing>]`.
    fn issues_url(&self, trailing: Option<&str>) -> Result<reqwest::Url> {
        let mut url = self.base.clone();
        {
            let mut seg = url
                .path_segments_mut()
                .map_err(|_| SourceError::Backend("base URL cannot be a base".into()))?;
            // Pushing the full path as one segment URL-encodes the slashes,
            // which is how GitLab addresses a project (`group%2Fproj`).
            seg.extend(["api", "v4", "projects"]);
            seg.push(&self.project);
            seg.push("issues");
            if let Some(t) = trailing {
                seg.push(t);
            }
        }
        Ok(url)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.header("PRIVATE-TOKEN", &self.token)
    }
}

#[async_trait]
impl TicketSource for GitLabSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn fetch_changed(&self, cursor: &Cursor) -> Result<Page> {
        let mut url = self.issues_url(None)?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("scope", "all");
            q.append_pair("per_page", "100");
            q.append_pair("order_by", "updated_at");
            q.append_pair("sort", "asc");
            q.append_pair("page", cursor.0.as_deref().unwrap_or("1"));
        }
        let resp = self
            .auth(self.client.get(url))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        let next_page = resp
            .headers()
            .get("x-next-page")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let issues: Vec<GlIssue> = resp.json().await.map_err(be)?;
        let tickets = issues
            .iter()
            .map(|i| issue_to_ticket(i, &self.project, self.mapping.as_ref()))
            .collect();
        Ok(Page {
            tickets,
            next: Cursor(next_page),
        })
    }

    async fn get(&self, uid: &str) -> Result<Ticket> {
        let iid = uid
            .rsplit_once('#')
            .map(|(_, n)| n)
            .unwrap_or(uid)
            .parse::<u64>()
            .map_err(|_| SourceError::Backend(format!("cannot parse iid from uid {uid}")))?;
        let url = self.issues_url(Some(&iid.to_string()))?;
        let resp = self
            .auth(self.client.get(url))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        let issue: GlIssue = resp.json().await.map_err(be)?;
        Ok(issue_to_ticket(
            &issue,
            &self.project,
            self.mapping.as_ref(),
        ))
    }
}

fn be<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Backend(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encodes_project_path() {
        let src = GitLabSource::new("group/sub/proj", "tok").unwrap();
        assert_eq!(
            src.issues_url(None).unwrap().as_str(),
            "https://gitlab.com/api/v4/projects/group%2Fsub%2Fproj/issues"
        );
        assert_eq!(
            src.issues_url(Some("7")).unwrap().as_str(),
            "https://gitlab.com/api/v4/projects/group%2Fsub%2Fproj/issues/7"
        );
    }

    #[test]
    fn read_only_capabilities() {
        let src = GitLabSource::new("g/p", "t").unwrap();
        assert!(!src.capabilities().push);
    }
}
