//! Integration tests for the MCP server foundation (#438).
//!
//! Each test boots a real listener on an ephemeral port
//! (`mcp_port = 0`) through the same [`McpServer::reconcile`]
//! path the app uses, then talks plain HTTP to it — so auth,
//! Host/Origin validation, locked-vault behaviour, and the
//! enable/disable lifecycle are exercised end-to-end rather than
//! by poking internals.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;
use unkai_core::models::{AppSettings, Email};
use unkai_mcp::{McpServer, SharedSettings};
use unkai_store::Cache;

/// Fixed token injected instead of a keychain read — see
/// `McpServer::new`.
const TOKEN: &str = "integration-test-token-not-a-real-secret";

/// JSON-RPC `initialize` request, the first message of every MCP
/// session.
const INITIALIZE_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"unkai-test","version":"0.0.0"}}}"#;

async fn start_server(cache: Cache) -> (McpServer, SharedSettings, String) {
    let settings = AppSettings {
        mcp_enabled: true,
        // Ephemeral port so parallel tests never collide.
        mcp_port: 0,
        ..Default::default()
    };
    let shared: SharedSettings = Arc::new(RwLock::new(settings));
    let server = McpServer::new(cache, shared.clone(), Some(TOKEN.to_string()));
    server.reconcile().await;
    let status = server.status().await;
    assert!(status.running, "server should be running: {status:?}");
    let endpoint = status.endpoint.expect("running server has endpoint");
    (server, shared, endpoint)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client")
}

/// POST an MCP message.  `token`/`origin` let individual tests
/// break one gate at a time.
async fn post_mcp(
    endpoint: &str,
    token: Option<&str>,
    origin: Option<&str>,
    session_id: Option<&str>,
    body: &'static str,
) -> reqwest::Response {
    let mut request = client()
        .post(endpoint)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body(body);
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(origin) = origin {
        request = request.header("Origin", origin);
    }
    if let Some(session_id) = session_id {
        request = request
            .header("Mcp-Session-Id", session_id)
            .header("MCP-Protocol-Version", "2025-06-18");
    }
    request.send().await.expect("request should complete")
}

// ── Auth rejection ─────────────────────────────────────────────

#[tokio::test]
async fn missing_token_is_401() {
    let (_server, _settings, endpoint) =
        start_server(Cache::open_in_memory().expect("cache")).await;
    let response = post_mcp(&endpoint, None, None, None, INITIALIZE_BODY).await;
    assert_eq!(response.status(), 401);
    assert!(response.headers().contains_key("www-authenticate"));
}

#[tokio::test]
async fn wrong_token_is_401() {
    let (_server, _settings, endpoint) =
        start_server(Cache::open_in_memory().expect("cache")).await;
    let response = post_mcp(&endpoint, Some("wrong-token"), None, None, INITIALIZE_BODY).await;
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn revoked_token_is_401() {
    let (server, _settings, endpoint) = start_server(Cache::open_in_memory().expect("cache")).await;
    server.set_token(None).await;
    let response = post_mcp(&endpoint, Some(TOKEN), None, None, INITIALIZE_BODY).await;
    assert_eq!(response.status(), 401);
}

// ── Host / Origin validation ───────────────────────────────────

#[tokio::test]
async fn browser_origin_is_403() {
    let (_server, _settings, endpoint) =
        start_server(Cache::open_in_memory().expect("cache")).await;
    let response = post_mcp(
        &endpoint,
        Some(TOKEN),
        Some("http://evil.example.com"),
        None,
        INITIALIZE_BODY,
    )
    .await;
    assert_eq!(response.status(), 403);
    assert!(response.text().await.unwrap().contains("origin_forbidden"));
}

/// DNS-rebinding shape: the TCP connection genuinely reaches
/// 127.0.0.1, but the `Host` header names an attacker domain.
/// Sent over a raw socket because HTTP clients (rightly) refuse
/// to forge `Host`.
#[tokio::test]
async fn non_loopback_host_is_403() {
    let (_server, _settings, endpoint) =
        start_server(Cache::open_in_memory().expect("cache")).await;
    let authority = endpoint
        .strip_prefix("http://")
        .and_then(|rest| rest.strip_suffix("/mcp"))
        .expect("endpoint shape");

    let mut stream = tokio::net::TcpStream::connect(authority)
        .await
        .expect("connect");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: evil.example.com\r\nAuthorization: Bearer {TOKEN}\r\n\
         Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{INITIALIZE_BODY}",
        INITIALIZE_BODY.len()
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    assert!(
        response.starts_with("HTTP/1.1 403"),
        "expected 403, got: {}",
        response.lines().next().unwrap_or_default()
    );
    assert!(response.contains("invalid_host"));
}

// ── Locked vault ───────────────────────────────────────────────

#[tokio::test]
async fn locked_vault_is_503_after_auth() {
    let (_server, _settings, endpoint) = start_server(Cache::locked_for_tests()).await;

    // Authenticated request → clear vault-locked error.
    let response = post_mcp(&endpoint, Some(TOKEN), None, None, INITIALIZE_BODY).await;
    assert_eq!(response.status(), 503);
    assert!(response.text().await.unwrap().contains("vault_locked"));

    // Unauthenticated request → 401, NOT 503: lock state must not
    // be observable without the token.
    let response = post_mcp(&endpoint, None, None, None, INITIALIZE_BODY).await;
    assert_eq!(response.status(), 401);
}

// ── Enable / disable lifecycle ─────────────────────────────────

#[tokio::test]
async fn disable_stops_listener_and_reenable_starts_it() {
    let (server, settings, endpoint) = start_server(Cache::open_in_memory().expect("cache")).await;

    // Running: a request gets *an* HTTP answer (here 200).
    let response = post_mcp(&endpoint, Some(TOKEN), None, None, INITIALIZE_BODY).await;
    assert_eq!(response.status(), 200);

    // Flip the toggle off and reconcile — connection refused.
    settings.write().await.mcp_enabled = false;
    server.reconcile().await;
    assert!(!server.status().await.running);
    assert!(
        client()
            .post(&endpoint)
            .header("Authorization", format!("Bearer {TOKEN}"))
            .body(INITIALIZE_BODY)
            .send()
            .await
            .is_err(),
        "listener should be gone after disable"
    );

    // Back on — a fresh listener comes up (new ephemeral port).
    settings.write().await.mcp_enabled = true;
    server.reconcile().await;
    let status = server.status().await;
    assert!(status.running);
    let response = post_mcp(
        &status.endpoint.expect("endpoint"),
        Some(TOKEN),
        None,
        None,
        INITIALIZE_BODY,
    )
    .await;
    assert_eq!(response.status(), 200);
}

// ── MCP acceptance: initialize + tools/list + enablement ───────

/// Drive a real MCP handshake and check `ping` is advertised,
/// then flip its enablement off and check it disappears from
/// `tools/list` *and* is refused by `tools/call`.
#[tokio::test]
async fn initialize_list_and_enablement() {
    let (_server, settings, endpoint) = start_server(Cache::open_in_memory().expect("cache")).await;

    // initialize → serverInfo names us; session id in the header.
    let response = post_mcp(&endpoint, Some(TOKEN), None, None, INITIALIZE_BODY).await;
    assert_eq!(response.status(), 200);
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .expect("session id header")
        .to_str()
        .expect("ascii session id")
        .to_string();
    assert!(response.text().await.unwrap().contains("unkai-mail"));

    // The spec-mandated initialized notification (202 Accepted).
    let response = post_mcp(
        &endpoint,
        Some(TOKEN),
        None,
        Some(&session_id),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert_eq!(response.status(), 202);

    // tools/list advertises ping (a read tool, default-enabled).
    const LIST_BODY: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    let response = post_mcp(&endpoint, Some(TOKEN), None, Some(&session_id), LIST_BODY).await;
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("\"ping\""));

    // tools/call ping works.
    const CALL_BODY: &str =
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ping"}}"#;
    let response = post_mcp(&endpoint, Some(TOKEN), None, Some(&session_id), CALL_BODY).await;
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("ok"));

    // Disable ping live — no restart, next list omits it…
    settings
        .write()
        .await
        .mcp_tool_enablement
        .insert("ping".into(), false);
    const LIST_BODY_2: &str = r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#;
    let response = post_mcp(&endpoint, Some(TOKEN), None, Some(&session_id), LIST_BODY_2).await;
    assert_eq!(response.status(), 200);
    assert!(!response.text().await.unwrap().contains("\"ping\""));

    // …and calling it anyway is refused server-side.
    const CALL_BODY_2: &str =
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"ping"}}"#;
    let response = post_mcp(&endpoint, Some(TOKEN), None, Some(&session_id), CALL_BODY_2).await;
    let body = response.text().await.unwrap();
    assert!(
        body.contains("disabled"),
        "disabled tool call should be refused: {body}"
    );
}

// ── Mail tools end-to-end (#440) ───────────────────────────────

/// Test double for a cached message; mirrors the fixture the
/// unit tests use, but here it flows through the full HTTP +
/// JSON-RPC + registry stack.
fn cached_message(uid: u32, subject: &str, body: &str, protection: Option<&str>) -> Email {
    Email {
        id: format!("INBOX:{uid}"),
        account_id: "acc".into(),
        folder: "INBOX".into(),
        from: "Alex Morgan <alex@example.com>".into(),
        to: vec!["you@example.com".into()],
        cc: vec![],
        subject: subject.into(),
        body_text: Some(body.into()),
        body_html: None,
        date: chrono::Utc::now(),
        is_read: false,
        is_starred: false,
        has_attachments: false,
        attachments: vec![],
        message_id: None,
        in_reply_to: None,
        references_ids: Vec::new(),
        protection: protection.map(str::to_string),
        signature_status: None,
        signer_fingerprint: None,
        is_pinned: false,
        priority: None,
        priority_override: None,
        reminder_at: None,
        mdn_requested_to: None,
        mdn_handled: None,
    }
}

/// Full acceptance path for the #440 mail tools: the read tools
/// are advertised (create_draft isn't, being a default-off write
/// tool), an operator search over seeded mail comes back with the
/// encrypted hit's snippet redacted, and enabling create_draft
/// makes its handler reachable.
#[tokio::test]
async fn mail_tools_search_and_redaction_end_to_end() {
    let cache = Cache::open_in_memory().expect("cache");
    cache
        .upsert_message(&cached_message(
            1,
            "Alpha secret",
            "the secret rendezvous point",
            Some("encrypted"),
        ))
        .expect("seed encrypted");
    cache
        .upsert_message(&cached_message(
            2,
            "Alpha public",
            "the public agenda",
            None,
        ))
        .expect("seed plain");

    let (_server, settings, endpoint) = start_server(cache).await;

    let response = post_mcp(&endpoint, Some(TOKEN), None, None, INITIALIZE_BODY).await;
    assert_eq!(response.status(), 200);
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .expect("session id header")
        .to_str()
        .expect("ascii session id")
        .to_string();
    let response = post_mcp(
        &endpoint,
        Some(TOKEN),
        None,
        Some(&session_id),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert_eq!(response.status(), 202);

    // Advertisement: reads on, the write tool off by default.
    const LIST_BODY: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    let response = post_mcp(&endpoint, Some(TOKEN), None, Some(&session_id), LIST_BODY).await;
    let body = response.text().await.unwrap();
    // Match the advertised tool *names* — descriptions mention
    // sibling tools, so a bare substring check would false-hit.
    for tool in [
        "search_mail",
        "get_message",
        "get_thread",
        "list_folders",
        "list_accounts",
    ] {
        assert!(
            body.contains(&format!("\"name\":\"{tool}\"")),
            "tools/list should advertise {tool}: {body}"
        );
    }
    assert!(
        !body.contains("\"name\":\"create_draft\""),
        "write tool must be off by default"
    );

    // Operator search over the seeded cache — the encrypted hit's
    // snippet is withheld, the plain one isn't.
    const SEARCH_BODY: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_mail","arguments":{"query":"subject:alpha in:INBOX"}}}"#;
    let response = post_mcp(&endpoint, Some(TOKEN), None, Some(&session_id), SEARCH_BODY).await;
    let body = response.text().await.unwrap();
    assert!(
        body.contains("encrypted content withheld"),
        "encrypted snippet should be redacted: {body}"
    );
    assert!(
        body.contains("Alpha public"),
        "plain hit should be present: {body}"
    );
    assert!(
        !body.contains("rendezvous"),
        "encrypted body text must not leak: {body}"
    );

    // The write tool stays refused until the user opts in…
    const DRAFT_BODY: &str = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"create_draft","arguments":{"account_id":"acc","to":["r@example.com"]}}}"#;
    let response = post_mcp(&endpoint, Some(TOKEN), None, Some(&session_id), DRAFT_BODY).await;
    let body = response.text().await.unwrap();
    assert!(
        body.contains("disabled"),
        "create_draft should be refused while off: {body}"
    );

    // …and once enabled, the handler actually runs (it rejects the
    // unknown account — this cache has no account store entry —
    // which proves the call got past enablement into the tool).
    settings
        .write()
        .await
        .mcp_tool_enablement
        .insert("create_draft".into(), true);
    const DRAFT_BODY_2: &str = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"create_draft","arguments":{"account_id":"acc","to":["r@example.com"]}}}"#;
    let response = post_mcp(
        &endpoint,
        Some(TOKEN),
        None,
        Some(&session_id),
        DRAFT_BODY_2,
    )
    .await;
    let body = response.text().await.unwrap();
    assert!(
        body.contains("unknown account_id"),
        "enabled create_draft should reach its handler: {body}"
    );
}
