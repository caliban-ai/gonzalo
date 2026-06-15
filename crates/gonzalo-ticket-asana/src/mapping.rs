//! Pure mapping from Asana task JSON to the canonical [`Ticket`].
//!
//! Asana has **no intrinsic state field** — status lives in a `completed` bool,
//! a board section, or a custom enum field, by workspace convention. This
//! connector therefore exercises the remaining state signals (ADR 0010):
//! [`StateSignal::Completed`], [`StateSignal::Section`], and
//! [`StateSignal::CustomField`]. Tasks are multi-homed across projects, single-
//! assignee, and carry plain `notes` plus limited `html_notes`.

use gonzalo_domain::{
    Actor, ActorRole, BodyFormat, Container, Provider, State, StateCategory, Ticket, TicketBody,
};
use gonzalo_ticket::{StateMapping, StateSignal};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub(crate) struct AsanaTask {
    pub gid: String,
    pub name: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub html_notes: Option<String>,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub assignee: Option<AsanaUser>,
    #[serde(default)]
    pub created_by: Option<AsanaUser>,
    #[serde(default)]
    pub memberships: Vec<AsanaMembership>,
    #[serde(default)]
    pub custom_fields: Vec<AsanaCustomField>,
    #[serde(default)]
    pub tags: Vec<AsanaNamed>,
    #[serde(default)]
    pub permalink_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AsanaUser {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AsanaMembership {
    #[serde(default)]
    pub project: Option<AsanaNamedGid>,
    #[serde(default)]
    pub section: Option<AsanaNamedGid>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AsanaNamedGid {
    pub gid: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AsanaNamed {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AsanaCustomField {
    pub gid: String,
    #[serde(default)]
    pub enum_value: Option<AsanaEnumValue>,
    #[serde(default)]
    pub display_value: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AsanaEnumValue {
    #[serde(default)]
    pub name: Option<String>,
}

/// The section name of the task's first (primary) membership, if any.
fn primary_section(task: &AsanaTask) -> Option<String> {
    task.memberships
        .first()
        .and_then(|m| m.section.as_ref())
        .and_then(|s| s.name.clone())
}

/// The value of a custom field by gid (enum option name, else display value).
fn custom_field_value(task: &AsanaTask, id: &str) -> Option<String> {
    task.custom_fields
        .iter()
        .find(|c| c.gid == id)
        .and_then(|c| {
            c.enum_value
                .as_ref()
                .and_then(|e| e.name.clone())
                .or_else(|| c.display_value.clone())
        })
}

/// Resolve category + raw status string per the connection's configured signal.
/// With no mapping, the `completed` bool is the intrinsic signal.
fn resolve_state(task: &AsanaTask, mapping: Option<&StateMapping>) -> (StateCategory, String) {
    if let Some(m) = mapping {
        match &m.signal {
            StateSignal::Completed => {
                let raw = if task.completed { "true" } else { "false" };
                return (m.category_of(raw), raw.to_string());
            }
            StateSignal::Section => {
                if let Some(section) = primary_section(task) {
                    return (m.category_of(&section), section);
                }
            }
            StateSignal::CustomField { id } => {
                if let Some(value) = custom_field_value(task, id) {
                    return (m.category_of(&value), value);
                }
            }
            // Other signals don't apply to Asana — fall through to intrinsic.
            _ => {}
        }
    }
    if task.completed {
        (StateCategory::Done, "completed".to_string())
    } else {
        (StateCategory::Open, "incomplete".to_string())
    }
}

/// Map an Asana task to a canonical [`Ticket`]. Multi-home memberships become
/// `containers` (first is primary).
pub(crate) fn task_to_ticket(task: &AsanaTask, mapping: Option<&StateMapping>) -> Ticket {
    let (category, raw_name) = resolve_state(task, mapping);

    let mut actors = Vec::new();
    if let Some(a) = &task.assignee {
        actors.push(Actor {
            role: ActorRole::Assignee,
            handle: a.name.clone(),
            display: None,
        });
    }
    if let Some(c) = &task.created_by {
        actors.push(Actor {
            role: ActorRole::Submitter,
            handle: c.name.clone(),
            display: None,
        });
    }

    let containers: Vec<Container> = task
        .memberships
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            m.project.as_ref().map(|p| Container {
                kind: "project".into(),
                id: p.gid.clone(),
                name: p.name.clone(),
                primary: i == 0,
            })
        })
        .collect();

    let body = match &task.html_notes {
        Some(html) => TicketBody {
            markdown: task.notes.clone().unwrap_or_default(),
            format: BodyFormat::Html,
            raw: Some(html.clone()),
        },
        None => TicketBody {
            markdown: task.notes.clone().unwrap_or_default(),
            format: BodyFormat::PlainText,
            raw: None,
        },
    };

    let mut fields = BTreeMap::new();
    if let Some(url) = &task.permalink_url {
        fields.insert("permalink_url".into(), serde_json::json!(url));
    }

    Ticket {
        provider: Provider::Asana,
        uid: task.gid.clone(),
        display: task.gid.clone(),
        item_type: "task".into(),
        title: task.name.clone(),
        state: State {
            category,
            resolution: None,
            raw_name,
            raw_id: None,
        },
        priority: None,
        actors,
        labels: task.tags.iter().map(|t| t.name.clone()).collect(),
        containers,
        links: vec![],
        body,
        fields,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = r#"{
        "gid": "1201",
        "name": "Ship Asana connector",
        "notes": "plain body",
        "html_notes": "<body>plain body</body>",
        "completed": false,
        "assignee": {"name": "John Ford"},
        "created_by": {"name": "Reporter"},
        "memberships": [
            {"project": {"gid": "p1", "name": "Sprint"}, "section": {"gid": "s1", "name": "In Progress"}},
            {"project": {"gid": "p2", "name": "Roadmap"}, "section": {"gid": "s2", "name": "Q3"}}
        ],
        "custom_fields": [
            {"gid": "cf1", "enum_value": {"name": "Doing"}}
        ],
        "tags": [{"name": "backend"}],
        "permalink_url": "https://app.asana.com/0/p1/1201"
    }"#;

    fn task() -> AsanaTask {
        serde_json::from_str(TASK).unwrap()
    }

    fn mapping(signal: StateSignal, pairs: &[(&str, StateCategory)]) -> StateMapping {
        let by_value = pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        StateMapping {
            signal,
            by_value,
            default: StateCategory::Open,
        }
    }

    #[test]
    fn intrinsic_completed_bool_without_mapping() {
        let t = task_to_ticket(&task(), None);
        assert_eq!(t.state.category, StateCategory::Open);
        assert_eq!(t.state.raw_name, "incomplete");
        // multi-home preserved; first membership is primary
        assert_eq!(t.containers.len(), 2);
        assert!(t.containers[0].primary);
        assert!(!t.containers[1].primary);
        assert_eq!(t.body.format, BodyFormat::Html);
        assert_eq!(t.actors[0].handle, "John Ford");
    }

    #[test]
    fn section_signal_drives_category() {
        let m = mapping(
            StateSignal::Section,
            &[("In Progress", StateCategory::InProgress)],
        );
        let t = task_to_ticket(&task(), Some(&m));
        assert_eq!(t.state.category, StateCategory::InProgress);
        assert_eq!(t.state.raw_name, "In Progress");
    }

    #[test]
    fn custom_field_signal_drives_category() {
        let m = mapping(
            StateSignal::CustomField { id: "cf1".into() },
            &[("Doing", StateCategory::InProgress)],
        );
        let t = task_to_ticket(&task(), Some(&m));
        assert_eq!(t.state.category, StateCategory::InProgress);
        assert_eq!(t.state.raw_name, "Doing");
    }

    #[test]
    fn completed_signal_maps_boolean() {
        let m = mapping(
            StateSignal::Completed,
            &[
                ("true", StateCategory::Done),
                ("false", StateCategory::Backlog),
            ],
        );
        let t = task_to_ticket(&task(), Some(&m));
        assert_eq!(t.state.category, StateCategory::Backlog);
    }
}
