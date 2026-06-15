//! Pure mapping from GitLab REST v4 issue JSON to the canonical [`Ticket`].
//!
//! GitLab free encodes workflow in `workflow::`-style **scoped labels** rather
//! than a status field, so this connector demonstrates the
//! [`StateSignal::ScopedLabel`] path (ADR 0010): when a `StateMapping` with that
//! signal is configured, the category comes from the matching scoped label;
//! otherwise it falls back to the intrinsic `opened`/`closed` state. (Premium's
//! native status field is a future addition.)

use gonzalo_domain::{
    Actor, ActorRole, BodyFormat, Container, Provider, State, StateCategory, Ticket, TicketBody,
};
use gonzalo_ticket::{StateMapping, StateSignal};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub(crate) struct GlIssue {
    pub iid: u64,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub state: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<GlUser>,
    #[serde(default)]
    pub author: Option<GlUser>,
    #[serde(default)]
    pub issue_type: Option<String>,
    #[serde(default)]
    pub references: Option<GlReferences>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GlUser {
    pub username: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GlReferences {
    #[serde(default)]
    pub full: Option<String>,
}

/// Resolve the normalized category (and the raw status string retained on
/// [`State`]). A `ScopedLabel` mapping wins when a matching label is present;
/// otherwise GitLab's intrinsic `opened`/`closed` decides.
fn resolve_state(
    issue_state: &str,
    labels: &[String],
    mapping: Option<&StateMapping>,
) -> (StateCategory, String) {
    if let Some(m) = mapping
        && let StateSignal::ScopedLabel { prefix } = &m.signal
        && let Some(label) = labels.iter().find(|l| l.starts_with(prefix.as_str()))
    {
        let suffix = &label[prefix.len()..];
        return (m.category_of(suffix), label.clone());
    }
    let category = match issue_state {
        "closed" => StateCategory::Done,
        _ => StateCategory::Open,
    };
    (category, issue_state.to_string())
}

fn actor(user: &GlUser, role: ActorRole) -> Actor {
    Actor {
        role,
        handle: user.username.clone(),
        display: None,
    }
}

/// Map a GitLab issue to a canonical [`Ticket`]. `project` is the full path
/// (e.g. `group/sub/proj`), used for the container and uid fallback.
pub(crate) fn issue_to_ticket(
    issue: &GlIssue,
    project: &str,
    mapping: Option<&StateMapping>,
) -> Ticket {
    let (category, raw_name) = resolve_state(&issue.state, &issue.labels, mapping);

    let uid = issue
        .references
        .as_ref()
        .and_then(|r| r.full.clone())
        .unwrap_or_else(|| format!("{project}#{}", issue.iid));

    let mut actors: Vec<Actor> = issue
        .assignees
        .iter()
        .map(|a| actor(a, ActorRole::Assignee))
        .collect();
    if let Some(author) = &issue.author {
        actors.push(actor(author, ActorRole::Submitter));
    }

    Ticket {
        provider: Provider::GitLab,
        uid,
        display: format!("#{}", issue.iid),
        item_type: issue.issue_type.clone().unwrap_or_else(|| "issue".into()),
        title: issue.title.clone(),
        state: State {
            category,
            resolution: None,
            raw_name,
            raw_id: None,
        },
        priority: None,
        actors,
        labels: issue.labels.clone(),
        containers: vec![Container {
            kind: "project".into(),
            id: project.to_string(),
            name: None,
            primary: true,
        }],
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

    fn issue(state: &str, labels: &[&str]) -> GlIssue {
        GlIssue {
            iid: 7,
            title: "x".into(),
            description: Some("body".into()),
            state: state.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            assignees: vec![GlUser {
                username: "dev".into(),
            }],
            author: Some(GlUser {
                username: "reporter".into(),
            }),
            issue_type: Some("issue".into()),
            references: Some(GlReferences {
                full: Some("group/proj#7".into()),
            }),
        }
    }

    fn workflow_mapping() -> StateMapping {
        let mut by_value = BTreeMap::new();
        by_value.insert("in review".to_string(), StateCategory::InProgress);
        by_value.insert("blocked".to_string(), StateCategory::Pending);
        StateMapping {
            signal: StateSignal::ScopedLabel {
                prefix: "workflow::".into(),
            },
            by_value,
            default: StateCategory::Open,
        }
    }

    #[test]
    fn intrinsic_state_without_mapping() {
        let t = issue_to_ticket(&issue("opened", &[]), "group/proj", None);
        assert_eq!(t.state.category, StateCategory::Open);
        assert_eq!(t.uid, "group/proj#7");
        assert_eq!(t.display, "#7");
        assert_eq!(t.actors[0].handle, "dev");

        let closed = issue_to_ticket(&issue("closed", &[]), "group/proj", None);
        assert_eq!(closed.state.category, StateCategory::Done);
    }

    #[test]
    fn scoped_label_drives_category_when_mapped() {
        let m = workflow_mapping();
        let t = issue_to_ticket(
            &issue("opened", &["workflow::in review", "type::feature"]),
            "group/proj",
            Some(&m),
        );
        assert_eq!(t.state.category, StateCategory::InProgress);
        assert_eq!(t.state.raw_name, "workflow::in review");
    }

    #[test]
    fn scoped_label_falls_back_to_default_when_unmapped() {
        let m = workflow_mapping();
        // a workflow label whose suffix isn't in the mapping -> default (Open)
        let t = issue_to_ticket(
            &issue("opened", &["workflow::bespoke"]),
            "group/proj",
            Some(&m),
        );
        assert_eq!(t.state.category, StateCategory::Open);
    }

    #[test]
    fn intrinsic_used_when_no_scoped_label_present() {
        let m = workflow_mapping();
        let t = issue_to_ticket(&issue("closed", &["type::bug"]), "group/proj", Some(&m));
        assert_eq!(t.state.category, StateCategory::Done);
    }
}
