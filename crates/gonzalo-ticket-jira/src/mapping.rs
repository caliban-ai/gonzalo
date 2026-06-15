//! Pure mapping from Jira REST v3 issue JSON to the canonical [`Ticket`].
//!
//! Jira's status is a *native categorized* field: every workflow status belongs
//! to a `statusCategory` (`new` / `indeterminate` / `done`). The connector uses
//! that as the default category, but honors a per-connection [`StateMapping`]
//! override keyed by the raw status name (ADR 0010) — so an instance can map,
//! say, a custom "Blocked" status to `Pending` while everything else falls back
//! to `statusCategory`. Bodies are ADF (rich JSON); we extract plain text into
//! `markdown` and retain the raw ADF.

use gonzalo_domain::{
    Actor, ActorRole, BodyFormat, Container, Priority, PriorityLevel, Provider, Resolution, State,
    StateCategory, Ticket, TicketBody,
};
use gonzalo_ticket::StateMapping;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub(crate) struct JiraIssue {
    pub key: String,
    pub id: String,
    pub fields: JiraFields,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JiraFields {
    pub summary: String,
    #[serde(default)]
    pub description: Option<Value>,
    pub status: JiraStatus,
    #[serde(default)]
    pub issuetype: Option<JiraNamed>,
    #[serde(default)]
    pub priority: Option<JiraNamed>,
    #[serde(default)]
    pub assignee: Option<JiraUser>,
    #[serde(default)]
    pub reporter: Option<JiraUser>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub project: Option<JiraProject>,
    #[serde(default)]
    pub resolution: Option<JiraNamed>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JiraStatus {
    pub name: String,
    pub id: String,
    #[serde(rename = "statusCategory")]
    pub status_category: JiraStatusCategory,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JiraStatusCategory {
    pub key: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JiraNamed {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JiraUser {
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "accountId", default)]
    pub account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JiraProject {
    pub key: String,
    pub name: String,
}

/// Jira `statusCategory.key` → canonical category. The three keys are fixed by
/// Jira regardless of an instance's custom status names.
fn category_from_status_category(key: &str) -> StateCategory {
    match key {
        "indeterminate" => StateCategory::InProgress,
        "done" => StateCategory::Done,
        // "new" (To Do) and anything unrecognized
        _ => StateCategory::Open,
    }
}

fn resolution_from_name(name: &str) -> Resolution {
    match name.to_ascii_lowercase().as_str() {
        "done" | "fixed" => Resolution::Done,
        "won't do" | "wont do" | "won't fix" | "wontfix" => Resolution::WontDo,
        "duplicate" => Resolution::Duplicate,
        "cannot reproduce" | "cannot reproduce it" => Resolution::CannotReproduce,
        "invalid" => Resolution::Invalid,
        _ => Resolution::Other(name.to_string()),
    }
}

fn priority_from_name(name: &str) -> PriorityLevel {
    match name.to_ascii_lowercase().as_str() {
        "highest" | "critical" | "blocker" => PriorityLevel::Urgent,
        "high" => PriorityLevel::High,
        "low" => PriorityLevel::Low,
        "lowest" | "trivial" => PriorityLevel::None,
        // "medium" and anything unrecognized
        _ => PriorityLevel::Medium,
    }
}

/// Extract plain text from an ADF (Atlassian Document Format) node, inserting a
/// newline after each block (paragraph/heading).
pub(crate) fn adf_to_text(node: &Value) -> String {
    let mut out = String::new();
    collect_text(node, &mut out);
    out.trim_end().to_string()
}

fn collect_text(node: &Value, out: &mut String) {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(t)) = map.get("text") {
                out.push_str(t);
            }
            if let Some(content) = map.get("content") {
                collect_text(content, out);
            }
            if let Some(Value::String(ty)) = map.get("type")
                && matches!(ty.as_str(), "paragraph" | "heading")
            {
                out.push('\n');
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        _ => {}
    }
}

fn actor(user: &JiraUser, role: ActorRole) -> Actor {
    Actor {
        role,
        handle: user
            .account_id
            .clone()
            .unwrap_or_else(|| user.display_name.clone()),
        display: Some(user.display_name.clone()),
    }
}

/// Map a Jira issue to a canonical [`Ticket`]. `mapping`, if given, overrides the
/// category for statuses it names; otherwise `statusCategory` decides.
pub(crate) fn issue_to_ticket(issue: &JiraIssue, mapping: Option<&StateMapping>) -> Ticket {
    let f = &issue.fields;

    let category = mapping
        .and_then(|m| m.by_value.get(&f.status.name).copied())
        .unwrap_or_else(|| category_from_status_category(&f.status.status_category.key));

    let resolution = f.resolution.as_ref().map(|r| resolution_from_name(&r.name));

    let mut actors = Vec::new();
    if let Some(a) = &f.assignee {
        actors.push(actor(a, ActorRole::Assignee));
    }
    if let Some(r) = &f.reporter {
        actors.push(actor(r, ActorRole::Submitter));
    }

    let body = match &f.description {
        Some(adf) => TicketBody {
            markdown: adf_to_text(adf),
            format: BodyFormat::Adf,
            raw: Some(adf.to_string()),
        },
        None => TicketBody {
            markdown: String::new(),
            format: BodyFormat::Adf,
            raw: None,
        },
    };

    let containers = f
        .project
        .as_ref()
        .map(|p| {
            vec![Container {
                kind: "project".into(),
                id: p.key.clone(),
                name: Some(p.name.clone()),
                primary: true,
            }]
        })
        .unwrap_or_default();

    let mut fields = BTreeMap::new();
    fields.insert("jira_id".into(), serde_json::json!(issue.id));

    Ticket {
        provider: Provider::Jira,
        uid: issue.key.clone(),
        display: issue.key.clone(),
        item_type: f
            .issuetype
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "issue".into()),
        title: f.summary.clone(),
        state: State {
            category,
            resolution,
            raw_name: f.status.name.clone(),
            raw_id: Some(f.status.id.clone()),
        },
        priority: f.priority.as_ref().map(|p| Priority {
            level: priority_from_name(&p.name),
            raw: Some(p.name.clone()),
        }),
        actors,
        labels: f.labels.clone(),
        containers,
        links: vec![],
        body,
        fields,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IN_PROGRESS: &str = r#"{
        "key": "ENG-42",
        "id": "10042",
        "fields": {
            "summary": "Wire the Jira connector",
            "description": {"type":"doc","content":[
                {"type":"paragraph","content":[{"type":"text","text":"First line."}]},
                {"type":"paragraph","content":[{"type":"text","text":"Second line."}]}
            ]},
            "status": {"name":"In Progress","id":"3","statusCategory":{"key":"indeterminate"}},
            "issuetype": {"name":"Story"},
            "priority": {"name":"High"},
            "assignee": {"displayName":"John Ford","accountId":"acc-1"},
            "reporter": {"displayName":"Reporter","accountId":"acc-2"},
            "labels": ["backend","ticket"],
            "project": {"key":"ENG","name":"Engineering"}
        }
    }"#;

    #[test]
    fn maps_in_progress_story() {
        let issue: JiraIssue = serde_json::from_str(IN_PROGRESS).unwrap();
        let t = issue_to_ticket(&issue, None);

        assert_eq!(t.provider, Provider::Jira);
        assert_eq!(t.uid, "ENG-42");
        assert_eq!(t.item_type, "Story");
        assert_eq!(t.state.category, StateCategory::InProgress);
        assert_eq!(t.state.raw_name, "In Progress");
        assert_eq!(t.priority.unwrap().level, PriorityLevel::High);
        assert_eq!(t.body.format, BodyFormat::Adf);
        assert_eq!(t.body.markdown, "First line.\nSecond line.");
        assert!(t.body.raw.is_some());
        assert_eq!(t.containers[0].id, "ENG");
        assert_eq!(t.actors[0].role, ActorRole::Assignee);
        assert_eq!(t.actors[0].handle, "acc-1");
    }

    #[test]
    fn done_status_category_with_resolution() {
        let json = r#"{"key":"ENG-1","id":"1","fields":{
            "summary":"x",
            "status":{"name":"Done","id":"5","statusCategory":{"key":"done"}},
            "resolution":{"name":"Won't Do"}
        }}"#;
        let issue: JiraIssue = serde_json::from_str(json).unwrap();
        let t = issue_to_ticket(&issue, None);
        assert_eq!(t.state.category, StateCategory::Done);
        assert_eq!(t.state.resolution, Some(Resolution::WontDo));
    }

    #[test]
    fn state_mapping_overrides_status_category() {
        // "Blocked" is statusCategory=indeterminate (would be InProgress), but
        // a per-connection mapping reclassifies it as Pending.
        let json = r#"{"key":"ENG-2","id":"2","fields":{
            "summary":"x",
            "status":{"name":"Blocked","id":"7","statusCategory":{"key":"indeterminate"}}
        }}"#;
        let issue: JiraIssue = serde_json::from_str(json).unwrap();

        let mut by_value = BTreeMap::new();
        by_value.insert("Blocked".to_string(), StateCategory::Pending);
        let mapping = StateMapping {
            signal: gonzalo_ticket::StateSignal::NativeStatus,
            by_value,
            default: StateCategory::Open,
        };

        assert_eq!(
            issue_to_ticket(&issue, None).state.category,
            StateCategory::InProgress
        );
        assert_eq!(
            issue_to_ticket(&issue, Some(&mapping)).state.category,
            StateCategory::Pending
        );
    }

    #[test]
    fn adf_extracts_text_across_blocks() {
        let adf = serde_json::json!({
            "type":"doc","content":[
                {"type":"heading","content":[{"type":"text","text":"Title"}]},
                {"type":"paragraph","content":[
                    {"type":"text","text":"a "},
                    {"type":"text","text":"b"}
                ]}
            ]
        });
        assert_eq!(adf_to_text(&adf), "Title\na b");
    }
}
