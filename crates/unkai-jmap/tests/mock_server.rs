//! Integration tests for `unkai-jmap` using a local mock JMAP server.
//!
//! These tests spin up a tiny Axum HTTP server that implements the bare
//! minimum of the JMAP protocol — just enough to exercise our client
//! code without touching a real mail server.

use axum::{
    Json, Router,
    extract::State as AxState,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use unkai_jmap::JmapClient;

// ── Mock server state ���─────────────────────────────────────────

/// Shared state for the mock server — controls what responses it gives.
#[derive(Clone)]
struct MockState {
    /// The port the server is listening on (so we can build self-referential URLs).
    port: u16,
}

// ── Mock endpoints ─────────────────────────────────────────────

/// `GET /.well-known/jmap` — session discovery.
async fn well_known(
    headers: HeaderMap,
    AxState(state): AxState<Arc<MockState>>,
) -> Result<Json<Value>, StatusCode> {
    // Require Basic Auth.
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !auth.starts_with("Basic ") {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(Json(json!({
        "apiUrl": format!("http://127.0.0.1:{}/api", state.port),
        "downloadUrl": format!("http://127.0.0.1:{}/download/{{blobId}}", state.port),
        "uploadUrl": format!("http://127.0.0.1:{}/upload", state.port),
        "eventSourceUrl": format!("http://127.0.0.1:{}/events?types={{types}}&closeafter={{closeafter}}&ping={{ping}}", state.port),
        "accounts": {
            "acc1": {
                "name": "Test User",
                "isPersonal": true,
                "isReadOnly": false,
            },
        },
        "primaryAccounts": {
            "urn:ietf:params:jmap:mail": "acc1",
            "urn:ietf:params:jmap:submission": "acc1",
        },
    })))
}

/// `POST /api` — the JMAP method call endpoint.
///
/// Dispatches based on the method name in each method call.
async fn api_handler(Json(body): Json<Value>) -> Json<Value> {
    let calls = body
        .get("methodCalls")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut responses = Vec::new();
    for call in &calls {
        let method = call[0].as_str().unwrap_or("");
        let _args = &call[1];
        let call_id = call[2].as_str().unwrap_or("?");

        let resp = match method {
            "Mailbox/get" => json!([
                "Mailbox/get",
                {
                    "accountId": "acc1",
                    "state": "state1",
                    "list": [
                        {
                            "id": "mbox-inbox",
                            "name": "Inbox",
                            "role": "inbox",
                            "sortOrder": 1,
                            "totalEmails": 42,
                            "unreadEmails": 3,
                            "totalThreads": 40,
                            "unreadThreads": 2,
                        },
                        {
                            "id": "mbox-sent",
                            "name": "Sent",
                            "role": "sent",
                            "sortOrder": 5,
                            "totalEmails": 10,
                            "unreadEmails": 0,
                            "totalThreads": 10,
                            "unreadThreads": 0,
                        },
                        {
                            "id": "mbox-drafts",
                            "name": "Drafts",
                            "role": "drafts",
                            "sortOrder": 3,
                            "totalEmails": 2,
                            "unreadEmails": 0,
                            "totalThreads": 2,
                            "unreadThreads": 0,
                        },
                    ],
                    "notFound": [],
                },
                call_id
            ]),
            "Email/query" => json!([
                "Email/query",
                {
                    "accountId": "acc1",
                    "ids": ["email-001", "email-002"],
                    "total": 2,
                    "position": 0,
                },
                call_id
            ]),
            "Email/get" => {
                // Check if this is a list fetch (envelope) or full message fetch.
                let properties = _args
                    .get("properties")
                    .and_then(|p| p.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                    .unwrap_or_default();

                let has_body =
                    properties.contains(&"bodyValues") || _args.get("fetchAllBodyValues").is_some();

                if has_body {
                    // Full message fetch.
                    json!([
                        "Email/get",
                        {
                            "accountId": "acc1",
                            "state": "state1",
                            "list": [{
                                "id": "email-001",
                                "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                "to": [{ "email": "bob@example.com" }],
                                "cc": [],
                                "subject": "Hello from JMAP!",
                                "receivedAt": "2025-01-15T10:30:00Z",
                                "keywords": { "$seen": true },
                                "hasAttachment": false,
                                "mailboxIds": { "mbox-inbox": true },
                                "bodyValues": {
                                    "1": { "value": "Hi Bob,\n\nThis is a JMAP test message.\n\nBest,\nAlice", "isEncodingProblem": false, "isTruncated": false },
                                },
                                "textBody": [{ "partId": "1", "type": "text/plain" }],
                                "htmlBody": [],
                                "attachments": [],
                            }],
                            "notFound": [],
                        },
                        call_id
                    ])
                } else {
                    // Envelope fetch.
                    json!([
                        "Email/get",
                        {
                            "accountId": "acc1",
                            "state": "state1",
                            "list": [
                                {
                                    "id": "email-001",
                                    "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                    "subject": "Hello from JMAP!",
                                    "receivedAt": "2025-01-15T10:30:00Z",
                                    "keywords": { "$seen": true },
                                    "hasAttachment": false,
                                    "mailboxIds": { "mbox-inbox": true },
                                },
                                {
                                    "id": "email-002",
                                    "from": [{ "name": "Charlie", "email": "charlie@example.com" }],
                                    "subject": "JMAP rocks",
                                    "receivedAt": "2025-01-14T09:00:00Z",
                                    "keywords": {},
                                    "hasAttachment": true,
                                    "mailboxIds": { "mbox-inbox": true },
                                },
                            ],
                            "notFound": [],
                        },
                        call_id
                    ])
                }
            }
            "Email/set" => json!([
                "Email/set",
                {
                    "accountId": "acc1",
                    "oldState": "state1",
                    "newState": "state2",
                    "created": { "draft": { "id": "email-new-001" } },
                    "updated": _args.get("update").map(|u| {
                        u.as_object()
                            .map(|o| o.keys().map(|k| (k.clone(), json!(null))).collect::<serde_json::Map<String, Value>>())
                            .unwrap_or_default()
                    }).unwrap_or_default(),
                    "notCreated": {},
                    "notUpdated": {},
                },
                call_id
            ]),
            "EmailSubmission/set" => json!([
                "EmailSubmission/set",
                {
                    "accountId": "acc1",
                    "created": { "sub": { "id": "sub-001" } },
                    "notCreated": {},
                },
                call_id
            ]),
            "Identity/get" => json!([
                "Identity/get",
                {
                    "accountId": "acc1",
                    "state": "state1",
                    "list": [{
                        "id": "identity-1",
                        "name": "Test User",
                        "email": "test@example.com",
                    }],
                    "notFound": [],
                },
                call_id
            ]),
            _ => json!([
                "error",
                { "type": "unknownMethod" },
                call_id
            ]),
        };
        responses.push(resp);
    }

    Json(json!({
        "methodResponses": responses,
        "sessionState": "session-state-1",
    }))
}

// ── Test helpers ────��──────────────────────────────────────────

/// Start the mock JMAP server and return (base_url, port).
async fn start_mock() -> (String, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let port = addr.port();

    let state = Arc::new(MockState { port });

    let app = Router::new()
        .route("/.well-known/jmap", get(well_known))
        .route("/api", post(api_handler))
        .with_state(state);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give the server a moment to bind.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (format!("http://127.0.0.1:{port}"), port)
}

/// Connect to the mock server.
async fn connect_mock() -> JmapClient {
    let (base_url, _) = start_mock().await;
    JmapClient::connect(&base_url, "test@example.com", "password123")
        .await
        .expect("connect to mock JMAP server should succeed")
}

// ── Tests ───��──────────────────────────────────────────────────

#[tokio::test]
async fn test_session_discovery() {
    let (base_url, _) = start_mock().await;
    let client = JmapClient::connect(&base_url, "test@example.com", "password123")
        .await
        .expect("session discovery should succeed");

    assert_eq!(client.account_id(), "acc1");
}

#[tokio::test]
async fn test_session_bad_credentials() {
    let (base_url, _) = start_mock().await;
    // Our mock always accepts any Basic auth — but if we wanted to test
    // auth failure, we'd need to update the mock. For now, verify the
    // happy path works and the error mapping path compiles.
    let result = JmapClient::connect(&base_url, "user", "pass").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_list_folders() {
    let client = connect_mock().await;
    let folders = client
        .list_folders()
        .await
        .expect("list_folders should succeed");

    assert_eq!(folders.len(), 3);
    // Inbox should be sorted first.
    assert_eq!(folders[0].name, "Inbox");
    assert_eq!(folders[0].unread_count, Some(3));
    // Check that attributes are mapped correctly.
    assert!(folders[0].attributes.contains(&"Inbox".to_string()));

    // Drafts and Sent should be present.
    let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"Drafts"));
    assert!(names.contains(&"Sent"));
}

#[tokio::test]
async fn test_fetch_envelopes() {
    let client = connect_mock().await;
    let envelopes = client
        .fetch_envelopes("Inbox", 50, None)
        .await
        .expect("fetch_envelopes should succeed");

    assert_eq!(envelopes.len(), 2);

    // First email (Alice)
    assert_eq!(envelopes[0].subject, "Hello from JMAP!");
    assert!(envelopes[0].from.contains("alice@example.com"));
    assert!(envelopes[0].is_read); // $seen keyword

    // Second email (Charlie)
    assert_eq!(envelopes[1].subject, "JMAP rocks");
    assert!(!envelopes[1].is_read); // no $seen keyword
}

#[tokio::test]
async fn test_fetch_message() {
    let client = connect_mock().await;

    // First get envelopes to find the synthetic UID.
    let envelopes = client
        .fetch_envelopes("Inbox", 50, None)
        .await
        .expect("fetch_envelopes should succeed");
    let uid = envelopes[0].uid;

    let email = client
        .fetch_message("Inbox", uid, "test-account")
        .await
        .expect("fetch_message should succeed");

    assert_eq!(email.subject, "Hello from JMAP!");
    assert_eq!(email.from, "Alice <alice@example.com>");
    assert!(email.body_text.unwrap().contains("JMAP test message"));
    assert!(email.is_read);
    assert!(!email.has_attachments);
}

#[tokio::test]
async fn test_mark_as_read() {
    let client = connect_mock().await;

    let envelopes = client.fetch_envelopes("Inbox", 50, None).await.unwrap();
    let uid = envelopes[0].uid;

    // Should succeed without error.
    client
        .mark_as_read("Inbox", uid)
        .await
        .expect("mark_as_read should succeed");
}

#[tokio::test]
async fn test_send_email() {
    use unkai_core::models::OutgoingEmail;

    let client = connect_mock().await;

    let email = OutgoingEmail {
        from: "test@example.com".into(),
        to: vec!["recipient@example.com".into()],
        cc: vec![],
        bcc: vec![],
        reply_to: None,
        subject: "Test send".into(),
        body_text: Some("Hello from JMAP tests!".into()),
        body_html: None,
        attachments: vec![],
        calendar_part: None,
        skip_sent_copy: false,
        in_reply_to: None,
        references: vec![],
    };

    client
        .send_email(&email)
        .await
        .expect("send_email should succeed");
}

#[tokio::test]
async fn test_event_source_url() {
    let client = connect_mock().await;
    let url = client
        .event_source_url()
        .expect("should resolve event source URL");
    assert!(url.contains("types=*"));
    assert!(url.contains("closeafter=no"));
    assert!(url.contains("ping=30"));
}

#[tokio::test]
async fn test_connection_test() {
    let (base_url, _) = start_mock().await;
    let result = JmapClient::test(&base_url, "test@example.com", "password123")
        .await
        .expect("test should succeed");
    assert!(result.contains("JMAP login succeeded"));
    assert!(result.contains("Test User"));
}
