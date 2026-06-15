//! Conformance tests: drive `GitHubSource` against recorded fixtures served by
//! a mock HTTP server, and run the shared `gonzalo_ticket::conformance` checks.

use gonzalo_domain::{Resolution, StateCategory};
use gonzalo_ticket::conformance::{assert_ticket_invariants, assert_write_gating};
use gonzalo_ticket::{Cursor, TicketSource};
use gonzalo_ticket_github::GitHubSource;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn imports_issues_and_filters_pull_requests() {
    let server = MockServer::start().await;
    let body = serde_json::json!([
        {
            "number": 15, "node_id": "I_1", "title": "design",
            "body": "b", "state": "closed", "state_reason": "completed",
            "labels": [{"name": "area/x"}], "assignees": [{"login": "jf"}],
            "user": {"login": "rep"}, "html_url": "https://h/15"
        },
        {
            "number": 16, "node_id": "I_2", "title": "a pr", "state": "open",
            "html_url": "https://h/16", "pull_request": {"url": "u"}
        }
    ]);
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let src = GitHubSource::with_base(&server.uri(), "o/r", None).unwrap();
    let page = src.fetch_changed(&Cursor::default()).await.unwrap();

    assert_eq!(page.tickets.len(), 1, "pull request must be filtered out");
    let t = &page.tickets[0];
    assert_eq!(t.uid, "o/r#15");
    assert_eq!(t.state.category, StateCategory::Done);
    assert_eq!(t.state.resolution, Some(Resolution::Done));
    assert_ticket_invariants(t);
    assert_write_gating(&src, &t.uid).await;
}

#[tokio::test]
async fn gets_a_single_issue() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues/15"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 15, "node_id": "I_1", "title": "t", "state": "open",
            "html_url": "https://h/15"
        })))
        .mount(&server)
        .await;

    let src = GitHubSource::with_base(&server.uri(), "o/r", None).unwrap();
    let t = src.get("o/r#15").await.unwrap();
    assert_eq!(t.display, "#15");
    assert_eq!(t.state.category, StateCategory::Open);
    assert_ticket_invariants(&t);
}
