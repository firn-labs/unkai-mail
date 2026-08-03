//! Shared Nextcloud / DAV-source glue for the groupware tools
//! (#441).
//!
//! The crate-level APIs in `unkai-caldav` / `unkai-carddav` /
//! `unkai-nextcloud` are stateless functions taking `(server_url,
//! username, app_password, trusted_certs)`.  This module supplies
//! the same credential glue the Tauri commands use — account
//! records from the SQLCipher store (`nextcloud_store`), app
//! passwords from the OS keychain (`credentials`) — plus the
//! feature-availability checks the registry gates tools on.
//!
//! ## Feature availability
//!
//! Availability comes from the `NextcloudCapabilities` snapshot
//! cached on each account record at connect time (synthetic for
//! generic-DAV / local sources, #413).  A missing snapshot on a
//! real Nextcloud counts as "has DAV" (every Nextcloud ships
//! CalDAV/CardDAV) but *not* as "has Talk" — Talk is an optional
//! app and advertising tools we can't back would send agents into
//! dead ends.

use rmcp::ErrorData;
use rmcp::model::JsonObject;
use unkai_core::models::{NextcloudAccount, NextcloudCapabilities};

use crate::registry::{NextcloudFeature, ToolContext};
use crate::util::{internal, invalid, optional_str};

/// Load every connected DAV / Nextcloud source.  Errors degrade to
/// an empty list (logged) — for availability checks that reads as
/// "nothing connected", which fails safe on the advertising side.
pub fn load_nc_accounts(cache: &unkai_store::Cache) -> Vec<NextcloudAccount> {
    match unkai_store::nextcloud_store::load_accounts(cache) {
        Ok(accounts) => accounts,
        Err(e) => {
            tracing::warn!("MCP: could not load Nextcloud accounts: {e}");
            Vec::new()
        }
    }
}

/// Does `account` offer `feature`?
pub(crate) fn account_has_feature(account: &NextcloudAccount, feature: NextcloudFeature) -> bool {
    let caps: Option<&NextcloudCapabilities> = account.capabilities.as_ref();
    match feature {
        // DAV features: trust the snapshot when present; a real
        // Nextcloud without one still counts (DAV is core there),
        // a DAV/local source without one doesn't (the synthetic
        // snapshot is exactly how those record what they offer).
        NextcloudFeature::Contacts => caps.map(|c| c.carddav).unwrap_or(account.is_nextcloud()),
        NextcloudFeature::Calendar => caps.map(|c| c.caldav).unwrap_or(account.is_nextcloud()),
        // Talk is an optional OCS app — no snapshot, no Talk.
        NextcloudFeature::Talk => account.is_nextcloud() && caps.map(|c| c.talk).unwrap_or(false),
    }
}

/// Does *any* connected source offer `feature`?
pub fn feature_available(accounts: &[NextcloudAccount], feature: NextcloudFeature) -> bool {
    accounts.iter().any(|a| account_has_feature(a, feature))
}

fn feature_noun(feature: NextcloudFeature) -> &'static str {
    match feature {
        NextcloudFeature::Contacts => "contacts (CardDAV)",
        NextcloudFeature::Calendar => "calendars (CalDAV)",
        NextcloudFeature::Talk => "Nextcloud Talk",
    }
}

/// Resolve which connected source a groupware tool call targets.
///
/// Honours an optional `nextcloud_account_id` parameter; when the
/// caller omits it and exactly one connected source offers the
/// feature (the overwhelmingly common setup), that one is used.
/// Ambiguity is an error that lists the candidate ids so the agent
/// can retry with an explicit choice instead of guessing.
pub(crate) fn resolve_nc_account(
    ctx: &ToolContext,
    args: &Option<JsonObject>,
    feature: NextcloudFeature,
) -> Result<NextcloudAccount, ErrorData> {
    let candidates: Vec<NextcloudAccount> = load_nc_accounts(&ctx.cache)
        .into_iter()
        .filter(|a| account_has_feature(a, feature))
        .collect();
    let noun = feature_noun(feature);

    match optional_str(args, "nextcloud_account_id")? {
        Some(id) => candidates.into_iter().find(|a| a.id == id).ok_or_else(|| {
            invalid(format!(
                "no connected source with id '{id}' offers {noun} — omit \
                 nextcloud_account_id to use the default"
            ))
        }),
        None => {
            let mut candidates = candidates;
            match candidates.len() {
                0 => Err(invalid(format!(
                    "no connected Nextcloud / DAV source offers {noun} — the user can \
                     connect one in Unkai Mail's settings"
                ))),
                1 => Ok(candidates.remove(0)),
                _ => {
                    let ids: Vec<String> = candidates
                        .iter()
                        .map(|a| format!("'{}' ({})", a.id, a.server_url))
                        .collect();
                    Err(invalid(format!(
                        "multiple connected sources offer {noun}; pass \
                         nextcloud_account_id as one of: {}",
                        ids.join(", ")
                    )))
                }
            }
        }
    }
}

/// App password for a remote source, from the OS keychain.  Local
/// sources have no keychain entry at all — callers must branch on
/// `is_local()` *before* asking for credentials.
pub(crate) fn nc_password(account: &NextcloudAccount) -> Result<String, ErrorData> {
    unkai_store::credentials::get_nextcloud_password(&account.id).map_err(|e| {
        internal(format!(
            "could not read the Nextcloud app password from the OS keychain: {e}"
        ))
    })
}

/// Resolved CardDAV addressbook-home URL for any remote source
/// (#413): generic DAV records store the RFC 6764-resolved home;
/// Nextcloud derives it from the fixed server layout.  Never
/// called for local sources (they have no home).
pub(crate) fn carddav_home_of(account: &NextcloudAccount) -> String {
    match &account.carddav_home {
        Some(home) => home.clone(),
        None => format!(
            "{}/remote.php/dav/addressbooks/users/{}/",
            account.server_url.trim_end_matches('/'),
            account.username
        ),
    }
}

/// Fallback `(email, display_name)` for `ORGANIZER` when the OCS
/// profile can't be asked: prefer `username` when it's already an
/// email, else synthesise `username@server-host`.  Unrouteable on
/// the public internet but satisfies Sabre's "ATTENDEE without
/// ORGANIZER is 403" check so the PUT itself succeeds.  Mirrors
/// the in-app calendar-write flow.
pub(crate) fn organizer_local(account: &NextcloudAccount) -> (String, Option<String>) {
    let email = if account.username.contains('@') {
        account.username.clone()
    } else {
        let host = account
            .server_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .split('/')
            .next()
            .unwrap_or("nextcloud.local");
        format!("{}@{}", account.username, host)
    };
    (email, account.display_name.clone())
}

/// Resolve the `(email, display_name)` to write into `ORGANIZER`
/// for an outbound VEVENT — same strategy as the in-app flow: with
/// attendees present, ask `/ocs/v2.php/cloud/user` for the primary
/// email (what Nextcloud's iMIP Mail Provider keys against) and
/// fall back to [`organizer_local`] when the lookup fails; without
/// attendees skip the network round-trip entirely (the scheduling
/// plugin won't fire, ORGANIZER is just metadata).
pub(crate) async fn resolve_organizer(
    account: &NextcloudAccount,
    app_password: &str,
    has_attendees: bool,
) -> (String, Option<String>) {
    if !has_attendees || !account.is_nextcloud() {
        return organizer_local(account);
    }
    match unkai_nextcloud::fetch_current_user(
        &account.server_url,
        &account.username,
        app_password,
        &account.trusted_certs,
    )
    .await
    {
        Ok(profile) => {
            if let Some(email) = profile.email {
                let name = profile
                    .display_name
                    .or_else(|| account.display_name.clone());
                return (email, name);
            }
            tracing::warn!(
                "MCP: Nextcloud user has no email set in Personal info — \
                 iMIP will fall back to the system mailer"
            );
        }
        Err(e) => tracing::warn!("MCP: OCS user lookup failed, using fallback ORGANIZER: {e}"),
    }
    organizer_local(account)
}

/// Test fixtures shared by the groupware tool modules' tests.
#[cfg(test)]
pub(crate) mod test_support {
    use unkai_core::models::{DavSourceKind, NextcloudAccount, NextcloudCapabilities};

    pub(crate) fn nc_account(
        id: &str,
        kind: DavSourceKind,
        caps: Option<NextcloudCapabilities>,
    ) -> NextcloudAccount {
        NextcloudAccount {
            id: id.into(),
            server_url: "https://cloud.example.com".into(),
            username: "jane".into(),
            display_name: Some("Jane Smith".into()),
            capabilities: caps,
            trusted_certs: Vec::new(),
            kind,
            carddav_home: None,
            caldav_home: None,
        }
    }

    pub(crate) fn caps(talk: bool, caldav: bool, carddav: bool) -> NextcloudCapabilities {
        NextcloudCapabilities {
            talk,
            caldav,
            carddav,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{caps, nc_account};
    use super::*;
    use unkai_core::models::DavSourceKind;

    #[test]
    fn dav_features_follow_the_snapshot() {
        let a = nc_account(
            "a",
            DavSourceKind::Nextcloud,
            Some(caps(false, true, false)),
        );
        assert!(account_has_feature(&a, NextcloudFeature::Calendar));
        assert!(!account_has_feature(&a, NextcloudFeature::Contacts));
        assert!(!account_has_feature(&a, NextcloudFeature::Talk));
    }

    #[test]
    fn nextcloud_without_snapshot_counts_for_dav_but_not_talk() {
        let a = nc_account("a", DavSourceKind::Nextcloud, None);
        assert!(account_has_feature(&a, NextcloudFeature::Calendar));
        assert!(account_has_feature(&a, NextcloudFeature::Contacts));
        assert!(!account_has_feature(&a, NextcloudFeature::Talk));
    }

    #[test]
    fn local_source_only_offers_what_its_synthetic_snapshot_says() {
        let a = nc_account("a", DavSourceKind::Local, Some(caps(false, true, true)));
        assert!(account_has_feature(&a, NextcloudFeature::Calendar));
        assert!(account_has_feature(&a, NextcloudFeature::Contacts));
        assert!(!account_has_feature(&a, NextcloudFeature::Talk));
        let bare = nc_account("b", DavSourceKind::Local, None);
        assert!(!account_has_feature(&bare, NextcloudFeature::Calendar));
    }

    #[test]
    fn organizer_local_synthesises_from_the_host() {
        let a = nc_account("a", DavSourceKind::Nextcloud, None);
        assert_eq!(organizer_local(&a).0, "jane@cloud.example.com");
        let mut b = nc_account("b", DavSourceKind::Nextcloud, None);
        b.username = "jane@corp.example".into();
        assert_eq!(organizer_local(&b).0, "jane@corp.example");
    }
}
