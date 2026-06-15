//! A read-only [`TicketSource`] backed by the GitHub REST API.
//!
//! Phase 1 imports issues via `fetch_changed` / `get`; write-back is not yet
//! implemented, so [`capabilities`](TicketSource::capabilities) reports all-false
//! and the trait's `Unsupported` defaults apply. HTTP-level behavior is exercised
//! by the conformance suite's recorded fixtures (#20); the pure mapping is unit-
//! tested in [`crate::mapping`].

use crate::mapping::{GhIssue, issue_to_ticket};
use async_trait::async_trait;
use gonzalo_domain::Ticket;
use gonzalo_ticket::{Capabilities, Cursor, Page, Result, SourceError, TicketSource};

const API_ROOT: &str = "https://api.github.com";
const ACCEPT: &str = "application/vnd.github+json";

/// Imports issues from a single GitHub repository.
pub struct GitHubSource {
    client: reqwest::Client,
    api_root: reqwest::Url,
    owner: String,
    repo: String,
    owner_repo: String,
    token: Option<String>,
}

impl GitHubSource {
    /// Import from `owner/name` anonymously (subject to GitHub's low unauth
    /// rate limit).
    pub fn new(owner_repo: impl Into<String>) -> Result<Self> {
        Self::build(owner_repo.into(), None, API_ROOT)
    }

    /// Import from `owner/name`, authenticating with a personal-access / app
    /// token on every request.
    pub fn with_token(owner_repo: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        Self::build(owner_repo.into(), Some(token.into()), API_ROOT)
    }

    /// Import against a custom API base — GitHub Enterprise (e.g.
    /// `https://ghe.example.com/api/v3`) or a test server.
    pub fn with_base(
        api_root: &str,
        owner_repo: impl Into<String>,
        token: Option<String>,
    ) -> Result<Self> {
        Self::build(owner_repo.into(), token, api_root)
    }

    fn build(owner_repo: String, token: Option<String>, api_root: &str) -> Result<Self> {
        let (owner, repo) = owner_repo.split_once('/').ok_or_else(|| {
            SourceError::Backend(format!("expected owner/repo, got {owner_repo}"))
        })?;
        let client = reqwest::Client::builder()
            .user_agent("gonzalo-ticket-github")
            .build()
            .map_err(be)?;
        let api_root = reqwest::Url::parse(api_root).map_err(be)?;
        Ok(Self {
            client,
            api_root,
            owner: owner.to_string(),
            repo: repo.to_string(),
            owner_repo: owner_repo.clone(),
            token,
        })
    }

    /// Build an issues URL under the configured repo, e.g. `.../issues` or
    /// `.../issues/15`.
    fn issues_url(&self, trailing: Option<&str>) -> Result<reqwest::Url> {
        let mut url = self.api_root.clone();
        {
            let mut seg = url
                .path_segments_mut()
                .map_err(|_| SourceError::Backend("api root cannot be a base".into()))?;
            seg.extend(["repos", &self.owner, &self.repo, "issues"]);
            if let Some(t) = trailing {
                seg.push(t);
            }
        }
        Ok(url)
    }

    fn send(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let rb = rb.header(reqwest::header::ACCEPT, ACCEPT);
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }
}

#[async_trait]
impl TicketSource for GitHubSource {
    fn capabilities(&self) -> Capabilities {
        // Phase 1: read-only. Write-back lands in a later increment.
        Capabilities::default()
    }

    async fn fetch_changed(&self, cursor: &Cursor) -> Result<Page> {
        let mut url = self.issues_url(None)?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("state", "all");
            q.append_pair("per_page", "100");
            q.append_pair("sort", "updated");
            q.append_pair("direction", "asc");
            if let Some(since) = &cursor.0 {
                q.append_pair("since", since);
            }
        }
        let resp = self
            .send(self.client.get(url))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        let issues: Vec<GhIssue> = resp.json().await.map_err(be)?;
        let tickets = issues
            .iter()
            .filter(|i| !i.is_pull_request())
            .map(|i| issue_to_ticket(i, &self.owner_repo))
            .collect();
        // TODO(#19): advance the cursor from the page's max `updated_at` and
        // follow Link-header pagination. Single page for now.
        Ok(Page {
            tickets,
            next: Cursor::default(),
        })
    }

    async fn get(&self, uid: &str) -> Result<Ticket> {
        let number = uid
            .rsplit_once('#')
            .map(|(_, n)| n)
            .unwrap_or(uid)
            .parse::<u64>()
            .map_err(|_| {
                SourceError::Backend(format!("cannot parse issue number from uid {uid}"))
            })?;
        let url = self.issues_url(Some(&number.to_string()))?;
        let resp = self
            .send(self.client.get(url))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        let issue: GhIssue = resp.json().await.map_err(be)?;
        Ok(issue_to_ticket(&issue, &self.owner_repo))
    }
}

fn be<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Backend(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_owner_repo_without_slash() {
        assert!(GitHubSource::new("not-a-repo").is_err());
    }

    #[test]
    fn builds_issue_urls_under_the_repo() {
        let src = GitHubSource::new("caliban-ai/gonzalo").unwrap();
        assert_eq!(
            src.issues_url(None).unwrap().as_str(),
            "https://api.github.com/repos/caliban-ai/gonzalo/issues"
        );
        assert_eq!(
            src.issues_url(Some("15")).unwrap().as_str(),
            "https://api.github.com/repos/caliban-ai/gonzalo/issues/15"
        );
    }
}
