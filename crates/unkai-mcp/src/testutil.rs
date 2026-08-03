//! Shared fixtures for the tool modules' unit tests (#441).

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::model::CallToolResult;
use serde_json::Value;
use tokio::sync::RwLock;
use unkai_core::models::{Account, AppSettings};
use unkai_store::Cache;

use crate::registry::{ToolContext, ToolRegistry};

pub(crate) fn test_context() -> ToolContext {
    ToolContext {
        cache: Cache::open_in_memory().expect("in-memory cache"),
        settings: Arc::new(RwLock::new(AppSettings::default())),
    }
}

pub(crate) fn mail_account(id: &str) -> Account {
    Account {
        id: id.into(),
        display_name: "Alex Morgan".into(),
        email: "alex@example.com".into(),
        imap_host: "imap.example.com".into(),
        imap_port: 993,
        smtp_host: "smtp.example.com".into(),
        smtp_port: 465,
        use_jmap: false,
        jmap_url: None,
        signature: None,
        folder_icons: Vec::new(),
        folder_icon_overrides: HashMap::new(),
        trusted_certs: Vec::new(),
        emoji: None,
        sort_order: 0,
        person_name: None,
        pgp_key_fingerprint: None,
        smime_cert_fingerprint: None,
    }
}

/// Invoke a registered tool directly, bypassing the HTTP layer —
/// enablement/vault gates live in `call_tool` and are tested
/// separately; these tests exercise the handlers.
pub(crate) async fn invoke(
    ctx: &ToolContext,
    tool: &str,
    args: Value,
) -> Result<CallToolResult, ErrorData> {
    let registry = ToolRegistry::builtin();
    let tool = registry.get(tool).expect("tool registered");
    let args = args.as_object().cloned().expect("args are an object");
    tool.invoke(ctx.clone(), Some(args)).await
}

pub(crate) fn result_payload(result: &CallToolResult) -> Value {
    let text = result.content[0].as_text().expect("text content");
    serde_json::from_str(&text.text).expect("payload is valid JSON")
}
