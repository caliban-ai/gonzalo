//! Conformance tests: drive `GitHubSource` against recorded fixtures served by
//! a mock HTTP server, and run the shared `gonzalo_ticket::conformance` checks.

use gonzalo_domain::{Resolution, StateCategory};
use gonzalo_ticket::conformance::{assert_ticket_invariants, assert_write_gating};
use gonzalo_ticket::{Cursor, TicketSource};
use gonzalo_ticket_github::GitHubSource;
use wiremock::matchers::{method, path, query_param};
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
async fn follows_link_header_pagination_across_pages() {
    let server = MockServer::start().await;

    // Page 1 carries a `rel="next"` pointing at page 2 of the same server.
    let next = format!(
        "<{base}/repos/o/r/issues?page=2>; rel=\"next\", \
         <{base}/repos/o/r/issues?page=2>; rel=\"last\"",
        base = server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "number": 2, "node_id": "I_2", "title": "two", "state": "open",
              "html_url": "https://h/2" }
        ])))
        .expect(1)
        .mount(&server)
        .await;
    // The initial request (no `page` query) returns page 1 plus the Link header.
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", next.as_str())
                .set_body_json(serde_json::json!([
                    { "number": 1, "node_id": "I_1", "title": "one", "state": "open",
                      "html_url": "https://h/1" }
                ])),
        )
        .mount(&server)
        .await;

    let src = GitHubSource::with_base(&server.uri(), "o/r", None).unwrap();

    // Page 1: one ticket, and a non-terminating cursor pointing at page 2.
    let p1 = src.fetch_changed(&Cursor::default()).await.unwrap();
    assert_eq!(p1.tickets.len(), 1);
    assert_eq!(p1.tickets[0].uid, "o/r#1");
    assert!(p1.next.0.is_some(), "page 1 must carry a next cursor");

    // Page 2: fetched via the cursor from page 1; no further next -> terminates.
    let p2 = src.fetch_changed(&p1.next).await.unwrap();
    assert_eq!(p2.tickets.len(), 1);
    assert_eq!(p2.tickets[0].uid, "o/r#2");
    assert_eq!(p2.next, Cursor::default(), "last page must terminate");
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

#[tokio::test]
async fn writes_state_and_comment() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/repos/o/r/issues/15"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/15/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let src = GitHubSource::with_base(&server.uri(), "o/r", Some("tok".into())).unwrap();
    src.set_state("o/r#15", StateCategory::Done).await.unwrap();
    src.comment("o/r#15", "hi").await.unwrap();
}
