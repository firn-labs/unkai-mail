//! Small URL guards used by the protocol crates.
//!
//! These exist to make sure we never accidentally send credentials
//! over an unencrypted transport.  IMAP, SMTP, JMAP, CalDAV and
//! CardDAV all carry HTTP Basic auth (or comparable) headers in the
//! request itself; if the URL turned out to be `http://` instead
//! of `https://` (server misconfig, autoconfig fallback bug, a
//! freshly-typed account where the user dropped the `s`) we'd leak
//! the password in cleartext.  Calling `ensure_https()` before any
//! authenticated request short-circuits with a clear error instead
//! of trusting the URL.

use crate::UnkaiError;

/// Reject any URL that doesn't carry the `https://` scheme — with a
/// narrow exemption for HTTP loopback (`127.0.0.1`, `[::1]`,
/// `localhost`).
///
/// This is a defence-in-depth guard.  Account discovery and the
/// autoconfig flow already filter out HTTP endpoints, so under
/// normal use this never trips — but a misconfigured Nextcloud
/// instance, a typo in a manually-entered server URL, or a future
/// regression in discovery would otherwise silently put credentials
/// over plaintext HTTP.  Refuse to proceed instead.
///
/// The loopback exemption exists because:
///   * mock servers in `tests/` bind to `http://127.0.0.1:<port>`
///     and the integration tests pass real credentials to them.
///     Loopback never leaves the machine, so cleartext is safe.
///   * Developers running a local Nextcloud instance for testing
///     also commonly do so over plain HTTP on localhost.
pub fn ensure_https(url: &str) -> Result<(), UnkaiError> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        // Cheap host extraction — everything up to the first `/`,
        // `?`, `#`, or end of string.  Strip an `userinfo@` prefix
        // and a `:port` suffix on the way through.
        let host_with_port = rest
            .split(|c| c == '/' || c == '?' || c == '#')
            .next()
            .unwrap_or("");
        let host_with_port = host_with_port.rsplit('@').next().unwrap_or(host_with_port);
        // IPv6 literals come bracketed: `[::1]:8080`.
        let host = if let Some(end) = host_with_port.strip_prefix('[') {
            end.split(']').next().unwrap_or("")
        } else {
            host_with_port.split(':').next().unwrap_or("")
        };
        if matches!(host, "127.0.0.1" | "::1" | "localhost") {
            return Ok(());
        }
    }
    Err(UnkaiError::Network(format!(
        "refusing to send credentials over a non-HTTPS URL: {url}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_passes() {
        assert!(ensure_https("https://example.com/path").is_ok());
    }

    #[test]
    fn http_loopback_passes() {
        assert!(ensure_https("http://127.0.0.1:8080/path").is_ok());
        assert!(ensure_https("http://localhost:3000/").is_ok());
        assert!(ensure_https("http://[::1]:8080/api").is_ok());
    }

    #[test]
    fn http_public_rejected() {
        let err = ensure_https("http://example.com/path").unwrap_err();
        assert!(format!("{err}").contains("non-HTTPS"));
    }

    #[test]
    fn http_userinfo_does_not_spoof_loopback() {
        // `userinfo@host` — the userinfo is `127.0.0.1`, host is
        // `evil.com`.  Must reject.
        assert!(ensure_https("http://127.0.0.1@evil.com/path").is_err());
    }

    #[test]
    fn other_schemes_rejected() {
        assert!(ensure_https("ftp://example.com").is_err());
        assert!(ensure_https("ws://example.com").is_err());
        assert!(ensure_https("file:///etc/passwd").is_err());
    }

    #[test]
    fn empty_rejected() {
        assert!(ensure_https("").is_err());
    }
}
