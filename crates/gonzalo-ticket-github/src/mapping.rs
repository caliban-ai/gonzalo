//! Pure mapping from GitHub REST issue JSON to the canonical [`Ticket`].
//!
//! Kept free of I/O so it is fully testable from recorded fixtures. GitHub's
//! status is *intrinsic* (open/closed + `state_reason`) and not per-connection
//! configurable, so — unlike GitLab/Asana — the connector bakes the state model
//! in rather than taking a `StateMapping` (ADR 0010).

use gonzalo_domain::{
    Actor, ActorRole, BodyFormat, Container, Provider, Resolution, State, StateCategory, Ticket,
    TicketBody,
};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub(crate) struct GhUser {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GhLabel {
    pub name: String,
}

/// The subset of GitHub's REST issue object this connector reads.
#[derive(Debug, Deserialize)]
pub(crate) struct GhIssue {
    pub number: u64,
    pub node_id: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: String,
    #[serde(default)]
    pub state_reason: Option<String>,
    #[serde(default)]
    pub labels: Vec<GhLabel>,
    #[serde(default)]
    pub assignees: Vec<GhUser>,
    #[serde(default)]
    pub user: Option<GhUser>,
    pub html_url: String,
    /// Present iff this object is actually a pull request — the issues endpoint
    /// returns PRs too, and we filter them out.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

impl GhIssue {
    pub(crate) fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

/// Normalize GitHub's `(state, state_reason)` pair to a category + resolution.
fn normalize_state(state: &str, reason: Option<&str>) -> (StateCategory, Option<Resolution>) {
    match (state, reason) {
        ("open", _) => (StateCategory::Open, None),
        ("closed", Some("not_planned")) => (StateCategory::Canceled, Some(Resolution::WontDo)),
        ("closed", _) => (StateCategory::Done, Some(Resolution::Done)),
        _ => (StateCategory::Open, None),
    }
}

/// Map a GitHub issue to a canonical [`Ticket`]. `owner_repo` is `"owner/name"`.
pub(crate) fn issue_to_ticket(issue: &GhIssue, owner_repo: &str) -> Ticket {
    let (category, resolution) = normalize_state(&issue.state, issue.state_reason.as_deref());

    let mut actors: Vec<Actor> = issue
        .assignees
        .iter()
        .map(|a| Actor {
            role: ActorRole::Assignee,
            handle: a.login.clone(),
            display: None,
        })
        .collect();
    if let Some(u) = &issue.user {
        actors.push(Actor {
            role: ActorRole::Submitter,
            handle: u.login.clone(),
            display: None,
        });
    }

    let mut fields = BTreeMap::new();
    fields.insert("html_url".into(), serde_json::json!(issue.html_url));

    Ticket {
        provider: Provider::GitHub,
        uid: format!("{owner_repo}#{}", issue.number),
        display: format!("#{}", issue.number),
        item_type: "issue".into(),
        title: issue.title.clone(),
        state: State {
            category,
            resolution,
            raw_name: issue.state.clone(),
            raw_id: Some(issue.node_id.clone()),
        },
        priority: None,
        actors,
        labels: issue.labels.iter().map(|l| l.name.clone()).collect(),
        containers: vec![Container {
            kind: "repo".into(),
            id: owner_repo.to_string(),
            name: None,
            primary: true,
        }],
        links: vec![],
        body: TicketBody {
            markdown: issue.body.clone().unwrap_or_default(),
            format: BodyFormat::Markdown,
            raw: None,
        },
        fields,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLOSED_COMPLETED: &str = r#"{
        "number": 15,
        "node_id": "I_kwDO_abc",
        "title": "design: ticket-system capability layer",
        "body": "Model tickets as a capability layer.",
        "state": "closed",
        "state_reason": "completed",
        "labels": [{"name": "area/integration"}, {"name": "kind/design"}],
        "assignees": [{"login": "johnford2002"}],
        "user": {"login": "reporter1"},
        "html_url": "https://github.com/caliban-ai/gonzalo/issues/15"
    }"#;

    #[test]
    fn maps_closed_completed_issue() {
        let issue: GhIssue = serde_json::from_str(CLOSED_COMPLETED).unwrap();
        let t = issue_to_ticket(&issue, "caliban-ai/gonzalo");

        assert_eq!(t.uid, "caliban-ai/gonzalo#15");
        assert_eq!(t.display, "#15");
        assert_eq!(t.provider, Provider::GitHub);
        assert_eq!(t.state.category, StateCategory::Done);
        assert_eq!(t.state.resolution, Some(Resolution::Done));
        assert_eq!(t.state.raw_name, "closed");
        assert_eq!(t.labels, vec!["area/integration", "kind/design"]);
        // assignee first, then submitter
        assert_eq!(t.actors[0].role, ActorRole::Assignee);
        assert_eq!(t.actors[0].handle, "johnford2002");
        assert!(
            t.actors
                .iter()
                .any(|a| a.role == ActorRole::Submitter && a.handle == "reporter1")
        );
        assert_eq!(t.containers[0].id, "caliban-ai/gonzalo");
        assert!(t.containers[0].primary);
    }

    #[test]
    fn closed_not_planned_maps_to_canceled_wontdo() {
        let (cat, res) = normalize_state("closed", Some("not_planned"));
        assert_eq!(cat, StateCategory::Canceled);
        assert_eq!(res, Some(Resolution::WontDo));
    }

    #[test]
    fn open_issue_has_no_resolution() {
        let (cat, res) = normalize_state("open", None);
        assert_eq!(cat, StateCategory::Open);
        assert_eq!(res, None);
    }

    #[test]
    fn detects_pull_requests_for_filtering() {
        let mut issue: GhIssue = serde_json::from_str(CLOSED_COMPLETED).unwrap();
        assert!(!issue.is_pull_request());
        issue.pull_request = Some(serde_json::json!({"url": "..."}));
        assert!(issue.is_pull_request());
    }
}
