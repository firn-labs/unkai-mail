//! Tool registry scaffolding (#438).
//!
//! Every MCP tool the app exposes is declared here with a **stable
//! id**, a category, and a read/write classification.  The
//! follow-up issues (#440 mail tools, #441 groupware tools) add
//! entries to [`ToolRegistry::builtin`]; the transport, auth, and
//! enablement plumbing never needs to change for that.
//!
//! Enablement is a two-layer decision:
//!
//! - The per-tool map in `AppSettings::mcp_tool_enablement`
//!   (keyed by tool id) is the user's explicit choice.
//! - A missing key falls back to the tool's class default:
//!   **read** tools default on, **write** tools default off.
//!
//! [`is_enabled`] is consulted both when advertising tools
//! (`tools/list`) *and* again inside `tools/call` — a client that
//! remembers a tool from before the user disabled it still can't
//! call it.

use std::sync::Arc;

use futures::future::BoxFuture;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use unkai_core::models::AppSettings;
use unkai_store::Cache;

/// Read/write classification for a tool.  Drives the enablement
/// default (reads on, writes off) and gives the settings UI a
/// grouping axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAccess {
    /// Only reads cached data; never mutates local or server state.
    Read,
    /// Mutates something (drafts, events, …).  Always an explicit
    /// per-tool opt-in.
    Write,
}

/// Static metadata for one tool.  `id` is the wire name MCP
/// clients call and the key in the enablement map — treat it as a
/// public API and never rename an existing one.
#[derive(Debug, Clone, Copy)]
pub struct ToolDescriptor {
    pub id: &'static str,
    /// Coarse grouping for the settings UI (`"server"`, later
    /// `"mail"`, `"contacts"`, `"calendar"`, `"talk"`).
    pub category: &'static str,
    pub access: ToolAccess,
    pub description: &'static str,
}

/// Everything a tool handler gets to work with.  Cheap to clone —
/// `Cache` is an `Arc` around the pool.  Follow-up issues extend
/// this (e.g. account list access) rather than re-plumbing
/// handler signatures.
#[derive(Clone)]
pub struct ToolContext {
    pub cache: Cache,
    /// Live app settings (#440).  Handlers read policy flags —
    /// today `mcp_expose_decrypted_content` — per call, so a
    /// settings flip applies to in-flight sessions immediately
    /// instead of waiting for a server restart.
    pub settings: crate::SharedSettings,
}

/// Type-erased async tool handler: `(context, arguments) →
/// CallToolResult`.  Boxed so the registry can hold tools with
/// different bodies in one `Vec`.
pub type ToolHandlerFn = Arc<
    dyn Fn(ToolContext, Option<JsonObject>) -> BoxFuture<'static, Result<CallToolResult, ErrorData>>
        + Send
        + Sync,
>;

/// A descriptor bundled with its executable handler and its JSON
/// input schema.
pub struct RegisteredTool {
    pub descriptor: ToolDescriptor,
    input_schema: Arc<JsonObject>,
    handler: ToolHandlerFn,
}

impl RegisteredTool {
    /// The rmcp `Tool` advertisement for `tools/list`.
    pub fn to_tool(&self) -> Tool {
        Tool::new(
            self.descriptor.id,
            self.descriptor.description,
            self.input_schema.clone(),
        )
    }

    /// Run the tool.  Enablement and vault-lock checks happen in
    /// the caller (`call_tool`) so they can't be forgotten by an
    /// individual handler.
    pub async fn invoke(
        &self,
        ctx: ToolContext,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ErrorData> {
        (self.handler)(ctx, arguments).await
    }
}

/// The set of tools this build knows about, in a stable order.
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
}

impl ToolRegistry {
    /// All built-in tools: the `ping` health check (#438) plus the
    /// mail tool set (#440).  The groupware tools (#441) add their
    /// entries here too.
    pub fn builtin() -> Self {
        let mut registry = Self { tools: Vec::new() };
        registry.register(
            ToolDescriptor {
                id: "ping",
                category: "server",
                access: ToolAccess::Read,
                description: "Health check. Returns server name and version so a client can \
                              verify the connection end-to-end.",
            },
            empty_object_schema(),
            Arc::new(|_ctx, _args| {
                Box::pin(async {
                    let payload = serde_json::json!({
                        "status": "ok",
                        "server": "unkai-mail",
                        "version": env!("CARGO_PKG_VERSION"),
                    });
                    Ok(CallToolResult::success(vec![ContentBlock::text(
                        payload.to_string(),
                    )]))
                })
            }),
        );
        crate::mail::register_mail_tools(&mut registry);
        registry
    }

    pub(crate) fn register(
        &mut self,
        descriptor: ToolDescriptor,
        input_schema: JsonObject,
        handler: ToolHandlerFn,
    ) {
        debug_assert!(
            self.get(descriptor.id).is_none(),
            "duplicate tool id '{}'",
            descriptor.id
        );
        self.tools.push(RegisteredTool {
            descriptor,
            input_schema: Arc::new(input_schema),
            handler,
        });
    }

    pub fn get(&self, id: &str) -> Option<&RegisteredTool> {
        self.tools.iter().find(|t| t.descriptor.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &RegisteredTool> {
        self.tools.iter()
    }
}

/// `{"type": "object", "properties": {}}` — the schema for tools
/// that take no arguments.
fn empty_object_schema() -> JsonObject {
    let mut schema = JsonObject::new();
    schema.insert("type".into(), "object".into());
    schema.insert(
        "properties".into(),
        serde_json::Value::Object(JsonObject::new()),
    );
    schema
}

/// Whether `tool` is currently enabled: the user's explicit map
/// entry wins; otherwise reads default on and writes default off.
pub fn is_enabled(settings: &AppSettings, tool: &ToolDescriptor) -> bool {
    settings
        .mcp_tool_enablement
        .get(tool.id)
        .copied()
        .unwrap_or(tool.access == ToolAccess::Read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(id: &'static str, access: ToolAccess) -> ToolDescriptor {
        ToolDescriptor {
            id,
            category: "test",
            access,
            description: "test tool",
        }
    }

    #[test]
    fn read_tools_default_on_write_tools_default_off() {
        let settings = AppSettings::default();
        assert!(is_enabled(&settings, &descriptor("r", ToolAccess::Read)));
        assert!(!is_enabled(&settings, &descriptor("w", ToolAccess::Write)));
    }

    #[test]
    fn explicit_map_entry_overrides_class_default() {
        let mut settings = AppSettings::default();
        settings.mcp_tool_enablement.insert("r".into(), false);
        settings.mcp_tool_enablement.insert("w".into(), true);
        assert!(!is_enabled(&settings, &descriptor("r", ToolAccess::Read)));
        assert!(is_enabled(&settings, &descriptor("w", ToolAccess::Write)));
    }

    #[test]
    fn builtin_registry_contains_ping() {
        let registry = ToolRegistry::builtin();
        let ping = registry.get("ping").expect("ping registered");
        assert_eq!(ping.descriptor.access, ToolAccess::Read);
        assert!(registry.get("does-not-exist").is_none());
    }

    #[tokio::test]
    async fn ping_reports_ok() {
        let registry = ToolRegistry::builtin();
        let ping = registry.get("ping").expect("ping registered");
        let ctx = ToolContext {
            cache: Cache::open_in_memory().expect("in-memory cache"),
            settings: Arc::new(tokio::sync::RwLock::new(AppSettings::default())),
        };
        let result = ping.invoke(ctx, None).await.expect("ping succeeds");
        assert_eq!(result.is_error, Some(false));
        let text = result.content[0].as_text().expect("text content");
        assert!(text.text.contains("\"status\":\"ok\""));
    }
}
