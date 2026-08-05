//! Hardcoded connection presets for widely-used mail providers.
//!
//! Network discovery (autoconfig / SRV) covers most domains, but it
//! has two gaps the setup wizard cares about:
//!
//! 1. It only works once the user has typed a *matching* email
//!    address — there's no way to browse "which providers does this
//!    client know about?".
//! 2. It needs the network. A preset table answers instantly and
//!    offline, which matters on first launch (hotel Wi-Fi captive
//!    portals, corporate proxies, …).
//!
//! So the wizard offers this table as a pick-list *and*
//! [`crate::discover`] consults it before any network probe. The
//! values are the providers' published IMAP/SMTP endpoints — they
//! change rarely, and when they do the autoconfig path (which the
//! user can still trigger by picking "detect automatically") acts as
//! the up-to-date fallback.
//!
//! Ports follow what the rest of the app supports: IMAP is always
//! 993 (implicit TLS — the IMAP client doesn't speak STARTTLS), SMTP
//! is 465 (implicit) or 587 (STARTTLS), both of which the SMTP client
//! selects by port.

use serde::{Deserialize, Serialize};

use crate::{DiscoveredAccount, DiscoverySource};

/// Extra setup requirement the UI should surface for a preset.
///
/// Machine-readable on purpose: the frontend maps each variant to a
/// localised hint string instead of us baking English prose into the
/// backend.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PresetHint {
    /// The provider rejects the normal account password over IMAP —
    /// the user must generate an app-specific password first.
    AppPassword,
    /// The provider ships with remote mail access (IMAP/SMTP) turned
    /// off; the user must enable it in the provider's web settings.
    EnableRemoteAccess,
}

/// One provider entry for the wizard pick-list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderPreset {
    /// Stable machine id (kebab-case) — the UI keys its selection on
    /// this, never on the display name.
    pub id: String,
    /// Human-facing provider name for the pick-list.
    pub display_name: String,
    /// Email domains served by this provider. Used to match a typed
    /// address to a preset before any network discovery runs.
    pub domains: Vec<String>,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_tls: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_tls: bool,
    /// JMAP session base URL for providers that offer it. When set,
    /// the wizard can pre-enable the JMAP toggle.
    pub jmap_url: Option<String>,
    /// Extra setup requirement to hint at in the UI, if any.
    pub hint: Option<PresetHint>,
}

impl ProviderPreset {
    /// Convert to the same shape network discovery returns, so the
    /// wizard's prefill code has a single input type.
    pub fn to_discovered(&self) -> DiscoveredAccount {
        DiscoveredAccount {
            imap_host: self.imap_host.clone(),
            imap_port: self.imap_port,
            imap_tls: self.imap_tls,
            smtp_host: self.smtp_host.clone(),
            smtp_port: self.smtp_port,
            smtp_tls: self.smtp_tls,
            source: DiscoverySource::Preset,
        }
    }
}

/// Terse constructor keeping the table below readable.
#[allow(clippy::too_many_arguments)]
fn preset(
    id: &str,
    display_name: &str,
    domains: &[&str],
    imap_host: &str,
    smtp_host: &str,
    smtp_port: u16,
    jmap_url: Option<&str>,
    hint: Option<PresetHint>,
) -> ProviderPreset {
    ProviderPreset {
        id: id.to_string(),
        display_name: display_name.to_string(),
        domains: domains.iter().map(|d| d.to_string()).collect(),
        imap_host: imap_host.to_string(),
        imap_port: 993,
        imap_tls: true,
        smtp_host: smtp_host.to_string(),
        smtp_port,
        // Port 465 is implicit TLS, 587 is STARTTLS — mirrors how the
        // SMTP client picks its handshake.
        smtp_tls: smtp_port == 465,
        jmap_url: jmap_url.map(|u| u.to_string()),
        hint,
    }
}

/// The full preset table, in pick-list display order.
pub fn all() -> Vec<ProviderPreset> {
    vec![
        preset(
            "gmail",
            "Gmail",
            &["gmail.com", "googlemail.com"],
            "imap.gmail.com",
            "smtp.gmail.com",
            465,
            None,
            Some(PresetHint::AppPassword),
        ),
        preset(
            "outlook",
            "Outlook / Microsoft 365",
            &[
                "outlook.com",
                "outlook.de",
                "hotmail.com",
                "hotmail.de",
                "live.com",
                "live.de",
                "msn.com",
            ],
            "outlook.office365.com",
            "smtp-mail.outlook.com",
            587,
            None,
            Some(PresetHint::AppPassword),
        ),
        preset(
            "icloud",
            "iCloud Mail",
            &["icloud.com", "me.com", "mac.com"],
            "imap.mail.me.com",
            "smtp.mail.me.com",
            587,
            None,
            Some(PresetHint::AppPassword),
        ),
        preset(
            "yahoo",
            "Yahoo Mail",
            &["yahoo.com", "yahoo.de", "ymail.com", "rocketmail.com"],
            "imap.mail.yahoo.com",
            "smtp.mail.yahoo.com",
            465,
            None,
            Some(PresetHint::AppPassword),
        ),
        preset(
            "gmx",
            "GMX",
            &["gmx.net", "gmx.de", "gmx.at", "gmx.ch", "gmx.com"],
            "imap.gmx.net",
            "mail.gmx.net",
            465,
            None,
            Some(PresetHint::EnableRemoteAccess),
        ),
        preset(
            "webde",
            "WEB.DE",
            &["web.de"],
            "imap.web.de",
            "smtp.web.de",
            587,
            None,
            Some(PresetHint::EnableRemoteAccess),
        ),
        preset(
            "fastmail",
            "Fastmail",
            &["fastmail.com", "fastmail.fm"],
            "imap.fastmail.com",
            "smtp.fastmail.com",
            465,
            Some("https://api.fastmail.com"),
            Some(PresetHint::AppPassword),
        ),
        preset(
            "mailbox-org",
            "mailbox.org",
            &["mailbox.org"],
            "imap.mailbox.org",
            "smtp.mailbox.org",
            465,
            None,
            None,
        ),
    ]
}

/// Find the preset serving `domain` (case-insensitive), if any.
pub fn for_domain(domain: &str) -> Option<ProviderPreset> {
    let needle = domain.trim().to_lowercase();
    all().into_iter().find(|p| p.domains.contains(&needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_domain_case_insensitively() {
        let p = for_domain("GMAIL.com").expect("preset");
        assert_eq!(p.id, "gmail");
        assert_eq!(p.imap_host, "imap.gmail.com");
    }

    #[test]
    fn unknown_domain_returns_none() {
        assert!(for_domain("example.com").is_none());
    }

    #[test]
    fn table_is_internally_consistent() {
        let presets = all();
        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_domains = std::collections::HashSet::new();
        for p in &presets {
            // ids unique + kebab-case-ish
            assert!(seen_ids.insert(p.id.clone()), "duplicate id {}", p.id);
            assert!(!p.display_name.is_empty());
            // domains unique across the whole table — a domain that
            // matched two presets would make the pick order random
            for d in &p.domains {
                assert!(seen_domains.insert(d.clone()), "duplicate domain {d}");
                assert_eq!(*d, d.to_lowercase(), "domains must be lowercase");
            }
            // Only TLS setups the mail stack actually supports.
            assert_eq!(p.imap_port, 993, "{}: IMAP must be implicit TLS", p.id);
            assert!(p.imap_tls);
            assert!(
                matches!(p.smtp_port, 465 | 587),
                "{}: unexpected SMTP port",
                p.id
            );
            assert_eq!(p.smtp_tls, p.smtp_port == 465);
        }
    }

    #[test]
    fn to_discovered_tags_preset_source() {
        let d = for_domain("mailbox.org").unwrap().to_discovered();
        assert_eq!(d.source, DiscoverySource::Preset);
        assert_eq!(d.imap_host, "imap.mailbox.org");
    }
}
