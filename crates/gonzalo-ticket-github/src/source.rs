//! A read-only [`TicketSource`] backed by the GitHub REST API.
//!
//! Phase 1 imports issues via `fetch_changed` / `get`; write-back is not yet
//! implemented, so [`capabilities`](TicketSource::capabilities) reports all-false
//! and the trait's `Unsupported` defaults apply. HTTP-level behavior is exercised
//! by the conformance suite's recorded fixtures (#20); the pure mapping is unit-
//! tested in [`crate::mapping`].

use crate::mapping::{GhIssue, issue_to_ticket};
use async_trait::async_trait;
use gonzalo_domain::{StateCategory, Ticket};
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

    /// Build an issues URL under the configured repo, e.g. `.../issues`,
    /// `.../issues/15`, or `.../issues/15/comments`. Each `trailing` element is a
    /// distinct, individually-encoded path segment.
    fn issues_url(&self, trailing: &[&str]) -> Result<reqwest::Url> {
        let mut url = self.api_root.clone();
        {
            let mut seg = url
                .path_segments_mut()
                .map_err(|_| SourceError::Backend("api root cannot be a base".into()))?;
            seg.extend(["repos", &self.owner, &self.repo, "issues"]);
            seg.extend(trailing);
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
        Capabilities {
            push: true,
            comments: true,
            ..Capabilities::default()
        }
    }

    async fn fetch_changed(&self, cursor: &Cursor) -> Result<Page> {
        // Follow GitHub's `Link`-header pagination. The first call (empty
        // cursor) builds the issues URL; every later call GETs the `rel="next"`
        // URL that the previous page carried forward in the cursor — page N+1,
        // with `per_page`/`sort`/`direction` echoed by GitHub. When GitHub stops
        // emitting a `rel="next"`, the cursor terminates and the ingest loop stops.
        let url: reqwest::Url = match &cursor.0 {
            Some(next) => reqwest::Url::parse(next).map_err(be)?,
            None => {
                let mut url = self.issues_url(&[])?;
                {
                    let mut q = url.query_pairs_mut();
                    q.append_pair("state", "all");
                    q.append_pair("per_page", "100");
                    q.append_pair("sort", "updated");
                    q.append_pair("direction", "asc");
                }
                url
            }
        };
        let resp = self
            .send(self.client.get(url))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        // Read the next-page indicator before consuming the body.
        let next = resp
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_next_link);
        let issues: Vec<GhIssue> = resp.json().await.map_err(be)?;
        let tickets = issues
            .iter()
            .filter(|i| !i.is_pull_request())
            .map(|i| issue_to_ticket(i, &self.owner_repo))
            .collect();
        // TODO(#19): incremental `since`-based sync (advancing off each page's
        // max `updated_at`) is still future work; this always starts from page 1.
        Ok(Page {
            tickets,
            next: Cursor(next),
        })
    }

    async fn get(&self, uid: &str) -> Result<Ticket> {
        let url = self.issues_url(&[&issue_number(uid)?.to_string()])?;
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

    async fn set_state(&self, uid: &str, target: StateCategory) -> Result<()> {
        // GitHub state is binary + reason: closed/completed, closed/not_planned,
        // or open. Categories collapse onto those.
        let (state, reason) = match target {
            StateCategory::Done => ("closed", Some("completed")),
            StateCategory::Canceled => ("closed", Some("not_planned")),
            _ => ("open", None),
        };
        let mut body = serde_json::json!({ "state": state });
        if let Some(r) = reason {
            body["state_reason"] = serde_json::json!(r);
        }
        let url = self.issues_url(&[&issue_number(uid)?.to_string()])?;
        self.send(self.client.patch(url).json(&body))
            .send()
            .await
            .map_err(be)?
            .error_for_status()
            .map_err(be)?;
        Ok(())
    }

    async fn comment(&self, uid: &str, body: &str) -> Result<()> {
        let url = self.issues_url(&[&issue_number(uid)?.to_string(), "comments"])?;
        self.send(
            self.client
                .post(url)
                .json(&serde_json::json!({ "body": body })),
        )
        .send()
        .await
        .map_err(be)?
        .error_for_status()
        .map_err(be)?;
        Ok(())
    }
}

/// Parse the issue number from a `uid` (`owner/repo#N` or a bare `N`).
fn issue_number(uid: &str) -> Result<u64> {
    uid.rsplit_once('#')
        .map(|(_, n)| n)
        .unwrap_or(uid)
        .parse::<u64>()
        .map_err(|_| SourceError::Backend(format!("cannot parse issue number from uid {uid}")))
}

fn be<E: std::fmt::Display>(e: E) -> SourceError {
    SourceError::Backend(e.to_string())
}

/// Parse a GitHub `Link` response header, returning the URL of the `rel="next"`
/// relation if one is present.
///
/// GitHub paginates with a comma-separated list of `<url>; rel="name"` entries,
/// e.g. `<...?page=2>; rel="next", <...?page=9>; rel="last"`. The last page
/// omits `next` entirely, so `None` is the terminating signal: the ingest loop
/// stops when the cursor holds no next URL.
fn parse_next_link(header: &str) -> Option<String> {
    for part in header.split(',') {
        let mut segs = part.split(';').map(str::trim);
        let Some(url) = segs
            .next()
            .and_then(|s| s.strip_prefix('<'))
            .and_then(|s| s.strip_suffix('>'))
        else {
            continue;
        };
        for param in segs {
            if let Some(rel) = param.strip_prefix("rel=")
                && rel
                    .trim_matches('"')
                    .split_whitespace()
                    .any(|r| r == "next")
            {
                return Some(url.to_string());
            }
        }
    }
    None
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
            src.issues_url(&[]).unwrap().as_str(),
            "https://api.github.com/repos/caliban-ai/gonzalo/issues"
        );
        assert_eq!(
            src.issues_url(&["15", "comments"]).unwrap().as_str(),
            "https://api.github.com/repos/caliban-ai/gonzalo/issues/15/comments"
        );
    }

    #[test]
    fn parse_next_link_finds_next_among_multiple_rels() {
        let header = concat!(
            "<https://api.github.com/repositories/1/issues?page=2>; rel=\"next\", ",
            "<https://api.github.com/repositories/1/issues?page=9>; rel=\"last\""
        );
        assert_eq!(
            parse_next_link(header),
            Some("https://api.github.com/repositories/1/issues?page=2".to_string())
        );
    }

    #[test]
    fn parse_next_link_terminates_without_a_next_rel() {
        // The last page emits only prev/first — no `next`, so pagination ends.
        let header = concat!(
            "<https://api.github.com/repositories/1/issues?page=8>; rel=\"prev\", ",
            "<https://api.github.com/repositories/1/issues?page=1>; rel=\"first\""
        );
        assert_eq!(parse_next_link(header), None);
    }
}
