//! Conformance tests for `LinearSource` against a recorded GraphQL fixture.

use gonzalo_domain::StateCategory;
use gonzalo_ticket::conformance::{assert_ticket_invariants, assert_write_gating};
use gonzalo_ticket::{Cursor, TicketSource};
use gonzalo_ticket_linear::LinearSource;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn imports_via_graphql() {
    let server = MockServer::start().await;
    let body = serde_json::json!({"data": {"issues": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": [{
            "id": "u1", "identifier": "ENG-7", "title": "t", "priority": 2,
            "state": {"name": "In Progress", "type": "started"},
            "team": {"key": "ENG", "name": "Eng"}
        }]
    }}});
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let src = LinearSource::with_endpoint(&server.uri(), "key").unwrap();
    let page = src.fetch_changed(&Cursor::default()).await.unwrap();
    assert_eq!(page.tickets.len(), 1);
    assert_eq!(page.tickets[0].display, "ENG-7");
    assert_eq!(page.tickets[0].state.category, StateCategory::InProgress);
    assert_ticket_invariants(&page.tickets[0]);
    assert_write_gating(&src, "u1").await;
}

#[tokio::test]
async fn surfaces_graphql_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"errors": [{"message": "unauthorized"}]})),
        )
        .mount(&server)
        .await;
    let src = LinearSource::with_endpoint(&server.uri(), "key").unwrap();
    let err = src.fetch_changed(&Cursor::default()).await.unwrap_err();
    assert!(err.to_string().contains("unauthorized"));
}

#[tokio::test]
async fn set_state_resolves_team_state_then_updates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"issue": {"team": {"states": {"nodes": [
                {"id": "st-done", "type": "completed"}
            ]}}}}
        })))
        .mount(&server)
        .await;
    let src = LinearSource::with_endpoint(&server.uri(), "k").unwrap();
    src.set_state("u1", StateCategory::Done).await.unwrap();
    src.comment("u1", "hi").await.unwrap();
}
