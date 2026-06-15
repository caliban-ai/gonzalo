//! Reusable conformance checks for `TicketSource` implementations.
//!
//! The ticket analogue of `gonzalo-core`'s substrate conformance suite
//! (ADR 0006/0010): the executable definition of what every connector must do.
//! Connectors run these in their own tests against **recorded fixtures**
//! (wiremock), keyed on policy variants where a platform's status signal is
//! configurable (e.g. GitLab scoped-label vs intrinsic, Asana section vs
//! completed). These helpers are provider-agnostic — they assert the invariants
//! that must hold for any imported ticket and any source.

use crate::{SourceError, TicketSource};
use gonzalo_domain::{StateCategory, Ticket};

/// Invariants every imported ticket must satisfy, regardless of provider.
///
/// Panics (test-style) on violation so connectors can call it directly in a
/// `#[tokio::test]`.
pub fn assert_ticket_invariants(t: &Ticket) {
    assert!(!t.uid.is_empty(), "ticket uid must be non-empty");
    assert!(!t.display.is_empty(), "ticket display must be non-empty");
    assert!(
        !t.item_type.is_empty(),
        "ticket item_type must be non-empty"
    );
    assert!(
        !t.state.raw_name.is_empty(),
        "state.raw_name must be retained for fidelity"
    );
    let primaries = t.containers.iter().filter(|c| c.primary).count();
    assert!(
        primaries <= 1,
        "at most one primary container is allowed, found {primaries}"
    );
}

/// A source must honor its declared write capability: if `capabilities().push`
/// is false, `set_state` must fail with [`SourceError::Unsupported`] rather than
/// silently no-op or panic.
pub async fn assert_write_gating<S: TicketSource + Sync>(source: &S, uid: &str) {
    if source.capabilities().push {
        return;
    }
    let err = source.set_state(uid, StateCategory::Done).await.err();
    assert!(
        matches!(err, Some(SourceError::Unsupported(_))),
        "a read-only source must reject set_state with Unsupported, got {err:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemorySource;
    use gonzalo_domain::{BodyFormat, Container, Provider, State, TicketBody};
    use std::collections::BTreeMap;

    fn ticket() -> Ticket {
        Ticket {
            provider: Provider::GitHub,
            uid: "o/r#1".into(),
            display: "#1".into(),
            item_type: "issue".into(),
            title: "t".into(),
            state: State {
                category: StateCategory::Open,
                resolution: None,
                raw_name: "open".into(),
                raw_id: None,
            },
            priority: None,
            actors: vec![],
            labels: vec![],
            containers: vec![Container {
                kind: "repo".into(),
                id: "o/r".into(),
                name: None,
                primary: true,
            }],
            links: vec![],
            body: TicketBody {
                markdown: String::new(),
                format: BodyFormat::Markdown,
                raw: None,
            },
            fields: BTreeMap::new(),
        }
    }

    #[test]
    fn invariants_pass_for_a_well_formed_ticket() {
        assert_ticket_invariants(&ticket());
    }

    #[test]
    #[should_panic(expected = "uid must be non-empty")]
    fn invariants_catch_empty_uid() {
        let mut t = ticket();
        t.uid = String::new();
        assert_ticket_invariants(&t);
    }

    #[tokio::test]
    async fn write_gating_holds_for_read_only_source() {
        let src = InMemorySource::new(vec![ticket()]);
        assert_write_gating(&src, "o/r#1").await;
    }
}
