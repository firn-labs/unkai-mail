//! Local MCP (Model Context Protocol) server for Unkai Mail (#438).
//!
//! Hosts a **localhost-only** streamable-HTTP MCP endpoint inside
//! the running desktop app so the user can point their own AI
//! client (BYO model — Unkai never ships or calls an LLM) at
//! `http://127.0.0.1:<port>/mcp`.
//!
//! Architecture:
//!
//! - [`McpServer`] is the lifecycle controller the Tauri layer
//!   owns: it starts/stops the listener as `AppSettings::
//!   mcp_enabled` / `mcp_port` change ([`McpServer::reconcile`])
//!   and holds the in-memory bearer token.
//! - [`server`] mounts rmcp's streamable-HTTP tower service into
//!   an axum router and implements the MCP handler against the
//!   tool registry.
//! - [`auth`] guards every request: loopback `Host` validation
//!   (DNS-rebinding defence), `Origin` rejection, constant-time
//!   bearer auth, and a "vault locked" gate while the SQLCipher
//!   cache is FIDO-locked.
//! - [`registry`] is the tool scaffolding — stable ids, category
//!   and read/write classification, per-tool enablement (reads
//!   default on, writes default off) enforced at both
//!   `tools/list` and `tools/call`.
//!
//! Secrets: the bearer token lives in the OS keychain
//! (`unkai-mail-mcp` service, via `unkai_store::credentials`) and
//! in this process's memory — never in `AppSettings`, so it can
//! never ride along in the Nextcloud settings-sync bundle.

pub mod auth;
pub mod calendar;
pub mod contacts;
pub mod invite_html;
pub mod mail;
pub mod meeting;
pub mod nc;
pub mod registry;
pub mod server;
pub mod talk;
#[cfg(test)]
pub(crate) mod testutil;
mod util;

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use unkai_core::models::AppSettings;
use unkai_store::Cache;

use crate::registry::{ToolContext, ToolRegistry};

/// Live app settings shared with the Tauri layer — same shape as
/// the `SharedSettings` alias in `src-tauri/src/main.rs`.
pub type SharedSettings = Arc<RwLock<AppSettings>>;

/// State the request-guard middleware needs on every request.
#[derive(Clone)]
pub struct GuardState {
    /// In-memory copy of the bearer token (`None` = no token
    /// generated ⇒ every request is 401).  Kept in memory so we
    /// don't do an OS-keychain round-trip per request; the Tauri
    /// token commands update it alongside the keychain.
    pub token: Arc<RwLock<Option<String>>>,
    /// For the "vault locked" gate.
    pub cache: Cache,
}

/// A running listener and the knobs to stop it.
struct RunningServer {
    /// The port from settings at start time — compared against
    /// the current settings by `reconcile` to detect a change.
    /// (`bound_port` differs when this is 0.)
    configured_port: u16,
    /// The port the OS actually bound.
    bound_port: u16,
    cancel: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

/// Snapshot for the AI settings page (`mcp_server_status`).
#[derive(Debug, Clone, Serialize)]
pub struct McpServerStatus {
    /// Whether a listener is currently accepting connections.
    pub running: bool,
    /// The actually-bound port while running.
    pub port: Option<u16>,
    /// Full endpoint URL while running, e.g.
    /// `http://127.0.0.1:52226/mcp`.
    pub endpoint: Option<String>,
    /// Why the last start attempt failed (port in use, …), if it
    /// did.  Cleared on a successful start.
    pub last_error: Option<String>,
}

struct Inner {
    cache: Cache,
    settings: SharedSettings,
    token: Arc<RwLock<Option<String>>>,
    registry: Arc<ToolRegistry>,
    /// `Mutex` (not `RwLock`) because every touch point mutates,
    /// and it serialises concurrent `reconcile` calls so two
    /// racing settings updates can't double-start a listener.
    running: Mutex<Option<RunningServer>>,
    last_error: Mutex<Option<String>>,
}

/// Lifecycle controller for the MCP server.  Cheap to clone
/// (`Arc` inside); the Tauri layer keeps one in managed state.
#[derive(Clone)]
pub struct McpServer {
    inner: Arc<Inner>,
}

impl McpServer {
    /// `initial_token` is the keychain-loaded bearer token (or
    /// `None` when the user never generated one).  Passed in
    /// rather than read here so this crate stays keychain-free
    /// and tests can inject a known token.
    pub fn new(cache: Cache, settings: SharedSettings, initial_token: Option<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                cache,
                settings,
                token: Arc::new(RwLock::new(initial_token)),
                registry: Arc::new(ToolRegistry::builtin()),
                running: Mutex::new(None),
                last_error: Mutex::new(None),
            }),
        }
    }

    /// Swap the in-memory bearer token.  The caller (Tauri token
    /// commands) is responsible for the keychain side; passing
    /// `None` revokes — every subsequent request 401s, including
    /// on already-established sessions, because the guard checks
    /// per request.
    pub async fn set_token(&self, token: Option<String>) {
        *self.inner.token.write().await = token;
    }

    /// Bring the listener in line with the current settings:
    /// start it when `mcp_enabled` and not running, stop it when
    /// disabled, restart it when the configured port changed.
    /// Never fails — problems land in `status().last_error` and
    /// the log, because callers (boot, settings updates) have no
    /// meaningful way to handle them.
    pub async fn reconcile(&self) {
        let (enabled, port) = {
            let settings = self.inner.settings.read().await;
            (settings.mcp_enabled, settings.mcp_port)
        };

        let mut running = self.inner.running.lock().await;

        // Stop whatever no longer matches the desired state.
        if let Some(current) = running.as_ref() {
            if enabled && current.configured_port == port {
                return; // Already in the desired state.
            }
            let current = running.take().expect("checked Some above");
            tracing::info!(
                "Stopping MCP server on 127.0.0.1:{} ({})",
                current.bound_port,
                if enabled { "port change" } else { "disabled" }
            );
            current.cancel.cancel();
            // Graceful shutdown normally returns quickly (the
            // cancellation token also tears down live SSE
            // sessions), but never let a stuck connection wedge
            // the reconcile path.
            let join = current.join;
            if tokio::time::timeout(std::time::Duration::from_secs(5), join)
                .await
                .is_err()
            {
                tracing::warn!("MCP server did not shut down within 5s; continuing");
            }
        }

        if !enabled {
            return;
        }

        // Start the listener.
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                let message = format!("failed to bind 127.0.0.1:{port}: {e}");
                tracing::warn!("MCP server not started: {message}");
                *self.inner.last_error.lock().await = Some(message);
                return;
            }
        };
        let bound_port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(e) => {
                let message = format!("failed to read bound address: {e}");
                tracing::warn!("MCP server not started: {message}");
                *self.inner.last_error.lock().await = Some(message);
                return;
            }
        };

        let cancel = CancellationToken::new();
        let router = server::build_router(
            ToolContext {
                cache: self.inner.cache.clone(),
                settings: self.inner.settings.clone(),
            },
            self.inner.settings.clone(),
            self.inner.registry.clone(),
            GuardState {
                token: self.inner.token.clone(),
                cache: self.inner.cache.clone(),
            },
            cancel.clone(),
        );

        let shutdown = cancel.clone();
        let join = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
            {
                tracing::warn!("MCP server exited with error: {e}");
            }
        });

        tracing::info!(
            "MCP server listening on http://127.0.0.1:{bound_port}{}",
            server::ENDPOINT_PATH
        );
        *self.inner.last_error.lock().await = None;
        *running = Some(RunningServer {
            configured_port: port,
            bound_port,
            cancel,
            join,
        });
    }

    /// Stop the listener unconditionally, regardless of the
    /// `mcp_enabled` setting.  Used when a profile's runtime
    /// context is torn down (#535) — the settings say "enabled"
    /// but the profile is going away, so the listener must not
    /// outlive its cache.  Idempotent; a later `reconcile` on a
    /// fresh server instance starts it again.
    pub async fn shutdown(&self) {
        let mut running = self.inner.running.lock().await;
        if let Some(current) = running.take() {
            tracing::info!(
                "Stopping MCP server on 127.0.0.1:{} (context shutdown)",
                current.bound_port
            );
            current.cancel.cancel();
            if tokio::time::timeout(std::time::Duration::from_secs(5), current.join)
                .await
                .is_err()
            {
                tracing::warn!("MCP server did not shut down within 5s; continuing");
            }
        }
    }

    /// Status snapshot for the settings UI.
    pub async fn status(&self) -> McpServerStatus {
        let running = self.inner.running.lock().await;
        let last_error = self.inner.last_error.lock().await.clone();
        match running.as_ref() {
            Some(current) => McpServerStatus {
                running: true,
                port: Some(current.bound_port),
                endpoint: Some(format!(
                    "http://127.0.0.1:{}{}",
                    current.bound_port,
                    server::ENDPOINT_PATH
                )),
                last_error,
            },
            None => McpServerStatus {
                running: false,
                port: None,
                endpoint: None,
                last_error,
            },
        }
    }
}
