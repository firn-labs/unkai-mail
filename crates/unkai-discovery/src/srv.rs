//! DNS SRV-based autoconfiguration (RFC 6186).
//!
//! Some providers don't host an autoconfig XML but do publish SRV
//! records like:
//!
//! ```text
//! _imaps._tcp.example.com. 3600 IN SRV 0 1 993 imap.example.com.
//! _submission._tcp.example.com. 3600 IN SRV 0 1 587 smtp.example.com.
//! ```
//!
//! We probe in priority order: implicit-TLS first (`_imaps`,
//! `_submissions`), STARTTLS second (`_imap`, `_submission`). The
//! first record set that gives us an IMAP+SMTP pair wins.

use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::ResolverConfig;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::RData;
use tracing::debug;

use crate::{DiscoveredAccount, DiscoveryError, DiscoverySource};

/// Resolve SRV records for the given mail domain. Returns the first
/// (IMAP, SMTP) pair we manage to put together; `Err(NotFound)` if
/// no usable records exist.
pub async fn discover(domain: &str) -> Result<DiscoveredAccount, DiscoveryError> {
    let mut builder = TokioResolver::builder_with_config(
        ResolverConfig::default(),
        TokioRuntimeProvider::default(),
    );
    // Default DNS timeout is 5s per attempt × 2 attempts = 10s — too
    // long when the wizard is waiting on us. Cap it at 4s total.
    builder.options_mut().timeout = Duration::from_secs(2);
    builder.options_mut().attempts = 2;

    let resolver = builder
        .build()
        .map_err(|e| DiscoveryError::Network(format!("DNS resolver init failed: {e}")))?;

    // Implicit-TLS first.
    let imap_tls = lookup_first_srv(&resolver, "_imaps._tcp", domain).await;
    let smtp_tls = lookup_first_srv(&resolver, "_submissions._tcp", domain).await;
    if let (Some(i), Some(s)) = (&imap_tls, &smtp_tls) {
        debug!("SRV: TLS pair imap={i:?} smtp={s:?}");
        return Ok(DiscoveredAccount {
            imap_host: i.host.clone(),
            imap_port: i.port,
            imap_tls: true,
            smtp_host: s.host.clone(),
            smtp_port: s.port,
            smtp_tls: true,
            source: DiscoverySource::Srv,
        });
    }

    // STARTTLS fallback.
    let imap_starttls = lookup_first_srv(&resolver, "_imap._tcp", domain).await;
    let smtp_starttls = lookup_first_srv(&resolver, "_submission._tcp", domain).await;
    if let (Some(i), Some(s)) = (imap_tls.or(imap_starttls), smtp_tls.or(smtp_starttls)) {
        debug!("SRV: mixed/STARTTLS pair imap={i:?} smtp={s:?}");
        return Ok(DiscoveredAccount {
            imap_host: i.host,
            imap_port: i.port,
            imap_tls: matches!(i.port, 993),
            smtp_host: s.host,
            smtp_port: s.port,
            smtp_tls: matches!(s.port, 465),
            source: DiscoverySource::Srv,
        });
    }

    Err(DiscoveryError::NotFound)
}

#[derive(Debug, Clone)]
struct SrvEndpoint {
    host: String,
    port: u16,
}

/// Look up `<service>.<domain>` and return the lowest-priority,
/// highest-weight record's host:port. RFC 2782 sort order would have
/// us pick by priority then weight; for autoconfig the first
/// answer is fine because providers rarely publish multiple servers
/// here, and the user can always override.
async fn lookup_first_srv(
    resolver: &TokioResolver,
    service: &str,
    domain: &str,
) -> Option<SrvEndpoint> {
    let name = format!("{service}.{domain}.");
    match resolver.srv_lookup(&name).await {
        Ok(lookup) => {
            let mut best: Option<(u16, u16, SrvEndpoint)> = None;
            for record in lookup.answers() {
                let RData::SRV(ref srv) = record.data else {
                    continue;
                };
                let host = srv.target.to_ascii().trim_end_matches('.').to_string();
                if host.is_empty() {
                    continue;
                }
                let candidate = SrvEndpoint {
                    host,
                    port: srv.port,
                };
                let key = (srv.priority, u16::MAX - srv.weight);
                if best.as_ref().map(|(p, _, _)| key < (*p, 0)).unwrap_or(true) {
                    best = Some((key.0, key.1, candidate));
                }
            }
            best.map(|(_, _, ep)| ep)
        }
        Err(e) => {
            debug!("SRV lookup '{name}' failed: {e}");
            None
        }
    }
}
