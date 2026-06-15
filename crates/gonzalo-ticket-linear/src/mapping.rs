//! Pure mapping from Linear GraphQL issue nodes to the canonical [`Ticket`].
//!
//! Linear's workflow states carry a normalized `type`
//! (`triage`/`backlog`/`unstarted`/`started`/`completed`/`canceled`), which is
//! the cross-platform spine; the connector maps that, honoring a per-connection
//! [`StateMapping`] override keyed by the raw state name (ADR 0010). Bodies are
//! already markdown.

use gonzalo_domain::{
    Actor, ActorRole, BodyFormat, Container, Priority, PriorityLevel, Provider, State,
    StateCategory, Ticket, TicketBody,
};
use gonzalo_ticket::StateMapping;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub(crate) struct LinearIssue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: u8,
    pub state: LinearState,
    #[serde(default)]
    pub assignee: Option<LinearUser>,
    #[serde(default)]
    pub creator: Option<LinearUser>,
    #[serde(default)]
    pub labels: LinearLabels,
    #[serde(default)]
    pub team: Option<LinearTeam>,
    #[serde(default)]
    pub project: Option<LinearProject>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LinearState {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct LinearLabels {
    #[serde(default)]
    pub nodes: Vec<LinearNamed>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LinearNamed {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LinearUser {
    #[serde(rename = "displayName")]
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LinearTeam {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LinearProject {
    pub name: String,
}

/// Linear state `type` → canonical category. These six are fixed by Linear.
fn category_from_state_type(type_: &str) -> StateCategory {
    match type_ {
        "triage" => StateCategory::Triage,
        "backlog" => StateCategory::Backlog,
        "started" => StateCategory::InProgress,
        "completed" => StateCategory::Done,
        "canceled" => StateCategory::Canceled,
        // "unstarted" and anything unrecognized
        _ => StateCategory::Open,
    }
}

/// Linear priority is `0..=4` (0 = none, 1 = urgent … 4 = low).
fn priority_level(p: u8) -> PriorityLevel {
    match p {
        1 => PriorityLevel::Urgent,
        2 => PriorityLevel::High,
        3 => PriorityLevel::Medium,
        4 => PriorityLevel::Low,
        _ => PriorityLevel::None,
    }
}

fn actor(user: &LinearUser, role: ActorRole) -> Actor {
    Actor {
        role,
        handle: user.display_name.clone(),
        display: Some(user.display_name.clone()),
    }
}

/// Map a Linear issue to a canonical [`Ticket`]. `mapping`, if given, overrides
/// the category for state names it lists; otherwise the state `type` decides.
pub(crate) fn issue_to_ticket(issue: &LinearIssue, mapping: Option<&StateMapping>) -> Ticket {
    let category = mapping
        .and_then(|m| m.by_value.get(&issue.state.name).copied())
        .unwrap_or_else(|| category_from_state_type(&issue.state.type_));

    let mut actors = Vec::new();
    if let Some(a) = &issue.assignee {
        actors.push(actor(a, ActorRole::Assignee));
    }
    if let Some(c) = &issue.creator {
        actors.push(actor(c, ActorRole::Submitter));
    }

    let mut containers = Vec::new();
    if let Some(t) = &issue.team {
        containers.push(Container {
            kind: "team".into(),
            id: t.key.clone(),
            name: Some(t.name.clone()),
            primary: true,
        });
    }
    if let Some(p) = &issue.project {
        containers.push(Container {
            kind: "project".into(),
            id: p.name.clone(),
            name: Some(p.name.clone()),
            primary: false,
        });
    }

    Ticket {
        provider: Provider::Linear,
        uid: issue.id.clone(),
        display: issue.identifier.clone(),
        item_type: "issue".into(),
        title: issue.title.clone(),
        state: State {
            category,
            resolution: None,
            raw_name: issue.state.name.clone(),
            raw_id: None,
        },
        priority: Some(Priority {
            level: priority_level(issue.priority),
            raw: Some(issue.priority.to_string()),
        }),
        actors,
        labels: issue.labels.nodes.iter().map(|l| l.name.clone()).collect(),
        containers,
        links: vec![],
        body: TicketBody {
            markdown: issue.description.clone().unwrap_or_default(),
            format: BodyFormat::Markdown,
            raw: None,
        },
        fields: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTED: &str = r#"{
        "id": "uuid-123",
        "identifier": "ENG-7",
        "title": "Linear connector",
        "description": "do the thing",
        "priority": 2,
        "state": {"name": "In Progress", "type": "started"},
        "assignee": {"displayName": "John Ford"},
        "creator": {"displayName": "Reporter"},
        "labels": {"nodes": [{"name": "backend"}]},
        "team": {"key": "ENG", "name": "Engineering"},
        "project": {"name": "Tickets"}
    }"#;

    #[test]
    fn maps_started_issue() {
        let issue: LinearIssue = serde_json::from_str(STARTED).unwrap();
        let t = issue_to_ticket(&issue, None);
        assert_eq!(t.provider, Provider::Linear);
        assert_eq!(t.uid, "uuid-123");
        assert_eq!(t.display, "ENG-7");
        assert_eq!(t.state.category, StateCategory::InProgress);
        assert_eq!(t.priority.unwrap().level, PriorityLevel::High);
        assert_eq!(t.body.markdown, "do the thing");
        assert_eq!(t.labels, vec!["backend"]);
        assert_eq!(t.containers[0].kind, "team");
        assert!(t.containers[0].primary);
        assert_eq!(t.containers[1].kind, "project");
        assert_eq!(t.actors[0].handle, "John Ford");
    }

    #[test]
    fn state_types_map_to_categories() {
        assert_eq!(category_from_state_type("triage"), StateCategory::Triage);
        assert_eq!(category_from_state_type("backlog"), StateCategory::Backlog);
        assert_eq!(category_from_state_type("unstarted"), StateCategory::Open);
        assert_eq!(
            category_from_state_type("canceled"),
            StateCategory::Canceled
        );
    }

    #[test]
    fn priority_zero_is_none() {
        assert_eq!(priority_level(0), PriorityLevel::None);
        assert_eq!(priority_level(1), PriorityLevel::Urgent);
    }

    #[test]
    fn mapping_overrides_state_type() {
        let issue: LinearIssue = serde_json::from_str(STARTED).unwrap();
        let mut by_value = BTreeMap::new();
        by_value.insert("In Progress".to_string(), StateCategory::Pending);
        let mapping = StateMapping {
            signal: gonzalo_ticket::StateSignal::NativeStatus,
            by_value,
            default: StateCategory::Open,
        };
        assert_eq!(
            issue_to_ticket(&issue, Some(&mapping)).state.category,
            StateCategory::Pending
        );
    }
}
