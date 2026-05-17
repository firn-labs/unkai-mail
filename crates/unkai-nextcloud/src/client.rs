//! Shared HTTP client configuration for Nextcloud OCS calls.
//!
//! # Why a shared constructor
//!
//! All OCS (Open Collaboration Services) endpoints on Nextcloud require
//! two bits of etiquette:
//!
//! - **`OCS-APIRequest: true`** — Nextcloud rejects OCS calls without
//!   this header as a CSRF guard. Missing it is the #1 reason "my curl
//!   command works but my code doesn't" tickets exist.
//! - **`Accept: application/json`** — OCS speaks XML by default, which
//!   is horrible to parse. Flipping this header gets JSON back.
//!
//! Keeping the constructor in one place means we can't forget either
//! header when we add the next endpoint (Talk, Files, …).
//!
//! # Why not a `Client` struct
//!
//! Nextcloud endpoints need different authentication depending on the
//! phase of life: Login Flow v2 is unauthenticated, `cloud/capabilities`
//! takes Basic auth with the app password, and the user-info endpoint
//! same. Rather than a stateful client that has to juggle that, we
//! hand out a configured `reqwest::Client` and let each call attach its
//! own auth header — closer to how the underlying HTTP actually works.

use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;

use unkai_core::UnkaiError;
use unkai_core::models::TrustedCert;
use unkai_core::tls::build_client_config;

/// A single shared client is cheap to clone (`Arc` inside) and reuses
/// the TCP connection pool, so we pay the TLS handshake once per host.
///
/// `trusted_certs` is the per-account pre-trusted self-signed list.
/// Empty for the common public-CA case; populated for self-hosted NC
/// servers whose cert isn't in webpki-roots.  We plug it into the
/// shared rustls verifier (`unkai_core::tls::build_client_config`)
/// so the same fingerprint-fallback path that handles IMAP/SMTP
/// trust extends to NC HTTPS traffic.
pub fn build(trusted_certs: &[TrustedCert]) -> Result<Client, UnkaiError> {
    // `use_preconfigured_tls` accepts the rustls config directly.
    // Unwrap the `Arc` because `use_preconfigured_tls` takes
    // `Any + Send + Sync` (it stashes the value internally) and
    // unwrap-or-clone is cheap when nothing else holds the Arc.
    let rustls_config = Arc::unwrap_or_clone(build_client_config(trusted_certs));
    Client::builder()
        // Timeouts kept generous — Login Flow v2 polling is short but
        // some self-hosted Nextclouds answer slowly on cold starts.
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("Unkai Mail/", env!("CARGO_PKG_VERSION")))
        .use_preconfigured_tls(rustls_config)
        .build()
        .map_err(|e| UnkaiError::Network(format!("failed to build HTTP client: {e}")))
}

/// Strip a trailing `/` from a server URL so our `format!("{base}/foo")`
/// concatenations never produce `//foo`.
pub fn normalize_server_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}
