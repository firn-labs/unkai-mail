//! Mozilla autoconfig discovery protocol — fetch `config-v1.1.xml`
//! and pull the IMAP / SMTP server settings out of it.
//!
//! Three URLs to try, in order:
//!
//! 1. `https://autoconfig.<domain>/mail/config-v1.1.xml?emailaddress=<email>`
//!    — what the standard says provider-hosted autoconfig should
//!    look like. Most large providers expose this.
//! 2. `https://<domain>/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress=<email>`
//!    — the alternative the spec also blesses, occasionally used
//!    when the provider doesn't control the `autoconfig` subdomain.
//! 3. `https://autoconfig.thunderbird.net/v1.1/<domain>` — Mozilla's
//!    public ISP database (the `autoconfig.thunderbird.net` host is
//!    the canonical endpoint the autoconfig protocol publishes).
//!    Curated, covers most consumer-mail providers plus the long
//!    tail that bothered to submit a PR there.
//!
//! Each request gets a tight timeout; we don't want a flaky DNS
//! response on a misspelled domain to keep the user staring at a
//! spinner for 30 seconds. The first valid response wins.
//!
//! XML format is described by the Mozilla autoconfig spec at
//! <https://wiki.mozilla.org/Thunderbird:Autoconfiguration:ConfigFileFormat>
//! (the wiki page lives under the original implementer's namespace;
//! the format itself is what the autoconfig protocol defines).
//! We only care about `<incomingServer type="imap">` and
//! `<outgoingServer type="smtp">` — the rest of the schema (POP3,
//! Exchange, identity / SMTP submission auth strings) is ignored.

use std::time::Duration;

use quick_xml::events::Event;
use tracing::debug;

use crate::{DiscoveredAccount, DiscoveryError, DiscoverySource};

/// Try the three autoconfig URLs in order. Returns the first that
/// parses to a usable IMAP+SMTP pair.
pub async fn discover(domain: &str, email: &str) -> Result<DiscoveredAccount, DiscoveryError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent("unkai-mail-discovery/0.1")
        .build()
        .map_err(|e| DiscoveryError::Network(format!("build http client: {e}")))?;

    let candidates = [
        (
            format!("https://autoconfig.{domain}/mail/config-v1.1.xml?emailaddress={email}"),
            DiscoverySource::AutoconfigDomain,
        ),
        (
            format!(
                "https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress={email}"
            ),
            DiscoverySource::AutoconfigDomain,
        ),
        (
            format!("https://autoconfig.thunderbird.net/v1.1/{domain}"),
            DiscoverySource::AutoconfigIspdb,
        ),
    ];

    for (url, source) in candidates {
        debug!("Trying autoconfig URL: {url}");
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(body) => match parse(&body, source) {
                    Ok(found) => {
                        debug!("Autoconfig hit: {url}");
                        return Ok(found);
                    }
                    Err(e) => debug!("autoconfig parse failed for {url}: {e}"),
                },
                Err(e) => debug!("autoconfig body read failed for {url}: {e}"),
            },
            Ok(resp) => debug!("autoconfig {url} returned HTTP {}", resp.status()),
            Err(e) => debug!("autoconfig {url} request failed: {e}"),
        }
    }

    Err(DiscoveryError::NotFound)
}

/// Pull the first `incomingServer type="imap"` and `outgoingServer
/// type="smtp"` blocks out of an autoconfig XML document.
///
/// Hand-rolled with quick-xml's pull parser instead of full serde
/// deserialization — the schema has lots of fields we don't care
/// about (POP3, IMAP-with-OAUTH, several `displayName` variants),
/// and a streaming pass over the events is shorter than defining
/// matching structs for half of them. Returns the first complete
/// IMAP+SMTP pair found and ignores everything else.
fn parse(xml: &str, source: DiscoverySource) -> Result<DiscoveredAccount, DiscoveryError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut imap: Option<ServerEntry> = None;
    let mut smtp: Option<ServerEntry> = None;

    let mut current: Option<CurrentServer> = None;
    let mut text_target: Option<Field> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let tag = std::str::from_utf8(e.name().as_ref())
                    .map_err(|err| DiscoveryError::Parse(format!("bad utf8 tag: {err}")))?
                    .to_string();
                match tag.as_str() {
                    "incomingServer" | "outgoingServer" => {
                        let mut typ: Option<String> = None;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"type" {
                                typ = std::str::from_utf8(&attr.value).ok().map(|s| s.to_string());
                            }
                        }
                        current = match (tag.as_str(), typ.as_deref()) {
                            ("incomingServer", Some("imap")) => Some(CurrentServer {
                                kind: ServerKind::Imap,
                                entry: ServerEntry::default(),
                            }),
                            ("outgoingServer", Some("smtp")) => Some(CurrentServer {
                                kind: ServerKind::Smtp,
                                entry: ServerEntry::default(),
                            }),
                            // Skip POP3 / EWS / unknown server elements.
                            _ => None,
                        };
                    }
                    "hostname" if current.is_some() => text_target = Some(Field::Hostname),
                    "port" if current.is_some() => text_target = Some(Field::Port),
                    "socketType" if current.is_some() => text_target = Some(Field::SocketType),
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(target) = text_target.take()
                    && let Some(curr) = current.as_mut()
                {
                    let text = t
                        .xml_content()
                        .map_err(|e| DiscoveryError::Parse(format!("unescape text: {e}")))?
                        .to_string();
                    match target {
                        Field::Hostname => curr.entry.hostname = Some(text),
                        Field::Port => curr.entry.port = text.parse().ok(),
                        Field::SocketType => curr.entry.socket = Some(text),
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let tag = std::str::from_utf8(name.as_ref())
                    .map_err(|err| DiscoveryError::Parse(format!("bad utf8 end tag: {err}")))?;
                match tag {
                    "incomingServer" | "outgoingServer" => {
                        if let Some(curr) = current.take()
                            && curr.entry.is_complete()
                        {
                            match curr.kind {
                                ServerKind::Imap if imap.is_none() => imap = Some(curr.entry),
                                ServerKind::Smtp if smtp.is_none() => smtp = Some(curr.entry),
                                _ => {}
                            }
                        }
                    }
                    "hostname" | "port" | "socketType" => text_target = None,
                    _ => {}
                }
            }
            Err(e) => {
                return Err(DiscoveryError::Parse(format!(
                    "xml read error at {}: {e}",
                    reader.error_position()
                )));
            }
            _ => {}
        }
        buf.clear();
    }

    match (imap, smtp) {
        (Some(i), Some(s)) => Ok(DiscoveredAccount {
            imap_host: i.hostname.unwrap_or_default(),
            imap_port: i.port.unwrap_or(993),
            imap_tls: tls_for(i.socket.as_deref(), i.port),
            smtp_host: s.hostname.unwrap_or_default(),
            smtp_port: s.port.unwrap_or(587),
            smtp_tls: tls_for(s.socket.as_deref(), s.port),
            source,
        }),
        _ => Err(DiscoveryError::NotFound),
    }
}

/// Map a `socketType` element value (or fall back to the port) to
/// "is this implicit TLS?". `SSL` and explicit port 465/993 → yes;
/// `STARTTLS` / `plain` / port 587/143 → no.
fn tls_for(socket: Option<&str>, port: Option<u16>) -> bool {
    if let Some(s) = socket {
        let s = s.to_ascii_lowercase();
        if s == "ssl" || s == "tls" {
            return true;
        }
        if s == "starttls" || s == "plain" {
            return false;
        }
    }
    matches!(port, Some(465) | Some(993))
}

#[derive(Default, Debug)]
struct ServerEntry {
    hostname: Option<String>,
    port: Option<u16>,
    socket: Option<String>,
}

impl ServerEntry {
    fn is_complete(&self) -> bool {
        self.hostname.is_some() && self.port.is_some()
    }
}

#[derive(Debug)]
enum ServerKind {
    Imap,
    Smtp,
}

struct CurrentServer {
    kind: ServerKind,
    entry: ServerEntry,
}

enum Field {
    Hostname,
    Port,
    SocketType,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-world-shaped autoconfig XML — based on what Mozilla's
    /// ISP database returns for common providers, trimmed to the
    /// fields we read.
    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<clientConfig version="1.1">
  <emailProvider id="example.com">
    <domain>example.com</domain>
    <displayName>Example Mail</displayName>
    <displayShortName>Example</displayShortName>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <incomingServer type="pop3">
      <hostname>pop.example.com</hostname>
      <port>995</port>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    #[test]
    fn parses_imap_and_smtp() {
        let got = parse(SAMPLE, DiscoverySource::AutoconfigIspdb).unwrap();
        assert_eq!(got.imap_host, "imap.example.com");
        assert_eq!(got.imap_port, 993);
        assert!(got.imap_tls);
        assert_eq!(got.smtp_host, "smtp.example.com");
        assert_eq!(got.smtp_port, 587);
        assert!(!got.smtp_tls);
        assert_eq!(got.source, DiscoverySource::AutoconfigIspdb);
    }

    #[test]
    fn missing_imap_is_not_found() {
        // SMTP-only config is unusable for an inbox client.
        let xml = SAMPLE.replace(
            "<incomingServer type=\"imap\">",
            "<incomingServer type=\"unsupported\">",
        );
        let err = parse(&xml, DiscoverySource::AutoconfigIspdb).unwrap_err();
        assert!(matches!(err, DiscoveryError::NotFound));
    }
}
