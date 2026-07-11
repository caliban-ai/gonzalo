//! Conformance tests for `GitLabSource`: the scoped-label policy variant and
//! header-driven cursor advance, against a recorded fixture.

use gonzalo_domain::StateCategory;
use gonzalo_ticket::conformance::{assert_ticket_invariants, assert_write_gating};
use gonzalo_ticket::{Cursor, SourceError, StateMapping, StateSignal, TicketSource};
use gonzalo_ticket_gitlab::GitLabSource;
use std::collections::BTreeMap;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn workflow_mapping() -> StateMapping {
    let mut by_value = BTreeMap::new();
    by_value.insert("in review".to_string(), StateCategory::InProgress);
    StateMapping {
        signal: StateSignal::ScopedLabel {
            prefix: "workflow::".into(),
        },
        by_value,
        default: StateCategory::Open,
    }
}

#[tokio::test]
async fn scoped_label_policy_and_cursor_advance() {
    let server = MockServer::start().await;
    let body = serde_json::json!([{
        "iid": 7, "title": "t", "state": "opened",
        "labels": ["workflow::in review"],
        "references": {"full": "g/p#7"}
    }]);
    // path_regex avoids depending on whether %2F is decoded by the test server.
    Mock::given(method("GET"))
        .and(path_regex(r"/issues$"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-next-page", "2")
                .set_body_json(body),
        )
        .mount(&server)
        .await;

    let src = GitLabSource::with_base(&server.uri(), "g/p", "tok")
        .unwrap()
        .with_mapping(workflow_mapping());
    let page = src.fetch_changed(&Cursor::default()).await.unwrap();

    assert_eq!(page.tickets.len(), 1);
    assert_eq!(page.tickets[0].state.category, StateCategory::InProgress);
    assert_eq!(page.tickets[0].state.raw_name, "workflow::in review");
    assert_eq!(
        page.next.0.as_deref(),
        Some("2"),
        "cursor advances from x-next-page"
    );
    assert_ticket_invariants(&page.tickets[0]);
    assert_write_gating(&src, "g/p#7").await;
}

#[tokio::test]
async fn writes_state_and_note() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex(r"/issues/7$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"/issues/7/notes$"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let src = GitLabSource::with_base(&server.uri(), "g/p", "tok").unwrap();
    src.set_state("g/p#7", StateCategory::Done).await.unwrap();
    src.comment("g/p#7", "hi").await.unwrap();
}

#[tokio::test]
async fn non_terminal_set_state_under_scoped_label_is_unsupported() {
    // #143: with a scoped-label mapping in effect, a non-terminal move can't be
    // expressed via the intrinsic close/reopen event, so it must error rather
    // than report a false success. No HTTP request should be made.
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex(r"/issues/7$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&server)
        .await;
    let src = GitLabSource::with_base(&server.uri(), "g/p", "tok")
        .unwrap()
        .with_mapping(workflow_mapping());

    let err = src
        .set_state("g/p#7", StateCategory::InProgress)
        .await
        .expect_err("non-terminal move under a scoped-label mapping must error");
    assert!(matches!(err, SourceError::Unsupported(_)), "got {err:?}");
}

#[tokio::test]
async fn terminal_set_state_still_works_under_scoped_label() {
    // #143: terminal moves remain expressible via the intrinsic close event even
    // when a scoped-label mapping is configured.
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path_regex(r"/issues/7$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let src = GitLabSource::with_base(&server.uri(), "g/p", "tok")
        .unwrap()
        .with_mapping(workflow_mapping());
    src.set_state("g/p#7", StateCategory::Done).await.unwrap();
}
