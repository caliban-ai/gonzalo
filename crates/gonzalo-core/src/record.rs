//! The universal persisted unit and its classification.

use crate::{Identity, RecordKey, Revision};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a record represents. Drives the merge strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordKind {
    MemoryTier,
    Topic,
    Session,
    Checkpoint,
    /// A tracked work item imported from an external ticket platform.
    Ticket,
    /// An append-only comment/event on a ticket.
    TicketEvent,
}

/// How concurrent edits to a record of a given kind are reconciled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeClass {
    /// Edits union/concatenate (auto-memory topics, session transcripts).
    AppendOnly,
    /// Field-level 3-way merge against the common base.
    Structured,
    /// No safe automatic merge; surface to the caller.
    Opaque,
}

impl RecordKind {
    pub fn merge_class(self) -> MergeClass {
        match self {
            RecordKind::Topic | RecordKind::Session | RecordKind::TicketEvent => {
                MergeClass::AppendOnly
            }
            RecordKind::MemoryTier | RecordKind::Ticket => MergeClass::Structured,
            RecordKind::Checkpoint => MergeClass::Opaque,
        }
    }
}

/// A record body. M1 stores bytes inline; the `Blob` content-addressed
/// variant is reserved for M2 (large session/checkpoint externalization).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Body {
    Inline(Vec<u8>),
}

impl Body {
    /// The bytes used for content hashing and merging.
    pub fn bytes(&self) -> &[u8] {
        match self {
            Body::Inline(b) => b,
        }
    }
}

/// Provenance and labels for a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub author: Identity,
    pub origin_system: String,
    pub created: i64,
    pub updated: i64,
    pub labels: BTreeMap<String, String>,
}

/// The universal persisted unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub key: RecordKey,
    pub kind: RecordKind,
    pub revision: Revision,
    pub parent: Option<Revision>,
    pub body: Body,
    pub meta: Meta,
    pub links: Vec<RecordKey>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_class_is_assigned_per_kind() {
        assert_eq!(RecordKind::Topic.merge_class(), MergeClass::AppendOnly);
        assert_eq!(RecordKind::Session.merge_class(), MergeClass::AppendOnly);
        assert_eq!(RecordKind::MemoryTier.merge_class(), MergeClass::Structured);
        assert_eq!(RecordKind::Checkpoint.merge_class(), MergeClass::Opaque);
        assert_eq!(RecordKind::Ticket.merge_class(), MergeClass::Structured);
        assert_eq!(
            RecordKind::TicketEvent.merge_class(),
            MergeClass::AppendOnly
        );
    }

    #[test]
    fn body_exposes_bytes() {
        assert_eq!(Body::Inline(b"hi".to_vec()).bytes(), b"hi");
    }
}
