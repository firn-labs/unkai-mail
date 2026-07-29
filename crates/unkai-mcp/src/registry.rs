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

/// Which groupware surface a tool needs from a connected DAV /
/// Nextcloud source (#441).  Drives *availability*: a tool whose
/// feature no connected source offers is neither advertised in
/// `tools/list` nor callable — there is nothing it could answer
/// with.  Distinct from *enablement*, which is the user's choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextcloudFeature {
    /// CardDAV contacts (real Nextcloud, generic DAV, or a local
    /// source with contacts enabled).
    Contacts,
    /// CalDAV calendars (same source spread as `Contacts`).
    Calendar,
    /// Nextcloud Talk — OCS-only, so a real Nextcloud whose
    /// capability snapshot says the Talk app is installed.
    Talk,
}

/// Static metadata for one tool.  `id` is the wire name MCP
/// clients call and the key in the enablement map — treat it as a
/// public API and never rename an existing one.
#[derive(Debug, Clone, Copy)]
pub struct ToolDescriptor {
    pub id: &'static str,
    /// Coarse grouping for the settings UI (`"server"`, `"mail"`,
    /// `"contacts"`, `"calendar"`, `"talk"`).
    pub category: &'static str,
    pub access: ToolAccess,
    /// `Some(feature)` gates the tool on a connected source
    /// offering that feature; `None` means always available.
    pub requires: Option<NextcloudFeature>,
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
                requires: None,
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
        crate::contacts::register_contact_tools(&mut registry);
        crate::calendar::register_calendar_tools(&mut registry);
        crate::talk::register_talk_tools(&mut registry);
        crate::meeting::register_meeting_tools(&mut registry);
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

/// Whether `tool` is *available* given the currently connected
/// DAV / Nextcloud sources (#441): a tool with no `requires` is
/// always available; one that needs Talk / calendars / contacts
/// is only offered while some connected source has that feature.
/// Checked at both `tools/list` and `tools/call`, same as
/// enablement.  `accounts` is the caller's snapshot so one
/// listing reads the store once, not once per tool.
pub fn is_available(
    accounts: &[unkai_core::models::NextcloudAccount],
    tool: &ToolDescriptor,
) -> bool {
    match tool.requires {
        None => true,
        Some(feature) => crate::nc::feature_available(accounts, feature),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(id: &'static str, access: ToolAccess) -> ToolDescriptor {
        ToolDescriptor {
            id,
            category: "test",
            access,
            requires: None,
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
    fn groupware_tools_registered_with_expected_defaults() {
        let registry = ToolRegistry::builtin();
        let settings = AppSettings::default();
        for (id, feature) in [
            ("search_contacts", NextcloudFeature::Contacts),
            ("list_calendars", NextcloudFeature::Calendar),
            ("get_events", NextcloudFeature::Calendar),
            ("get_availability", NextcloudFeature::Calendar),
            ("list_talk_rooms", NextcloudFeature::Talk),
        ] {
            let tool = registry
                .get(id)
                .unwrap_or_else(|| panic!("{id} registered"));
            assert_eq!(
                tool.descriptor.access,
                ToolAccess::Read,
                "{id} is a read tool"
            );
            assert!(is_enabled(&settings, &tool.descriptor), "{id} defaults on");
            assert_eq!(tool.descriptor.requires, Some(feature), "{id} gating");
        }
        for (id, feature) in [
            ("create_contact", NextcloudFeature::Contacts),
            ("create_event", NextcloudFeature::Calendar),
            ("rsvp_event", NextcloudFeature::Calendar),
            ("create_talk_room", NextcloudFeature::Talk),
            ("create_meeting_invite", NextcloudFeature::Calendar),
        ] {
            let tool = registry
                .get(id)
                .unwrap_or_else(|| panic!("{id} registered"));
            assert_eq!(
                tool.descriptor.access,
                ToolAccess::Write,
                "{id} is a write tool"
            );
            assert!(
                !is_enabled(&settings, &tool.descriptor),
                "{id} defaults off"
            );
            assert_eq!(tool.descriptor.requires, Some(feature), "{id} gating");
        }
    }

    #[test]
    fn availability_gates_on_connected_features() {
        use crate::nc::test_support::{caps, nc_account};
        use unkai_core::models::DavSourceKind;

        let registry = ToolRegistry::builtin();
        let talk_tool = &registry.get("list_talk_rooms").unwrap().descriptor;
        let calendar_tool = &registry.get("get_events").unwrap().descriptor;
        let contacts_tool = &registry.get("search_contacts").unwrap().descriptor;
        let mail_tool = &registry.get("search_mail").unwrap().descriptor;

        // Nothing connected: only ungated tools are available.
        assert!(is_available(&[], mail_tool));
        assert!(!is_available(&[], talk_tool));
        assert!(!is_available(&[], calendar_tool));
        assert!(!is_available(&[], contacts_tool));

        // A Talk-less Nextcloud offers DAV but not Talk.
        let no_talk = vec![nc_account(
            "a",
            DavSourceKind::Nextcloud,
            Some(caps(false, true, true)),
        )];
        assert!(!is_available(&no_talk, talk_tool));
        assert!(is_available(&no_talk, calendar_tool));
        assert!(is_available(&no_talk, contacts_tool));

        // Talk shows up once any connection has the app.
        let with_talk = vec![nc_account(
            "a",
            DavSourceKind::Nextcloud,
            Some(caps(true, true, true)),
        )];
        assert!(is_available(&with_talk, talk_tool));
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
