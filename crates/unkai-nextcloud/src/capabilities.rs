//! Capability detection â€” what does this Nextcloud server actually support?
//!
//! Nextcloud returns a deeply nested capability tree from
//! `/ocs/v2.php/cloud/capabilities`. The structure changes between
//! versions and apps, so we parse only the fields we care about and
//! ignore the rest via `#[serde(default)]` â€” future NC versions that
//! move things around won't break auth.
//!
//! We detect four apps:
//!
//! | Flag    | Marker in the JSON tree                                |
//! |---------|--------------------------------------------------------|
//! | `talk`  | `capabilities.spreed` present (Nextcloud Talk's app id)|
//! | `files` | `capabilities.files` present                           |
//! | `caldav`| `capabilities.dav.chunking` or `bulkupload` (as proxy) |
//! | `carddav`| same â€” CalDAV/CardDAV are both under `dav`            |
//!
//! The CalDAV/CardDAV detection is a heuristic: `/ocs/v2.php/cloud/capabilities`
//! doesn't list them explicitly, but they're bundled with Nextcloud core
//! and any server exposing the `dav` capability supports both. If a
//! future issue needs stricter detection (e.g. probing the DAV
//! endpoints), we can refine here without touching the UI.

use serde::Deserialize;

use unkai_core::UnkaiError;
use unkai_core::models::NextcloudCapabilities;
use unkai_core::models::TrustedCert;

use crate::client;

// â”€â”€ Wire format â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
// These mirror Nextcloud's JSON literally. Everything is Option so a
// missing field is just "not supported", never an error.

#[derive(Debug, Deserialize)]
struct OcsEnvelope<T> {
    ocs: OcsBody<T>,
}

#[derive(Debug, Deserialize)]
struct OcsBody<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct CapabilitiesData {
    version: Option<Version>,
    capabilities: Capabilities,
}

#[derive(Debug, Deserialize)]
struct Version {
    /// Dotted version string, e.g. "28.0.4"
    string: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Capabilities {
    /// Nextcloud Talk uses the internal app id `spreed`.
    spreed: Option<serde_json::Value>,
    /// Files app (usually always present).
    files: Option<serde_json::Value>,
    /// DAV capabilities â€” presence implies CalDAV + CardDAV are reachable.
    dav: Option<serde_json::Value>,
    /// Nextcloud Office / Collabora exposes its capability block
    /// under the app id `richdocuments`. We don't read any of the
    /// inner fields â€” presence alone is the signal that the editor
    /// URL flow (`apps/richdocuments/index.json`) will work.
    richdocuments: Option<serde_json::Value>,
    /// Nextcloud Notes app id.  Presence alone is the signal that
    /// `/index.php/apps/notes/api/v1/notes` is reachable.
    notes: Option<serde_json::Value>,
    /// Nextcloud Tasks app id.  The Tasks app reuses CalDAV (VTODO)
    /// for storage, so the chip is purely informational â€” it tells
    /// the user the server has Tasks installed alongside its
    /// calendars.
    tasks: Option<serde_json::Value>,
}

/// Query `/ocs/v2.php/cloud/capabilities` and map it to our flat
/// `NextcloudCapabilities` shape.
///
/// Uses Basic auth with the app password obtained from Login Flow v2.
pub async fn fetch_capabilities(
    server_url: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<NextcloudCapabilities, UnkaiError> {
    let server = client::normalize_server_url(server_url);
    let url = format!("{server}/ocs/v2.php/cloud/capabilities?format=json");
    tracing::debug!("Fetching Nextcloud capabilities from {url}");

    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("capabilities request failed: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UnkaiError::Auth(
            "Nextcloud rejected app password (revoked or expired)".into(),
        ));
    }
    if !status.is_success() {
        return Err(UnkaiError::Nextcloud(format!(
            "capabilities returned HTTP {status}"
        )));
    }

    let env: OcsEnvelope<CapabilitiesData> = resp
        .json()
        .await
        .map_err(|e| UnkaiError::Protocol(format!("capabilities bad JSON: {e}")))?;

    // Tasks (and older Notes) don't publish capability blocks under
    // /cloud/capabilities â€” they only register a navigation entry
    // and a CalDAV / REST endpoint.  Hit /cloud/navigation/apps as a
    // fallback so the chip flips on for any server where the user
    // can actually open the app.  Best-effort: if the call 404s on
    // an ancient NC version we just leave the navigation set empty
    // and fall back to whatever the capabilities tree already told
    // us.
    let nav_apps = fetch_navigation_apps(&server, username, app_password, trusted_certs)
        .await
        .unwrap_or_default();
    tracing::debug!("Nextcloud navigation/apps ids: {nav_apps:?}");
    // Match navigation entries case-insensitively and on
    // substring â€” the Tasks app has been registered under at
    // least three ids over the years (`tasks`, `tasks-app`, and
    // a versioned id on Nextcloud Hub releases) so a strict
    // equality check misses real installs.
    let nav_match = |needle: &str| {
        nav_apps
            .iter()
            .any(|a| a.to_ascii_lowercase().contains(needle))
    };

    // Final fallback: probe the app's web entry point.  Both
    // Notes and Tasks expose `/index.php/apps/<id>/` once the
    // app is enabled for the current user, regardless of whether
    // they publish a capability block or a navigation entry.  A
    // 200 / 302 / 401 means "the route exists" â€” which is enough
    // to claim the chip; 404 is the only definitive negative.
    let notes_present = env.ocs.data.capabilities.notes.is_some()
        || nav_match("notes")
        || probe_app_route(&server, username, app_password, "notes", trusted_certs)
            .await
            .unwrap_or(false);
    let tasks_present = env.ocs.data.capabilities.tasks.is_some()
        || nav_match("tasks")
        || probe_app_route(&server, username, app_password, "tasks", trusted_certs)
            .await
            .unwrap_or(false);

    let dav_present = env.ocs.data.capabilities.dav.is_some();
    let caps = NextcloudCapabilities {
        version: env.ocs.data.version.and_then(|v| v.string),
        talk: env.ocs.data.capabilities.spreed.is_some(),
        files: env.ocs.data.capabilities.files.is_some(),
        caldav: dav_present,
        carddav: dav_present,
        office: env.ocs.data.capabilities.richdocuments.is_some(),
        notes: notes_present,
        tasks: tasks_present,
    };
    tracing::info!(
        "Nextcloud capabilities: version={:?} talk={} files={} dav={} office={} notes={} tasks={}",
        caps.version,
        caps.talk,
        caps.files,
        dav_present,
        caps.office,
        caps.notes,
        caps.tasks,
    );
    Ok(caps)
}

/// Hit `/ocs/v2.php/cloud/navigation/apps?format=json` and return
/// the `id` of every navigation entry the user can see.  Used as a
/// fallback signal for apps that don't publish a `capabilities`
/// block (notably Tasks, which only registers a CalDAV VTODO
/// provider + a nav entry).  A 404 / non-success / parse error
/// resolves to an empty list â€” the caller treats that as "no
/// extra signal", not as a hard auth failure.
async fn fetch_navigation_apps(
    server: &str,
    username: &str,
    app_password: &str,
    trusted_certs: &[TrustedCert],
) -> Result<Vec<String>, UnkaiError> {
    let url = format!("{server}/ocs/v2.php/cloud/navigation/apps?format=json");
    let http = client::build(trusted_certs)?;
    let resp = http
        .get(&url)
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("navigation/apps request failed: {e}")))?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let env: OcsEnvelope<Vec<NavApp>> = resp
        .json()
        .await
        .map_err(|e| UnkaiError::Protocol(format!("navigation/apps bad JSON: {e}")))?;
    Ok(env.ocs.data.into_iter().filter_map(|a| a.id).collect())
}

#[derive(Debug, Deserialize)]
struct NavApp {
    #[serde(default)]
    id: Option<String>,
}

/// Probe `<server>/index.php/apps/<app_id>/` and return `true` when
/// the response status looks like the route exists.  A 200 / 302 /
/// 303 means the SPA loaded (or redirected to its login flow); a
/// 401 means the route is gated by auth (still a positive signal â€”
/// the app is wired up); only a 404 / network error counts as
/// "definitely not installed".  Belt-and-suspenders detection for
/// apps that don't expose a capability block AND aren't in the
/// navigation list (Tasks under some Hub releases, custom NC
/// configurations that hide nav entries).
async fn probe_app_route(
    server: &str,
    username: &str,
    app_password: &str,
    app_id: &str,
    trusted_certs: &[TrustedCert],
) -> Result<bool, UnkaiError> {
    let url = format!("{server}/index.php/apps/{app_id}/");
    let http = client::build(trusted_certs)?;
    let resp = http
        .head(&url)
        .header("OCS-APIRequest", "true")
        .basic_auth(username, Some(app_password))
        .send()
        .await
        .map_err(|e| UnkaiError::Network(format!("app probe failed: {e}")))?;
    let s = resp.status().as_u16();
    Ok(matches!(s, 200 | 302 | 303 | 401))
}

// â”€â”€ Tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal but realistic slice of what Nextcloud 28 returns.
    const SAMPLE: &str = r#"{
      "ocs": {
        "data": {
          "version": { "string": "28.0.4" },
          "capabilities": {
            "spreed": { "features": [] },
            "files":  { "bigfilechunking": true },
            "dav":    { "chunking": "1.0" },
            "notes":  { "api_version": ["1.3"] },
            "tasks":  {}
          }
        }
      }
    }"#;

    #[test]
    fn parses_full_caps() {
        let env: OcsEnvelope<CapabilitiesData> = serde_json::from_str(SAMPLE).unwrap();
        assert!(env.ocs.data.capabilities.spreed.is_some());
        assert!(env.ocs.data.capabilities.files.is_some());
        assert!(env.ocs.data.capabilities.dav.is_some());
        assert!(env.ocs.data.capabilities.notes.is_some());
        assert!(env.ocs.data.capabilities.tasks.is_some());
    }

    #[test]
    fn missing_apps_are_false() {
        let json = r#"{
          "ocs": { "data": { "capabilities": {} } }
        }"#;
        let env: OcsEnvelope<CapabilitiesData> = serde_json::from_str(json).unwrap();
        assert!(env.ocs.data.capabilities.spreed.is_none());
        assert!(env.ocs.data.capabilities.files.is_none());
        assert!(env.ocs.data.capabilities.dav.is_none());
        assert!(env.ocs.data.capabilities.notes.is_none());
        assert!(env.ocs.data.capabilities.tasks.is_none());
    }
}
