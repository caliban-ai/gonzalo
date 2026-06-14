//! Ticket (tracked work item) views — a normalized work-item model over
//! external ticket platforms (GitHub, Jira, Linear, GitLab, Asana, …).
//!
//! See ADR 0010. The canonical model carries a normalized spine (state
//! category, resolution, actor roles, priority) plus lossless raw fields, so a
//! provider's native data round-trips while cross-platform queries key off the
//! normalized form. The provider boundary and per-connection field/state
//! mapping live in the `gonzalo-ticket` capability crate; these are the
//! persisted typed views.

use crate::codec::RecordCodec;
use gonzalo_core::{RecordKey, RecordKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The platform a ticket was imported from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    GitHub,
    Jira,
    Linear,
    GitLab,
    Asana,
    AzureDevOps,
    Bugzilla,
    /// Any platform without a dedicated variant (Zendesk, Monday, …).
    Other(String),
}

/// Normalized lifecycle category — the cross-platform spine (ADR 0010). Each
/// provider's native status maps onto exactly one of these via a
/// `StateMapping`; the raw status is retained on [`State`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateCategory {
    Triage,
    Backlog,
    Open,
    InProgress,
    /// Waiting on an external party (support "pending"; dev "blocked").
    Pending,
    Done,
    Canceled,
}

/// Why a ticket closed — the second state axis Bugzilla and Jira need
/// (status × resolution). `Duplicate` pairs with a `Link` to the canonical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    Done,
    WontDo,
    Duplicate,
    Invalid,
    CannotReproduce,
    Moved,
    Other(String),
}

/// Normalized state: a category, an optional resolution, and the raw
/// provider-native status kept for fidelity / write-back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub category: StateCategory,
    pub resolution: Option<Resolution>,
    pub raw_name: String,
    pub raw_id: Option<String>,
}

/// What capacity a person is involved in. Support/ITSM platforms distinguish
/// requester from assignee from submitter; dev trackers mostly use `Assignee`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActorRole {
    Requester,
    Assignee,
    Submitter,
    Follower,
}

/// A person involved with a ticket, and in what capacity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub role: ActorRole,
    pub handle: String,
    pub display: Option<String>,
}

/// A container a ticket is filed in (repo, project, board, section). Tickets
/// may be multi-homed (Asana), so `Ticket` carries a list; `primary` marks the
/// container whose mapping resolves the canonical state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    pub kind: String,
    pub id: String,
    pub name: Option<String>,
    pub primary: bool,
}

/// The nature of a relationship between tickets/records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkKind {
    Blocks,
    BlockedBy,
    Relates,
    Parent,
    Child,
    Duplicate,
}

/// What a link points at: a first-class record (once ingested) or an external
/// reference that hasn't been imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkTarget {
    Record(RecordKey),
    External(String),
}

/// A typed relationship to another record or external ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub kind: LinkKind,
    pub target: LinkTarget,
}

/// The source format of a ticket body. The normalized `markdown` is always
/// populated; `raw` round-trips the native form (e.g. Jira ADF JSON).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BodyFormat {
    Markdown,
    Adf,
    Html,
    PlainText,
}

/// Body text with its source format; `raw` retained for lossless round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketBody {
    pub markdown: String,
    pub format: BodyFormat,
    pub raw: Option<String>,
}

/// Normalized priority ordinal (ascending: `None` < … < `Urgent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PriorityLevel {
    None,
    Low,
    Medium,
    High,
    Urgent,
}

/// Normalized priority plus the raw provider value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Priority {
    pub level: PriorityLevel,
    pub raw: Option<String>,
}

/// A tracked work item, normalized across platforms (ADR 0010).
///
/// Does not derive `Eq`: `fields` holds arbitrary `serde_json::Value`s for
/// unmapped provider data, and `Value` is `PartialEq` but not `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    pub provider: Provider,
    /// Stable provider-global id — the basis for this record's `RecordKey`.
    pub uid: String,
    /// Human display id ("ENG-123", "PROJ-45", "#7").
    pub display: String,
    /// Work-item type ("bug", "story", "incident"); provider/process-defined.
    pub item_type: String,
    pub title: String,
    pub state: State,
    pub priority: Option<Priority>,
    pub actors: Vec<Actor>,
    pub labels: Vec<String>,
    pub containers: Vec<Container>,
    pub links: Vec<Link>,
    pub body: TicketBody,
    /// Custom / unmapped provider fields, retained verbatim.
    pub fields: BTreeMap<String, serde_json::Value>,
}
impl RecordCodec for Ticket {}
impl Ticket {
    pub const KIND: RecordKind = RecordKind::Ticket;
}

/// An append-only comment or lifecycle event on a ticket. Stored under the
/// `TicketEvent` kind, which merges by union (ADR 0005).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketEvent {
    /// The `uid` of the ticket this event belongs to.
    pub ticket_uid: String,
    /// Event nature: "comment", "state_change", "sla", …
    pub kind: String,
    pub author: String,
    /// Unix timestamp (seconds).
    pub at: i64,
    pub body: String,
}
impl RecordCodec for TicketEvent {}
impl TicketEvent {
    pub const KIND: RecordKind = RecordKind::TicketEvent;
}

#[cfg(test)]
mod tests {
    use super::*;
    use gonzalo_core::RecordKey;

    fn sample_ticket() -> Ticket {
        let mut fields = BTreeMap::new();
        fields.insert("story_points".into(), serde_json::json!(5));
        Ticket {
            provider: Provider::GitHub,
            uid: "gh:caliban-ai/gonzalo#15".into(),
            display: "#15".into(),
            item_type: "issue".into(),
            title: "design: ticket-system capability layer".into(),
            state: State {
                category: StateCategory::InProgress,
                resolution: None,
                raw_name: "In progress".into(),
                raw_id: Some("47fc9ee4".into()),
            },
            priority: Some(Priority {
                level: PriorityLevel::Medium,
                raw: Some("important-longterm".into()),
            }),
            actors: vec![Actor {
                role: ActorRole::Assignee,
                handle: "johnford2002".into(),
                display: None,
            }],
            labels: vec!["area/integration".into(), "kind/design".into()],
            containers: vec![Container {
                kind: "repo".into(),
                id: "caliban-ai/gonzalo".into(),
                name: Some("gonzalo".into()),
                primary: true,
            }],
            links: vec![Link {
                kind: LinkKind::Relates,
                target: LinkTarget::Record(RecordKey::new("caliban", "tickets", "gh:16")),
            }],
            body: TicketBody {
                markdown: "Model tickets as a capability layer.".into(),
                format: BodyFormat::Markdown,
                raw: None,
            },
            fields,
        }
    }

    #[test]
    fn ticket_roundtrips_through_body() {
        let t = sample_ticket();
        assert_eq!(Ticket::from_body(&t.to_body().unwrap()).unwrap(), t);
        assert_eq!(Ticket::KIND, RecordKind::Ticket);
    }

    #[test]
    fn ticket_event_roundtrips_through_body() {
        let e = TicketEvent {
            ticket_uid: "gh:caliban-ai/gonzalo#15".into(),
            kind: "comment".into(),
            author: "johnford2002".into(),
            at: 1_750_000_000,
            body: "Begin the build.".into(),
        };
        assert_eq!(TicketEvent::from_body(&e.to_body().unwrap()).unwrap(), e);
        assert_eq!(TicketEvent::KIND, RecordKind::TicketEvent);
    }

    #[test]
    fn priority_levels_are_ordered() {
        assert!(PriorityLevel::None < PriorityLevel::Urgent);
        assert!(PriorityLevel::Low < PriorityLevel::High);
    }
}
