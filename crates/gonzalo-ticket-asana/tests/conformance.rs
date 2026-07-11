//! Conformance tests for `AsanaSource`: the section and completed status-signal
//! policy variants, against recorded fixtures.

use gonzalo_domain::StateCategory;
use gonzalo_ticket::conformance::{assert_ticket_invariants, assert_write_gating};
use gonzalo_ticket::{Cursor, SourceError, StateMapping, StateSignal, TicketSource};
use gonzalo_ticket_asana::AsanaSource;
use std::collections::BTreeMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn task_fixture() -> serde_json::Value {
    serde_json::json!({"data": [{
        "gid": "1201", "name": "t", "completed": false,
        "memberships": [
            {"project": {"gid": "p1", "name": "Sprint"}, "section": {"gid": "s1", "name": "Doing"}},
            {"project": {"gid": "p2", "name": "Roadmap"}, "section": {"gid": "s2", "name": "Q3"}}
        ]
    }], "next_page": null})
}

async fn server_serving_tasks() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_fixture()))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn section_signal_policy_variant() {
    let server = server_serving_tasks().await;
    let mut by_value = BTreeMap::new();
    by_value.insert("Doing".to_string(), StateCategory::InProgress);
    let mapping = StateMapping {
        signal: StateSignal::Section,
        by_value,
        default: StateCategory::Open,
    };
    let src = AsanaSource::with_base(&server.uri(), "p1", "tok")
        .unwrap()
        .with_mapping(mapping);
    let page = src.fetch_changed(&Cursor::default()).await.unwrap();

    let t = &page.tickets[0];
    assert_eq!(t.state.category, StateCategory::InProgress);
    assert_eq!(t.containers.len(), 2, "multi-home memberships preserved");
    assert!(t.containers[0].primary);
    assert_ticket_invariants(t);
    assert_write_gating(&src, &t.uid).await;
}

#[tokio::test]
async fn completed_signal_policy_variant() {
    let server = server_serving_tasks().await;
    let mut by_value = BTreeMap::new();
    by_value.insert("false".to_string(), StateCategory::Backlog);
    by_value.insert("true".to_string(), StateCategory::Done);
    let mapping = StateMapping {
        signal: StateSignal::Completed,
        by_value,
        default: StateCategory::Open,
    };
    let src = AsanaSource::with_base(&server.uri(), "p1", "tok")
        .unwrap()
        .with_mapping(mapping);
    let page = src.fetch_changed(&Cursor::default()).await.unwrap();
    assert_eq!(page.tickets[0].state.category, StateCategory::Backlog);
}

#[tokio::test]
async fn writes_completed_and_story() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/tasks/1201"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tasks/1201/stories"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let src = AsanaSource::with_base(&server.uri(), "p1", "tok").unwrap();
    src.set_state("1201", StateCategory::Done).await.unwrap();
    src.comment("1201", "hi").await.unwrap();
}

fn section_mapping() -> StateMapping {
    let mut by_value = BTreeMap::new();
    by_value.insert("Doing".to_string(), StateCategory::InProgress);
    StateMapping {
        signal: StateSignal::Section,
        by_value,
        default: StateCategory::Open,
    }
}

#[tokio::test]
async fn non_terminal_set_state_under_section_is_unsupported() {
    // #143: with a section mapping in effect, a non-terminal move can't be
    // expressed via the `completed` bool, so it must error rather than report a
    // false success. No HTTP request should be made.
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/tasks/1201"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&server)
        .await;
    let src = AsanaSource::with_base(&server.uri(), "p1", "tok")
        .unwrap()
        .with_mapping(section_mapping());

    let err = src
        .set_state("1201", StateCategory::InProgress)
        .await
        .expect_err("non-terminal move under a section mapping must error");
    assert!(matches!(err, SourceError::Unsupported(_)), "got {err:?}");
}

#[tokio::test]
async fn terminal_set_state_still_works_under_section() {
    // #143: terminal moves remain expressible via the `completed` bool even when
    // a section mapping is configured.
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/tasks/1201"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let src = AsanaSource::with_base(&server.uri(), "p1", "tok")
        .unwrap()
        .with_mapping(section_mapping());
    src.set_state("1201", StateCategory::Done).await.unwrap();
}
