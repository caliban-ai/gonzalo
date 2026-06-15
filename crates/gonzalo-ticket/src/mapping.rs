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

/// Failure resolving a normalized category back to a board column for write-back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReverseError {
    /// No column maps to this category and no override was given.
    #[error("no column maps to category {0:?}")]
    Unmapped(StateCategory),
    /// More than one column maps to this category; an explicit override is required.
    #[error(
        "category {0:?} is ambiguous across columns {1:?}; set an explicit set_targets override"
    )]
    Ambiguous(StateCategory, Vec<String>),
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

    /// Resolve a normalized `category` back to the board column name to write.
    ///
    /// `overrides` (category→column) win outright. Otherwise the column is the
    /// unique `by_value` key whose category equals `category`. The `default`
    /// category is a read-time fallback only and is never a write target.
    pub fn column_for(
        &self,
        category: StateCategory,
        overrides: &BTreeMap<StateCategory, String>,
    ) -> Result<String, ReverseError> {
        if let Some(col) = overrides.get(&category) {
            return Ok(col.clone());
        }
        let mut matches: Vec<String> = self
            .by_value
            .iter()
            .filter(|(_, c)| **c == category)
            .map(|(k, _)| k.clone())
            .collect();
        matches.sort(); // deterministic order for the ambiguity message
        match matches.len() {
            0 => Err(ReverseError::Unmapped(category)),
            1 => Ok(matches.swap_remove(0)),
            _ => Err(ReverseError::Ambiguous(category, matches)),
        }
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

    fn board_mapping() -> StateMapping {
        // 1:1 columns like the caliban-ai board.
        let mut by_value = BTreeMap::new();
        by_value.insert("Backlog".into(), StateCategory::Backlog);
        by_value.insert("In progress".into(), StateCategory::InProgress);
        by_value.insert("Done".into(), StateCategory::Done);
        StateMapping {
            signal: StateSignal::NativeStatus,
            by_value,
            default: StateCategory::Open,
        }
    }

    #[test]
    fn column_for_inverts_unique_mapping() {
        let m = board_mapping();
        let none: BTreeMap<StateCategory, String> = BTreeMap::new();
        assert_eq!(
            m.column_for(StateCategory::InProgress, &none).unwrap(),
            "In progress"
        );
        assert_eq!(m.column_for(StateCategory::Done, &none).unwrap(), "Done");
    }

    #[test]
    fn column_for_unmapped_category_errors() {
        let m = board_mapping();
        let none: BTreeMap<StateCategory, String> = BTreeMap::new();
        assert_eq!(
            m.column_for(StateCategory::Pending, &none),
            Err(ReverseError::Unmapped(StateCategory::Pending))
        );

        // An override short-circuits before the Unmapped path: a category with
        // no by_value column can still resolve to a column via set_targets.
        let mut overrides = BTreeMap::new();
        overrides.insert(StateCategory::Pending, "Blocked".to_string());
        assert_eq!(
            m.column_for(StateCategory::Pending, &overrides).unwrap(),
            "Blocked"
        );
    }

    #[test]
    fn column_for_override_wins_and_resolves_ambiguity() {
        // Two columns share the Done category → ambiguous without an override.
        let mut by_value = BTreeMap::new();
        by_value.insert("Shipped".into(), StateCategory::Done);
        by_value.insert("Done".into(), StateCategory::Done);
        let m = StateMapping {
            signal: StateSignal::NativeStatus,
            by_value,
            default: StateCategory::Open,
        };

        let none: BTreeMap<StateCategory, String> = BTreeMap::new();
        match m.column_for(StateCategory::Done, &none) {
            Err(ReverseError::Ambiguous(StateCategory::Done, cols)) => {
                assert_eq!(cols, vec!["Done".to_string(), "Shipped".to_string()]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }

        let mut overrides = BTreeMap::new();
        overrides.insert(StateCategory::Done, "Shipped".to_string());
        assert_eq!(
            m.column_for(StateCategory::Done, &overrides).unwrap(),
            "Shipped"
        );
    }
}
