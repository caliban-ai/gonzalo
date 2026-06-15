//! Per-connection field/state mapping policy (ADR 0010).
//!
//! The signal that carries a ticket's status is configured **per connection**,
//! not fixed per provider — GitLab free encodes workflow in `workflow::` scoped
//! labels while Premium uses a native status field; Asana uses a `completed`
//! flag, a section, or a custom field depending on the workspace. A
//! [`StateMapping`] declares which signal a connection reads and how its raw
//! values translate onto the normalized [`StateCategory`]. The connector
//! extracts the raw value per the signal; the mapping is pure translation, so it
//! is trivially testable in isolation.

use gonzalo_domain::StateCategory;
use std::collections::BTreeMap;

/// Where a connection reads a ticket's status from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateSignal {
    /// The platform's intrinsic open/closed (+ reason) field.
    IntrinsicState,
    /// A categorized native status field (Jira / Linear / GitLab-Premium / ADO).
    NativeStatus,
    /// A scoped-label namespace, e.g. GitLab `workflow::`.
    ScopedLabel { prefix: String },
    /// A board section / column (Asana, Trello).
    Section,
    /// A custom field used as status, addressed by field id.
    CustomField { id: String },
    /// A boolean completed flag (Asana).
    Completed,
}

/// Resolves a provider's raw status value to a normalized [`StateCategory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateMapping {
    /// Where the connector reads the raw status value from.
    pub signal: StateSignal,
    /// Raw value (status name / label suffix / section name) → category.
    pub by_value: BTreeMap<String, StateCategory>,
    /// Category used when no `by_value` entry matches the raw value.
    pub default: StateCategory,
}

impl StateMapping {
    /// Translate a raw status value to a normalized category, falling back to
    /// [`StateMapping::default`] when nothing matches.
    ///
    /// Matching prefers an exact key, then falls back to a case-insensitive
    /// match — a board's status/column names are unique within one field, so a
    /// case-only variant (e.g. config `"In Progress"` vs board `"In progress"`)
    /// can never collide with a genuinely different column.
    pub fn category_of(&self, raw_value: &str) -> StateCategory {
        if let Some(cat) = self.by_value.get(raw_value) {
            return *cat;
        }
        self.by_value
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(raw_value))
            .map(|(_, cat)| *cat)
            .unwrap_or(self.default)
    }
}

/// Maps canonical ticket fields onto a provider's arbitrary field ids, for
/// schemaless platforms (Monday / Airtable) where even title, assignee, and
/// status are user-named columns. Unset entries fall back to the connector's
/// built-in field knowledge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldMapping {
    pub title: Option<String>,
    pub assignee: Option<String>,
    pub priority: Option<String>,
    /// Provider field id whose value carries status (paired with a
    /// [`StateMapping`] whose signal is [`StateSignal::CustomField`]).
    pub status: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gitlab_free_mapping() -> StateMapping {
        // GitLab free: workflow encoded in `workflow::` scoped labels; the
        // connector strips the prefix and hands us the suffix.
        let mut by_value = BTreeMap::new();
        by_value.insert("in review".into(), StateCategory::InProgress);
        by_value.insert("development".into(), StateCategory::InProgress);
        by_value.insert("blocked".into(), StateCategory::Pending);
        StateMapping {
            signal: StateSignal::ScopedLabel {
                prefix: "workflow::".into(),
            },
            by_value,
            default: StateCategory::Open,
        }
    }

    #[test]
    fn maps_known_raw_value_to_category() {
        let m = gitlab_free_mapping();
        assert_eq!(m.category_of("in review"), StateCategory::InProgress);
        assert_eq!(m.category_of("blocked"), StateCategory::Pending);
    }

    #[test]
    fn unmapped_raw_value_falls_back_to_default() {
        let m = gitlab_free_mapping();
        assert_eq!(m.category_of("something-bespoke"), StateCategory::Open);
    }

    #[test]
    fn matches_case_insensitively_when_no_exact_key() {
        // Config key "In Progress" should still resolve a board column reported
        // as "In progress" (the real caliban-ai #1 casing). Exact match wins
        // when present; case-only variants fall through to the case-insensitive
        // pass rather than the default.
        let mut by_value = BTreeMap::new();
        by_value.insert("In Progress".into(), StateCategory::InProgress);
        by_value.insert("Done".into(), StateCategory::Done);
        let m = StateMapping {
            signal: StateSignal::NativeStatus,
            by_value,
            default: StateCategory::Open,
        };
        assert_eq!(m.category_of("In Progress"), StateCategory::InProgress); // exact
        assert_eq!(m.category_of("In progress"), StateCategory::InProgress); // case-insensitive
        assert_eq!(m.category_of("DONE"), StateCategory::Done);
        assert_eq!(m.category_of("Backlog"), StateCategory::Open); // truly unmapped → default
    }

    #[test]
    fn asana_completed_signal_maps_both_booleans() {
        // Asana: status is a `completed` bool; connector passes "true"/"false".
        let mut by_value = BTreeMap::new();
        by_value.insert("true".into(), StateCategory::Done);
        by_value.insert("false".into(), StateCategory::Open);
        let m = StateMapping {
            signal: StateSignal::Completed,
            by_value,
            default: StateCategory::Open,
        };
        assert_eq!(m.category_of("true"), StateCategory::Done);
        assert_eq!(m.category_of("false"), StateCategory::Open);
    }
}
