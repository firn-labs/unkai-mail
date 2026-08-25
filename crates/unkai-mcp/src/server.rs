//! The MCP protocol handler and HTTP router (#438).
//!
//! `UnkaiMcp` implements rmcp's `ServerHandler` manually rather
//! than through the `#[tool_router]` macros: the macro path bakes
//! a static tool list into the type, while we need `tools/list`
//! to reflect the user's live per-tool enablement and
//! `tools/call` to re-check it on every invocation.  The manual
//! impl keeps both decisions in one place, backed by the
//! [`crate::registry`] scaffolding.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use tokio_util::sync::CancellationToken;

use crate::registry::{ToolContext, ToolRegistry, is_available, is_enabled};
use crate::{GuardState, SharedSettings};

/// Path the MCP endpoint is mounted on:
/// `http://127.0.0.1:<port>/mcp`.
pub const ENDPOINT_PATH: &str = "/mcp";

/// One MCP session's server side.  rmcp's session layer creates
/// an instance per client session via the factory closure in
/// [`build_router`]; all instances share the registry and the
/// live settings through `Arc`s.
#[derive(Clone)]
pub struct UnkaiMcp {
    ctx: ToolContext,
    settings: SharedSettings,
    registry: Arc<ToolRegistry>,
}

impl UnkaiMcp {
    pub fn new(ctx: ToolContext, settings: SharedSettings, registry: Arc<ToolRegistry>) -> Self {
        Self {
            ctx,
            settings,
            registry,
        }
    }
}

impl ServerHandler for UnkaiMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("unkai-mail", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Local MCP server of the Unkai Mail desktop app. Tools surface the user's \
             mail, contacts, and calendar data; which tools are available is controlled \
             by the user in Unkai Mail's AI settings."
                .to_string(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Snapshot the settings once so one listing can't observe
        // a mid-flight toggle half-applied; same for the connected
        // sources backing the availability check (#441 — groupware
        // tools are only advertised while a connection offers
        // their feature).
        let settings = self.settings.read().await;
        let nc_accounts = crate::nc::load_nc_accounts(&self.ctx.cache);
        let tools = self
            .registry
            .iter()
            .filter(|tool| {
                is_enabled(&settings, &tool.descriptor)
                    && is_available(&nc_accounts, &tool.descriptor)
            })
            .map(|tool| tool.to_tool())
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(tool) = self.registry.get(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            ));
        };

        // Re-check enablement server-side: advertising is not
        // authorisation.  A client holding a stale tools/list (or
        // guessing ids) must still be refused here.
        let enabled = {
            let settings = self.settings.read().await;
            is_enabled(&settings, &tool.descriptor)
        };
        if !enabled {
            return Err(ErrorData::invalid_request(
                format!(
                    "tool '{}' is disabled in Unkai Mail's AI settings",
                    request.name
                ),
                None,
            ));
        }

        // Availability is re-checked too (#441): a client holding
        // a stale tools/list must not reach a groupware tool whose
        // backing connection has since been removed.
        let nc_accounts = crate::nc::load_nc_accounts(&self.ctx.cache);
        if !is_available(&nc_accounts, &tool.descriptor) {
            return Err(ErrorData::invalid_request(
                format!(
                    "tool '{}' is unavailable — no connected Nextcloud / DAV source offers \
                     the feature it needs",
                    request.name
                ),
                None,
            ));
        }

        // Belt-and-braces: the HTTP middleware already refuses
        // requests while the vault is locked, but the lock can
        // flip between the middleware check and the handler
        // running, and future transports may not share the
        // middleware.
        if self.ctx.cache.is_locked() {
            return Err(ErrorData::internal_error(
                "Unkai Mail's encrypted vault is locked. Unlock the app, then retry.",
                None,
            ));
        }

        // rmcp 3 wraps tool outcomes in `CallToolResponse` (Complete /
        // InputRequired / Task); our registry tools always complete.
        tool.invoke(self.ctx.clone(), request.arguments)
            .await
            .map(CallToolResponse::from)
    }
}

/// Assemble the axum router: rmcp's streamable-HTTP tower service
/// mounted at [`ENDPOINT_PATH`], wrapped in the
/// [`crate::auth::request_guard`] middleware.
///
/// `cancel` is shared with the session layer so cancelling it
/// both stops the listener (via graceful shutdown in the caller)
/// and terminates any live SSE sessions.
pub fn build_router(
    ctx: ToolContext,
    settings: SharedSettings,
    registry: Arc<ToolRegistry>,
    guard: GuardState,
    cancel: CancellationToken,
) -> axum::Router {
    // The config struct is `#[non_exhaustive]`, so mutate a
    // default instead of a struct literal.  Everything else keeps
    // rmcp's defaults — notably the loopback-only allowed_hosts
    // list, which backs up our own Host validation in the
    // middleware.
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = cancel;

    let service = StreamableHttpService::new(
        move || {
            Ok(UnkaiMcp::new(
                ctx.clone(),
                settings.clone(),
                registry.clone(),
            ))
        },
        LocalSessionManager::default().into(),
        config,
    );

    axum::Router::new()
        .nest_service(ENDPOINT_PATH, service)
        .layer(axum::middleware::from_fn_with_state(
            guard,
            crate::auth::request_guard,
        ))
}
