//! Request guards for the MCP endpoint (#438).
//!
//! Every HTTP request has to clear four gates, in this order:
//!
//! 1. **Host validation** — the `Host` header must name a loopback
//!    authority.  This is the DNS-rebinding defence: a malicious
//!    web page can make the victim's browser resolve
//!    `attacker.com` to `127.0.0.1` and then issue requests that
//!    *reach* our listener, but the browser still sends
//!    `Host: attacker.com`, which we reject.
//! 2. **Origin rejection** — any request carrying an `Origin`
//!    header is refused outright.  Browsers attach `Origin` to
//!    every cross-origin request (and same-origin `POST`s);
//!    legitimate MCP clients are native processes that never send
//!    one.  Refusing the header entirely is simpler and strictly
//!    safer than maintaining an allow-list of origins for an
//!    endpoint no browser should ever talk to.
//! 3. **Bearer-token auth** — `Authorization: Bearer <token>`
//!    compared in constant time against the single app-wide token.
//!    No token generated yet ⇒ every request is 401, so an
//!    enabled-but-unconfigured server exposes nothing.
//! 4. **Vault lock** — while the SQLCipher cache is FIDO-locked,
//!    everything answers 503 with a machine-readable
//!    `vault_locked` error.  Checked *after* auth so an
//!    unauthenticated caller can't probe whether the vault is
//!    open.
//!
//! rmcp's `StreamableHttpService` performs its own loopback `Host`
//! validation as well (kept enabled) — gate 1 here is deliberate
//! defence-in-depth so our tests pin the behaviour and rejected
//! requests get a clear JSON body instead of an opaque 403.

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use subtle::ConstantTimeEq;
use unkai_core::UnkaiError;

use crate::GuardState;

/// Generate a fresh bearer token: 32 CSPRNG bytes, base64url
/// without padding (43 chars, URL- and header-safe).  The caller
/// is responsible for persisting it to the OS keychain — this
/// function is pure so tests can use it without touching keychain
/// state.
pub fn generate_token() -> Result<String, UnkaiError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| UnkaiError::Storage(format!("CSPRNG failure generating MCP token: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Constant-time equality on the raw bytes.  `subtle::ct_eq`
/// refuses to leak *which* byte mismatched through timing; the
/// length itself is not secret (every generated token is 43
/// chars), so the early length check is fine.
fn token_matches(expected: &str, presented: &str) -> bool {
    expected.len() == presented.len() && bool::from(expected.as_bytes().ct_eq(presented.as_bytes()))
}

/// True when the `Host` header names a loopback authority
/// (`127.0.0.1`, `localhost`, or `::1`), with or without a port.
fn host_is_loopback(host: &str) -> bool {
    let host = host.trim();
    // Split off an optional `:port`.  IPv6 authorities bracket the
    // address (`[::1]:52226`), so strip the brackets after
    // splitting at the *last* colon outside them.
    let bare = if let Some(rest) = host.strip_prefix('[') {
        // `[::1]` or `[::1]:52226` — everything up to the closing
        // bracket is the address.
        rest.split(']').next().unwrap_or_default()
    } else {
        // At most one colon in a bracketless authority (IPv4 or
        // hostname); more than one means a malformed / raw-IPv6
        // value we have no reason to accept.
        match host.split(':').collect::<Vec<_>>()[..] {
            [h] | [h, _] => h,
            _ => return false,
        }
    };
    bare.eq_ignore_ascii_case("localhost") || bare == "127.0.0.1" || bare == "::1"
}

/// Small JSON error body so MCP clients and curl users see *why*
/// they were refused instead of a bare status line.
fn refuse(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({ "error": code, "message": message }).to_string();
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    if status == StatusCode::UNAUTHORIZED {
        // RFC 6750: tell the client which auth scheme this
        // endpoint expects.
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer realm=\"unkai-mail-mcp\""),
        );
    }
    response
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// The axum middleware enforcing the four gates documented at the
/// top of this module.  Mounted over the whole MCP router.
pub async fn request_guard(State(guard): State<GuardState>, req: Request, next: Next) -> Response {
    // Gate 1: Host must be loopback (DNS-rebinding defence).
    match req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
    {
        Some(host) if host_is_loopback(host) => {}
        _ => {
            tracing::warn!("MCP request refused: non-loopback or missing Host header");
            return refuse(
                StatusCode::FORBIDDEN,
                "invalid_host",
                "The MCP server only accepts requests addressed to localhost.",
            );
        }
    }

    // Gate 2: no browser may talk to this endpoint.
    if req.headers().contains_key(header::ORIGIN) {
        tracing::warn!("MCP request refused: Origin header present (browser request?)");
        return refuse(
            StatusCode::FORBIDDEN,
            "origin_forbidden",
            "The MCP server does not accept browser (Origin-bearing) requests.",
        );
    }

    // Gate 3: bearer token, constant-time.
    let authorized = match (
        guard.token.read().await.as_deref(),
        bearer_token(req.headers()),
    ) {
        (Some(expected), Some(presented)) => token_matches(expected, presented),
        // No token generated yet, or none presented — either way
        // the request is unauthenticated.
        _ => false,
    };
    if !authorized {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Missing or invalid bearer token. Generate one in Unkai Mail's AI settings.",
        );
    }

    // Gate 4: the vault must be unlocked before any tool can serve
    // data.  Checked last so lock state is never disclosed to
    // unauthenticated callers.
    if guard.cache.is_locked() {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "vault_locked",
            "Unkai Mail's encrypted vault is locked. Unlock the app, then retry.",
        );
    }

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_distinct_and_url_safe() {
        let a = generate_token().expect("token");
        let b = generate_token().expect("token");
        assert_ne!(a, b);
        // 32 bytes → ceil(32 * 4 / 3) = 43 base64url chars, no padding.
        assert_eq!(a.len(), 43);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn token_comparison() {
        assert!(token_matches("abc123", "abc123"));
        assert!(!token_matches("abc123", "abc124"));
        // Length mismatch must not panic and must not match.
        assert!(!token_matches("abc123", "abc12"));
        assert!(!token_matches("", "abc"));
    }

    #[test]
    fn loopback_hosts_accepted() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:52226",
            "localhost",
            "LOCALHOST:52226",
            "[::1]",
            "[::1]:52226",
        ] {
            assert!(host_is_loopback(host), "{host} should be accepted");
        }
    }

    #[test]
    fn non_loopback_hosts_rejected() {
        for host in [
            "example.com",
            "example.com:52226",
            "192.168.1.10:52226",
            "127.0.0.1.evil.example",
            "",
            "[::2]",
        ] {
            assert!(!host_is_loopback(host), "{host} should be rejected");
        }
    }
}
