//! Unkai Discovery — autoconfigure IMAP/SMTP servers from an email
//! address.
//!
//! Three strategies, tried in order:
//!
//! 1. **Hardcoded presets** ([`presets`]) — a small table of
//!    widely-used providers (#413). Instant and offline; also
//!    powers the wizard's provider pick-list.
//!
//! 2. **Mozilla autoconfig** ([`autoconfig`]) — a small XML file the
//!    user's domain (or Mozilla's ISP database) hosts that names the
//!    incoming/outgoing servers. Covers most major mail providers
//!    plus every German Hoster that points at Mozilla's autoconfig
//!    database.
//!
//! 3. **DNS SRV records** ([`srv`]) — RFC 6186 records like
//!    `_imaps._tcp.<domain>` that point at the right host/port.
//!    Slower to configure on the provider side, but it's the
//!    standard fallback when autoconfig isn't available.
//!
//! The top-level [`discover`] entry point runs all three and returns
//! the first hit. Callers (the account-setup wizard) prefill the form
//! with whatever it finds and let the user override anything they
//! disagree with.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug;

pub mod autoconfig;
pub mod presets;
pub mod srv;

pub use presets::{PresetHint, ProviderPreset};

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("network error: {0}")]
    Network(String),
    #[error("invalid response: {0}")]
    Parse(String),
    #[error("no autoconfig data found for domain")]
    NotFound,
}

/// IMAP/SMTP server settings discovered from an email address.
/// Mirrors the relevant subset of the `Account` struct so the UI can
/// drop the result straight into its setup form.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredAccount {
    pub imap_host: String,
    pub imap_port: u16,
    /// Whether the IMAP port uses implicit TLS. `true` for 993,
    /// `false` for 143 (which would expect STARTTLS — Unkai today
    /// only supports implicit TLS, but we surface the flag so the UI
    /// can warn or future code can branch).
    pub imap_tls: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    /// `true` for implicit TLS (port 465), `false` for STARTTLS
    /// (port 587). Same caveat as `imap_tls` — informational for now.
    pub smtp_tls: bool,
    /// Where this answer came from, for logging / UX hinting.
    pub source: DiscoverySource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DiscoverySource {
    /// XML from `autoconfig.<domain>` or `<domain>/.well-known/...`
    /// — the user's own provider published it.
    AutoconfigDomain,
    /// XML from the public Mozilla ISP database (served at
    /// `autoconfig.thunderbird.net`, the canonical endpoint of
    /// the Mozilla autoconfig protocol) — Mozilla curates it.
    AutoconfigIspdb,
    /// SRV records resolved from `_imaps._tcp.<domain>` etc.
    Srv,
    /// Matched a hardcoded entry in the [`presets`] table — no
    /// network involved (#413).
    Preset,
}

/// Try every discovery method for `email`'s domain. Returns the
/// first match, or `Err(NotFound)` if nothing works.
///
/// Network errors from individual probes are logged but not surfaced
/// — a single broken provider shouldn't kill the whole flow when
/// another route still works. Only when *all* routes fail do we
/// return an error.
pub async fn discover(email: &str) -> Result<DiscoveredAccount, DiscoveryError> {
    let domain = email
        .split_once('@')
        .map(|(_, d)| d.trim().to_lowercase())
        .filter(|d| !d.is_empty())
        .ok_or_else(|| DiscoveryError::Parse(format!("not an email: {email:?}")))?;

    debug!("Autodiscover starting for domain '{domain}'");

    // Preset table first: instant, offline, and immune to a
    // provider's autoconfig endpoint having a bad day.
    if let Some(p) = presets::for_domain(&domain) {
        debug!("preset '{}' matched domain '{domain}'", p.id);
        return Ok(p.to_discovered());
    }

    match autoconfig::discover(&domain, email).await {
        Ok(found) => return Ok(found),
        Err(e) => debug!("autoconfig failed for '{domain}': {e}"),
    }

    match srv::discover(&domain).await {
        Ok(found) => return Ok(found),
        Err(e) => debug!("SRV lookup failed for '{domain}': {e}"),
    }

    Err(DiscoveryError::NotFound)
}
