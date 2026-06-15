//! Conformance tests for `JiraSource` against recorded fixtures, including the
//! per-connection `StateMapping` policy variant.

use gonzalo_domain::StateCategory;
use gonzalo_ticket::conformance::{assert_ticket_invariants, assert_write_gating};
use gonzalo_ticket::{Cursor, StateMapping, StateSignal, TicketSource};
use gonzalo_ticket_jira::JiraSource;
use std::collections::BTreeMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn imports_via_search_jql() {
    let server = MockServer::start().await;
    let body = serde_json::json!({"issues": [{
        "key": "ENG-1", "id": "1", "fields": {
            "summary": "s",
            "status": {"name": "In Progress", "id": "3", "statusCategory": {"key": "indeterminate"}},
            "issuetype": {"name": "Story"},
            "project": {"key": "ENG", "name": "Eng"}
        }
    }], "nextPageToken": null});
    Mock::given(method("POST"))
        .and(path("/rest/api/3/search/jql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let src = JiraSource::new(&server.uri(), "e@x", "tok").unwrap();
    let page = src.fetch_changed(&Cursor::default()).await.unwrap();
    assert_eq!(page.tickets.len(), 1);
    assert_eq!(page.tickets[0].state.category, StateCategory::InProgress);
    assert_ticket_invariants(&page.tickets[0]);
    assert_write_gating(&src, "ENG-1").await;
}

#[tokio::test]
async fn state_mapping_policy_variant_overrides_status_category() {
    let server = MockServer::start().await;
    // "Blocked" is statusCategory=indeterminate (-> InProgress by default), but
    // this connection's policy reclassifies it as Pending.
    let body = serde_json::json!({"issues": [{
        "key": "ENG-2", "id": "2", "fields": {
            "summary": "s",
            "status": {"name": "Blocked", "id": "7", "statusCategory": {"key": "indeterminate"}}
        }
    }]});
    Mock::given(method("POST"))
        .and(path("/rest/api/3/search/jql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let mut by_value = BTreeMap::new();
    by_value.insert("Blocked".to_string(), StateCategory::Pending);
    let mapping = StateMapping {
        signal: StateSignal::NativeStatus,
        by_value,
        default: StateCategory::Open,
    };
    let src = JiraSource::new(&server.uri(), "e", "t")
        .unwrap()
        .with_mapping(mapping);
    let page = src.fetch_changed(&Cursor::default()).await.unwrap();
    assert_eq!(page.tickets[0].state.category, StateCategory::Pending);
}
