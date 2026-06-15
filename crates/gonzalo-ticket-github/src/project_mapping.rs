//! Pure mapping from GitHub Projects v2 GraphQL item JSON to the canonical
//! [`Ticket`]. The board's Status column is a *native single-select field*
//! (`StateSignal::NativeStatus`), so unlike the REST issue connector this takes
//! a per-connection [`StateMapping`] (ADR 0010). Draft items and pull requests
//! have no stable issue uid and are skipped (the caller `filter_map`s `None`).

use gonzalo_domain::{
    Actor, ActorRole, BodyFormat, Container, Provider, State, Ticket, TicketBody,
};
use gonzalo_ticket::StateMapping;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub(crate) struct GqlResponse {
    pub data: GqlData,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlData {
    pub organization: GqlOrg,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlOrg {
    #[serde(rename = "projectV2")]
    pub project: GqlProject,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlProject {
    pub items: GqlItems,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlItems {
    #[serde(rename = "pageInfo")]
    pub page_info: GqlPageInfo,
    pub nodes: Vec<GqlItem>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlPageInfo {
    #[serde(rename = "hasNextPage")]
    pub has_next_page: bool,
    #[serde(rename = "endCursor")]
    pub end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlItem {
    /// The "Status" single-select field value; `None` when the card has no
    /// status set, or `name: None` when the field isn't a single-select.
    #[serde(rename = "fieldValueByName")]
    pub status: Option<GqlStatus>,
    pub content: Option<GqlContent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlStatus {
    pub name: Option<String>,
    #[serde(rename = "optionId")]
    pub option_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlContent {
    #[serde(rename = "__typename")]
    pub typename: String,
    pub number: Option<u64>,
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    pub repository: Option<GqlRepo>,
    #[serde(default)]
    pub labels: Option<GqlNodes<GqlLabel>>,
    #[serde(default)]
    pub assignees: Option<GqlNodes<GqlUser>>,
    #[serde(default)]
    pub author: Option<GqlUser>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlRepo {
    #[serde(rename = "nameWithOwner")]
    pub name_with_owner: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlNodes<T> {
    pub nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlLabel {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GqlUser {
    pub login: String,
}

/// Map one board item to a [`Ticket`], or `None` if it is a draft / PR / has no
/// linked issue. `mapping` resolves the Status column name to a category.
pub(crate) fn item_to_ticket(item: &GqlItem, mapping: &StateMapping) -> Option<Ticket> {
    let content = item.content.as_ref()?;
    if content.typename != "Issue" {
        return None; // draft cards and pull requests are skipped
    }
    let number = content.number?;
    let repo = content.repository.as_ref()?.name_with_owner.clone();

    let raw_status = item
        .status
        .as_ref()
        .and_then(|s| s.name.clone())
        .unwrap_or_default();
    let category = mapping.category_of(&raw_status);

    let mut actors: Vec<Actor> = content
        .assignees
        .as_ref()
        .map(|a| {
            a.nodes
                .iter()
                .map(|u| Actor {
                    role: ActorRole::Assignee,
                    handle: u.login.clone(),
                    display: None,
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(u) = &content.author {
        actors.push(Actor {
            role: ActorRole::Submitter,
            handle: u.login.clone(),
            display: None,
        });
    }

    Some(Ticket {
        provider: Provider::GitHub,
        uid: format!("{repo}#{number}"),
        display: format!("#{number}"),
        item_type: "issue".into(),
        title: content.title.clone().unwrap_or_default(),
        state: State {
            category,
            resolution: None,
            raw_name: raw_status,
            raw_id: item.status.as_ref().and_then(|s| s.option_id.clone()),
        },
        priority: None,
        actors,
        labels: content
            .labels
            .as_ref()
            .map(|l| l.nodes.iter().map(|x| x.name.clone()).collect())
            .unwrap_or_default(),
        containers: vec![Container {
            kind: "repo".into(),
            id: repo,
            name: None,
            primary: true,
        }],
        links: vec![],
        body: TicketBody {
            markdown: content.body.clone().unwrap_or_default(),
            format: BodyFormat::Markdown,
            raw: None,
        },
        fields: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_domain::StateCategory;
    use gonzalo_ticket::StateSignal;

    fn board_mapping() -> StateMapping {
        let mut by_value = BTreeMap::new();
        by_value.insert("Todo".into(), StateCategory::Open);
        by_value.insert("In Progress".into(), StateCategory::InProgress);
        by_value.insert("Blocked".into(), StateCategory::Pending);
        by_value.insert("Done".into(), StateCategory::Done);
        StateMapping {
            signal: StateSignal::NativeStatus,
            by_value,
            default: StateCategory::Open,
        }
    }

    const PAGE: &str = r#"{
      "data": { "organization": { "projectV2": { "items": {
        "pageInfo": { "hasNextPage": false, "endCursor": "Y3Vyc29yOjE=" },
        "nodes": [
          {
            "fieldValueByName": { "name": "In Progress", "optionId": "abc123" },
            "content": {
              "__typename": "Issue",
              "number": 18,
              "title": "ticket capability layer",
              "body": "do the thing",
              "repository": { "nameWithOwner": "caliban-ai/gonzalo" },
              "labels": { "nodes": [{ "name": "kind/feat" }] },
              "assignees": { "nodes": [{ "login": "johnford2002" }] },
              "author": { "login": "reporter1" }
            }
          },
          {
            "fieldValueByName": null,
            "content": { "__typename": "DraftIssue", "title": "just a draft" }
          },
          {
            "fieldValueByName": { "name": "Done", "optionId": "z9" },
            "content": { "__typename": "PullRequest", "number": 21, "title": "a PR",
              "repository": { "nameWithOwner": "caliban-ai/gonzalo" } }
          }
        ]
      } } } }
    }"#;

    #[test]
    fn maps_issue_items_and_skips_drafts_and_prs() {
        let resp: GqlResponse = serde_json::from_str(PAGE).unwrap();
        let m = board_mapping();
        let tickets: Vec<Ticket> = resp
            .data
            .organization
            .project
            .items
            .nodes
            .iter()
            .filter_map(|n| item_to_ticket(n, &m))
            .collect();

        assert_eq!(tickets.len(), 1, "draft and PR are skipped");
        let t = &tickets[0];
        assert_eq!(t.uid, "caliban-ai/gonzalo#18");
        assert_eq!(t.display, "#18");
        assert_eq!(t.state.category, StateCategory::InProgress);
        assert_eq!(t.state.raw_name, "In Progress");
        assert_eq!(t.state.raw_id.as_deref(), Some("abc123"));
        assert_eq!(t.labels, vec!["kind/feat"]);
        assert_eq!(t.actors[0].role, ActorRole::Assignee);
        assert_eq!(t.actors[0].handle, "johnford2002");
        assert!(t.actors.iter().any(|a| a.role == ActorRole::Submitter));
        assert_eq!(t.containers[0].id, "caliban-ai/gonzalo");
    }

    #[test]
    fn unknown_status_falls_back_to_default() {
        let item = GqlItem {
            status: Some(GqlStatus {
                name: Some("Weird".into()),
                option_id: None,
            }),
            content: Some(GqlContent {
                typename: "Issue".into(),
                number: Some(7),
                title: Some("x".into()),
                body: None,
                repository: Some(GqlRepo {
                    name_with_owner: "caliban-ai/gonzalo".into(),
                }),
                labels: None,
                assignees: None,
                author: None,
            }),
        };
        let t = item_to_ticket(&item, &board_mapping()).unwrap();
        assert_eq!(t.state.category, StateCategory::Open); // mapping default
    }
}
