//! Integration tests for `unkai-jmap` using a local mock JMAP server.
//!
//! These tests spin up a tiny Axum HTTP server that implements the bare
//! minimum of the JMAP protocol — just enough to exercise our client
//! code without touching a real mail server.

use axum::{
    Json, Router,
    extract::{Path, State as AxState},
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
                    "ids": ["email-001", "email-002", "email-pgp", "email-smime-enc", "email-smime-sig", "email-pgp-sig"],
                    "total": 6,
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

                // `fetch_raw_message` only asks for ["id", "blobId"]
                // — that's the JMAP shape for "give me the handle to
                // the raw RFC 5322 bytes so I can `Blob/get` them".
                // Return the blobId for whichever specific id was
                // requested, so the test can drive the encrypted vs
                // plaintext distinction by id.
                let only_blob = properties.contains(&"id")
                    && properties.contains(&"blobId")
                    && properties.len() == 2;

                let requested_ids = _args
                    .get("ids")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                if only_blob {
                    let id = requested_ids
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "email-001".to_string());
                    let blob_id = match id.as_str() {
                        "email-002" => "blob-charlie",
                        "email-pgp" => "blob-pgp",
                        "email-smime-enc" => "blob-smime-enc",
                        "email-smime-sig" => "blob-smime-sig",
                        _ => "blob-alice",
                    };
                    json!([
                        "Email/get",
                        {
                            "accountId": "acc1",
                            "state": "state1",
                            "list": [{
                                "id": id,
                                "blobId": blob_id,
                            }],
                            "notFound": [],
                        },
                        call_id
                    ])
                } else if has_body {
                    // Full message fetch.  The encrypted test seeds
                    // a separate id ("email-pgp") with armored
                    // ciphertext as the text-body part — mirrors
                    // what some servers return for `multipart/encrypted`
                    // under `fetchAllBodyValues=true`.
                    let id = requested_ids
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "email-001".to_string());
                    if id == "email-pgp" {
                        json!([
                            "Email/get",
                            {
                                "accountId": "acc1",
                                "state": "state1",
                                "list": [{
                                    "id": "email-pgp",
                                    "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                    "to": [{ "email": "bob@example.com" }],
                                    "cc": [],
                                    "subject": "Encrypted JMAP message",
                                    "receivedAt": "2025-01-15T10:30:00Z",
                                    "keywords": {},
                                    "hasAttachment": false,
                                    "mailboxIds": { "mbox-inbox": true },
                                    "bodyValues": {
                                        "1": {
                                            "value": "-----BEGIN PGP MESSAGE-----\n\nwV4D...\n-----END PGP MESSAGE-----\n",
                                            "isEncodingProblem": false,
                                            "isTruncated": false,
                                        },
                                    },
                                    "textBody": [{ "partId": "1", "type": "text/plain" }],
                                    "htmlBody": [],
                                    "attachments": [],
                                }],
                                "notFound": [],
                            },
                            call_id
                        ])
                    } else if id == "email-smime-enc" {
                        // S/MIME enveloped-data (#338): the encrypted CMS
                        // blob arrives as a binary `application/pkcs7-mime`
                        // attachment — there's no text body, so the sniff
                        // keys on the part's content type, not body text.
                        json!([
                            "Email/get",
                            {
                                "accountId": "acc1",
                                "state": "state1",
                                "list": [{
                                    "id": "email-smime-enc",
                                    "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                    "to": [{ "email": "bob@example.com" }],
                                    "cc": [],
                                    "subject": "S/MIME encrypted JMAP message",
                                    "receivedAt": "2025-01-12T07:00:00Z",
                                    "keywords": {},
                                    "hasAttachment": true,
                                    "mailboxIds": { "mbox-inbox": true },
                                    "bodyValues": {},
                                    "textBody": [],
                                    "htmlBody": [],
                                    "attachments": [{
                                        "partId": "1",
                                        "blobId": "blob-smime-enc",
                                        "name": "smime.p7m",
                                        "type": "application/pkcs7-mime",
                                        "size": 1234,
                                    }],
                                }],
                                "notFound": [],
                            },
                            call_id
                        ])
                    } else if id == "email-smime-sig" {
                        // S/MIME detached `multipart/signed` (#338): the
                        // server surfaces the clear-signed text as the body
                        // and the `.p7s` signature as a sibling
                        // `application/pkcs7-signature` attachment.
                        json!([
                            "Email/get",
                            {
                                "accountId": "acc1",
                                "state": "state1",
                                "list": [{
                                    "id": "email-smime-sig",
                                    "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                    "to": [{ "email": "bob@example.com" }],
                                    "cc": [],
                                    "subject": "S/MIME signed JMAP message",
                                    "receivedAt": "2025-01-11T06:00:00Z",
                                    "keywords": {},
                                    "hasAttachment": true,
                                    "mailboxIds": { "mbox-inbox": true },
                                    "bodyValues": {
                                        "1": { "value": "This message is signed.\n", "isEncodingProblem": false, "isTruncated": false },
                                    },
                                    "textBody": [{ "partId": "1", "type": "text/plain" }],
                                    "htmlBody": [],
                                    "attachments": [{
                                        "partId": "2",
                                        "blobId": "blob-smime-sig",
                                        "name": "smime.p7s",
                                        "type": "application/pkcs7-signature",
                                        "size": 567,
                                    }],
                                }],
                                "notFound": [],
                            },
                            call_id
                        ])
                    } else if id == "email-pgp-sig" {
                        // PGP detached `multipart/signed` (#57 / RFC 3156):
                        // the server surfaces the clear-signed text as the
                        // body and the armored signature as a sibling
                        // `application/pgp-signature` attachment.  Detected
                        // the same way as the S/MIME signed fixture.
                        json!([
                            "Email/get",
                            {
                                "accountId": "acc1",
                                "state": "state1",
                                "list": [{
                                    "id": "email-pgp-sig",
                                    "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                    "to": [{ "email": "bob@example.com" }],
                                    "cc": [],
                                    "subject": "PGP signed JMAP message",
                                    "receivedAt": "2025-01-10T05:00:00Z",
                                    "keywords": {},
                                    "hasAttachment": true,
                                    "mailboxIds": { "mbox-inbox": true },
                                    "bodyValues": {
                                        "1": { "value": "This message is signed with PGP.\n", "isEncodingProblem": false, "isTruncated": false },
                                    },
                                    "textBody": [{ "partId": "1", "type": "text/plain" }],
                                    "htmlBody": [],
                                    "attachments": [{
                                        "partId": "2",
                                        "blobId": "blob-pgp-sig",
                                        "name": "signature.asc",
                                        "type": "application/pgp-signature",
                                        "size": 489,
                                    }],
                                }],
                                "notFound": [],
                            },
                            call_id
                        ])
                    } else {
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
                    }
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
                                {
                                    "id": "email-pgp",
                                    "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                    "subject": "Encrypted JMAP message",
                                    "receivedAt": "2025-01-13T08:00:00Z",
                                    "keywords": {},
                                    "hasAttachment": false,
                                    "mailboxIds": { "mbox-inbox": true },
                                },
                                {
                                    "id": "email-smime-enc",
                                    "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                    "subject": "S/MIME encrypted JMAP message",
                                    "receivedAt": "2025-01-12T07:00:00Z",
                                    "keywords": {},
                                    "hasAttachment": true,
                                    "mailboxIds": { "mbox-inbox": true },
                                },
                                {
                                    "id": "email-smime-sig",
                                    "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                    "subject": "S/MIME signed JMAP message",
                                    "receivedAt": "2025-01-11T06:00:00Z",
                                    "keywords": {},
                                    "hasAttachment": true,
                                    "mailboxIds": { "mbox-inbox": true },
                                },
                                {
                                    "id": "email-pgp-sig",
                                    "from": [{ "name": "Alice", "email": "alice@example.com" }],
                                    "subject": "PGP signed JMAP message",
                                    "receivedAt": "2025-01-10T05:00:00Z",
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

/// `GET /download/{blobId}` — serve a stored RFC 5322 envelope per
/// blob id.  The encrypted-message test relies on `blob-pgp`
/// returning a well-formed PGP/MIME envelope (so the bridge-aware
/// parser detects it correctly when the test wires this up
/// end-to-end via the Tauri layer); the plaintext blobs are also
/// served so a future round-trip test can verify them.
async fn download_handler(
    Path(blob_id): Path<String>,
    headers: HeaderMap,
) -> Result<Vec<u8>, StatusCode> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !auth.starts_with("Basic ") {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let bytes: &[u8] = match blob_id.as_str() {
        "blob-alice" => b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: Hello from JMAP!\r\n\r\nHi Bob,\r\n\r\nThis is a JMAP test message.\r\n",
        "blob-charlie" => b"From: charlie@example.com\r\nTo: bob@example.com\r\nSubject: JMAP rocks\r\n\r\nHey.\r\n",
        // `multipart/encrypted` envelope shape per RFC 3156 §4 —
        // matches what `detect_pgp_mime_envelope` looks for.  The
        // ciphertext is a placeholder (real OpenPGP decryption is
        // covered in unit tests in the unkai-imap crate).
        "blob-pgp" => b"MIME-Version: 1.0\r\nFrom: alice@example.com\r\nTo: bob@example.com\r\nSubject: Encrypted JMAP message\r\nContent-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"PGP\"\r\n\r\n--PGP\r\nContent-Type: application/pgp-encrypted\r\n\r\nVersion: 1\r\n\r\n--PGP\r\nContent-Type: application/octet-stream\r\n\r\n-----BEGIN PGP MESSAGE-----\r\n\r\nwV4D...\r\n-----END PGP MESSAGE-----\r\n--PGP--\r\n",
        // S/MIME enveloped-data envelope shape (#338) — a bare
        // `application/pkcs7-mime` single part, matching what
        // `detect_smime_envelope` keys on.  The base64 body is a
        // placeholder (real CMS decryption is covered in unkai-imap
        // unit tests); this only proves the Blob/get path is
        // content-agnostic, exactly like the PGP fixture above.
        "blob-smime-enc" => b"MIME-Version: 1.0\r\nFrom: alice@example.com\r\nTo: bob@example.com\r\nSubject: S/MIME encrypted JMAP message\r\nContent-Type: application/pkcs7-mime; smime-type=enveloped-data; name=\"smime.p7m\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7m\"\r\n\r\nMIIBExampleEnvelopedDataPlaceholderBase64==\r\n",
        // S/MIME detached `multipart/signed` envelope shape (#338).
        "blob-smime-sig" => b"MIME-Version: 1.0\r\nFrom: alice@example.com\r\nTo: bob@example.com\r\nSubject: S/MIME signed JMAP message\r\nContent-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; micalg=\"sha-256\"; boundary=\"smime-boundary\"\r\n\r\n--smime-boundary\r\nContent-Type: text/plain\r\n\r\nThis message is signed.\r\n--smime-boundary\r\nContent-Type: application/pkcs7-signature; name=\"smime.p7s\"\r\nContent-Transfer-Encoding: base64\r\n\r\nMIIBExampleSignaturePlaceholderBase64==\r\n--smime-boundary--\r\n",
        _ => return Err(StatusCode::NOT_FOUND),
    };

    Ok(bytes.to_vec())
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
        .route("/download/{blob_id}", get(download_handler))
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

    assert_eq!(envelopes.len(), 6);

    // First email (Alice)
    assert_eq!(envelopes[0].subject, "Hello from JMAP!");
    assert!(envelopes[0].from.contains("alice@example.com"));
    assert!(envelopes[0].is_read); // $seen keyword

    // Second email (Charlie)
    assert_eq!(envelopes[1].subject, "JMAP rocks");
    assert!(!envelopes[1].is_read); // no $seen keyword

    // Third — the encrypted-message fixture; the envelope itself
    // doesn't carry encryption metadata (JMAP needs a body fetch
    // to sniff for the armor headers), so the chip only lights up
    // after `fetch_message` runs.  Verified separately in
    // `test_fetch_message_pgp_stamps_encrypted`.
    assert_eq!(envelopes[2].subject, "Encrypted JMAP message");
    assert!(envelopes[2].protection.is_none());

    // Fourth / fifth — the S/MIME fixtures (#338).  Same as PGP: the
    // envelope carries no encryption metadata; the chip lights up only
    // after `fetch_message` sniffs the body-part content types.  Verified
    // in `test_fetch_message_smime_stamps_encrypted` /
    // `test_fetch_message_smime_signed_stamps_signed`.
    assert_eq!(envelopes[3].subject, "S/MIME encrypted JMAP message");
    assert!(envelopes[3].protection.is_none());
    assert_eq!(envelopes[4].subject, "S/MIME signed JMAP message");
    assert!(envelopes[4].protection.is_none());

    // Sixth — the PGP detached-signed fixture (#57).  Same deal: no chip
    // until `fetch_message` sniffs the `application/pgp-signature` part.
    // Verified in `test_fetch_message_pgp_signed_stamps_signed`.
    assert_eq!(envelopes[5].subject, "PGP signed JMAP message");
    assert!(envelopes[5].protection.is_none());
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
        encryption_mode: None,
        signing_enabled: false,
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

// ── #341 — JMAP Blob/get + decrypt plumbing ────────────────────

/// `fetch_message` on a PGP/MIME-encrypted JMAP message stamps
/// `protection = "encrypted"` (mirrors the IMAP receive path) and
/// nulls the body so MailView shows the Decrypt button instead of
/// the armored ciphertext.  Pre-#341 the same fixture stamped
/// `encrypted-cannot-decrypt`, which the UI rendered as a "switch
/// to IMAP" banner — now JMAP can unlock locally so the banner
/// path is no longer exercised here.
#[tokio::test]
async fn test_fetch_message_pgp_stamps_encrypted() {
    let client = connect_mock().await;

    let envelopes = client.fetch_envelopes("Inbox", 50, None).await.unwrap();
    let pgp_env = envelopes
        .iter()
        .find(|e| e.subject == "Encrypted JMAP message")
        .expect("seeded encrypted envelope missing");

    let email = client
        .fetch_message("Inbox", pgp_env.uid, "test-account")
        .await
        .expect("fetch_message should succeed for the encrypted fixture");

    assert_eq!(email.protection.as_deref(), Some("encrypted"));
    assert_eq!(email.body_text, None);
    assert_eq!(email.body_html, None);
}

/// `fetch_raw_message` round-trips through `Email/get` (for the
/// blob id) and the session `downloadUrl` template, returning the
/// stored RFC 5322 envelope verbatim.  This is the bytes source
/// `decrypt_message` hands to `parse_eml_bytes_with_crypto` on
/// JMAP accounts.
#[tokio::test]
async fn test_fetch_raw_message_returns_blob_bytes() {
    let client = connect_mock().await;

    let envelopes = client.fetch_envelopes("Inbox", 50, None).await.unwrap();
    let pgp_env = envelopes
        .iter()
        .find(|e| e.subject == "Encrypted JMAP message")
        .expect("seeded encrypted envelope missing");

    let raw = client
        .fetch_raw_message("Inbox", pgp_env.uid)
        .await
        .expect("fetch_raw_message should succeed");

    let text = std::str::from_utf8(&raw).expect("blob is utf-8 in this fixture");
    assert!(text.contains("multipart/encrypted"));
    assert!(text.contains("application/pgp-encrypted"));
    assert!(text.contains("-----BEGIN PGP MESSAGE-----"));
}

// ── #338 — S/MIME JMAP receive sniff ───────────────────────────

/// `fetch_message` on an S/MIME `enveloped-data` JMAP message stamps
/// `protection = "encrypted"` and nulls the body so MailView shows the
/// Decrypt button.  Unlike PGP (ASCII armor in the text body), the
/// ciphertext is a binary `application/pkcs7-mime` part, so the sniff
/// keys on the part's content type — JMAP doesn't expose the
/// `smime-type` parameter, so any `pkcs7-mime` part is treated as
/// encrypted and the precise label is re-stamped at decrypt time.
#[tokio::test]
async fn test_fetch_message_smime_stamps_encrypted() {
    let client = connect_mock().await;

    let envelopes = client.fetch_envelopes("Inbox", 50, None).await.unwrap();
    let env = envelopes
        .iter()
        .find(|e| e.subject == "S/MIME encrypted JMAP message")
        .expect("seeded S/MIME enveloped fixture missing");

    let email = client
        .fetch_message("Inbox", env.uid, "test-account")
        .await
        .expect("fetch_message should succeed for the S/MIME enveloped fixture");

    assert_eq!(email.protection.as_deref(), Some("encrypted"));
    assert_eq!(email.body_text, None);
    assert_eq!(email.body_html, None);
}

/// `fetch_message` on an S/MIME detached `multipart/signed` JMAP
/// message stamps `protection = "signed"` but keeps the clear-signed
/// body readable (detection-only — CMS verification is deferred, same
/// as the IMAP receive path).  Detected via the sibling
/// `application/pkcs7-signature` part.
#[tokio::test]
async fn test_fetch_message_smime_signed_stamps_signed() {
    let client = connect_mock().await;

    let envelopes = client.fetch_envelopes("Inbox", 50, None).await.unwrap();
    let env = envelopes
        .iter()
        .find(|e| e.subject == "S/MIME signed JMAP message")
        .expect("seeded S/MIME signed fixture missing");

    let email = client
        .fetch_message("Inbox", env.uid, "test-account")
        .await
        .expect("fetch_message should succeed for the S/MIME signed fixture");

    assert_eq!(email.protection.as_deref(), Some("signed"));
    // Clear-signed body stays readable — only the chip is added.
    assert!(
        email
            .body_text
            .as_deref()
            .unwrap_or_default()
            .contains("This message is signed.")
    );
}

/// `fetch_message` on a PGP detached `multipart/signed` JMAP message
/// stamps `protection = "signed"` and keeps the clear-signed body — the
/// PGP counterpart to `test_fetch_message_smime_signed_stamps_signed`.
/// Closes the parity gap where the JMAP sniff previously recognised PGP
/// encrypted armor but not PGP signed-only mail (IMAP already stamped
/// both).  Detected via the sibling `application/pgp-signature` part.
#[tokio::test]
async fn test_fetch_message_pgp_signed_stamps_signed() {
    let client = connect_mock().await;

    let envelopes = client.fetch_envelopes("Inbox", 50, None).await.unwrap();
    let env = envelopes
        .iter()
        .find(|e| e.subject == "PGP signed JMAP message")
        .expect("seeded PGP signed fixture missing");

    let email = client
        .fetch_message("Inbox", env.uid, "test-account")
        .await
        .expect("fetch_message should succeed for the PGP signed fixture");

    assert_eq!(email.protection.as_deref(), Some("signed"));
    // Clear-signed body stays readable — only the chip is added.
    assert!(
        email
            .body_text
            .as_deref()
            .unwrap_or_default()
            .contains("This message is signed with PGP.")
    );
}

/// `fetch_raw_message` round-trips the S/MIME envelope bytes verbatim
/// through `Email/get` + the session `downloadUrl` — the same
/// protocol-agnostic Blob/get path PGP uses.  This is the bytes source
/// `decrypt_message` feeds to `parse_eml_bytes_with_crypto` on JMAP
/// accounts, where the parameter-aware `detect_smime_envelope` runs.
#[tokio::test]
async fn test_fetch_raw_message_returns_smime_blob_bytes() {
    let client = connect_mock().await;

    let envelopes = client.fetch_envelopes("Inbox", 50, None).await.unwrap();
    let env = envelopes
        .iter()
        .find(|e| e.subject == "S/MIME encrypted JMAP message")
        .expect("seeded S/MIME enveloped fixture missing");

    let raw = client
        .fetch_raw_message("Inbox", env.uid)
        .await
        .expect("fetch_raw_message should succeed");

    let text = std::str::from_utf8(&raw).expect("blob is utf-8 in this fixture");
    assert!(text.contains("application/pkcs7-mime"));
    assert!(text.contains("smime-type=enveloped-data"));
}
