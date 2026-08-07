//! App preferences, the vault gate, settings sync, MCP, and themes.
//!
//! Mirrors `ui/src/lib/api/settings.ts`.

use serde::Deserialize;
use serde::Serialize;
use unkai_core::UnkaiError;
use unkai_core::models::AppSettings;
use unkai_core::models::CustomTheme;
use unkai_mcp::McpServer;
use unkai_mcp::McpServerStatus;
use unkai_store::Cache;
use unkai_store::app_settings;
use unkai_store::credentials;
use unkai_store::settings_bundle;
use unkai_store::settings_sync;

use crate::notify::UiNotifier;
use crate::state::{SettingsSyncNotify, SharedLocalStorage, SharedSettings};
use crate::support::load_nextcloud_account;

// ── FIDO unlock (#164, Phase 1A) ──────────────────────────────
//
// These commands manage the wraps inside the keychain envelope.
// They don't yet replace the plain-mode startup path — registering
// keys is observable via the Settings UI, and the unlock-at-boot
// flow lands as a separate phase once the wrap/unwrap loop is
// hardware-verified.

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FidoCredentialView {
    /// `"fido_prf"` or `"passphrase"`.
    pub kind: String,
    pub credential_id: String,
    pub label: String,
    pub salt: String,
    pub created_at: i64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FidoStatusView {
    /// Always Some in plain / hybrid mode, None once the keychain
    /// is in FIDO-only mode (Phase 1B+).
    pub has_plain_key: bool,
    /// How many credentials the user has registered.
    pub credentials: Vec<FidoCredentialView>,
}

/// Snapshot of the keychain envelope.  Used by Settings to render
/// the "Hardware authentication" panel and (later) by the boot
/// path to decide whether to require an unlock before opening the
/// cache.
pub fn fido_status() -> Result<FidoStatusView, UnkaiError> {
    let env = unkai_store::cache::key::load_envelope()?;
    Ok(FidoStatusView {
        has_plain_key: env.plain_key.is_some(),
        credentials: env
            .wraps
            .into_iter()
            .map(|w| FidoCredentialView {
                kind: match w.kind {
                    unkai_store::fido::WrapKind::FidoPrf => "fido_prf".to_string(),
                    unkai_store::fido::WrapKind::Passphrase => "passphrase".to_string(),
                },
                credential_id: w.credential_id,
                label: w.label,
                salt: w.salt,
                created_at: w.created_at,
            })
            .collect(),
    })
}

/// Generate a fresh PRF salt for a new enrollment.  The frontend
/// supplies it as the `prf.eval.first` input to `navigator.
/// credentials.create` so the authenticator returns the matching
/// PRF output.
pub fn fido_generate_salt() -> Result<String, UnkaiError> {
    let salt = unkai_store::fido::generate_salt()?;
    Ok(unkai_store::fido::encode_b64(&salt))
}

/// Wrap the current master key under a freshly-registered FIDO
/// credential's PRF output.  Frontend has already called
/// WebAuthn `credentials.create` with the salt from
/// `fido_generate_salt`, received the credential id and the PRF
/// bytes back, and forwards them here for storage.
pub fn fido_enroll(
    credential_id_b64: String,
    salt_b64: String,
    prf_output_b64: String,
    label: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    use unkai_store::fido;
    let env = unkai_store::cache::key::load_envelope()?;
    // Same fallback as `fido_enroll_passphrase`: prefer the
    // envelope's plain key, fall back to the in-memory copy
    // when FIDO-only mode has cleared plain_key.
    let plain_hex = match env.plain_key.as_deref() {
        Some(hex) => hex.to_string(),
        None => cache.master_key_hex().ok_or_else(|| {
            UnkaiError::Auth(
                "Cannot enroll a credential while the database is locked — unlock first".into(),
            )
        })?,
    };
    let master_key = hex::decode(&plain_hex)
        .map_err(|e| UnkaiError::Storage(format!("master key hex decode: {e}")))?;
    let credential_id = fido::decode_b64(&credential_id_b64)?;
    let salt = fido::decode_b64(&salt_b64)?;
    let prf_output = fido::decode_b64(&prf_output_b64)?;
    let wrap = fido::wrap_master_key(
        fido::WrapKind::FidoPrf,
        &master_key,
        &prf_output,
        &credential_id,
        &salt,
        label,
    )?;
    unkai_store::cache::key::add_wrap(wrap)?;
    Ok(())
}

/// Wrap the current master key under a passphrase-derived AES key
/// (PBKDF2-HMAC-SHA-256, 720 000 iters).  Doubles as recovery
/// passphrase for Phase 1B and as the test path on platforms
/// where WebAuthn PRF isn't reachable yet (Linux WebKitGTK <
/// 2.46).  Salt + synthetic credential id are server-side
/// generated so the frontend never produces them.
pub fn fido_enroll_passphrase(
    passphrase: String,
    label: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    use unkai_store::fido::{self, WrapKind};
    if passphrase.trim().is_empty() {
        return Err(UnkaiError::Other("passphrase must not be empty".into()));
    }
    let mut env = unkai_store::cache::key::load_envelope()?;
    // Prefer the keychain envelope's plain key (pre-FIDO-only),
    // fall back to the in-memory copy that `unlock_with_*` stashes
    // on the Cache.  The fallback is what makes "Change passphrase"
    // work after the user has flipped Key Encryption on — by that
    // point envelope.plain_key is None and we'd otherwise refuse.
    let plain_hex = match env.plain_key.as_deref() {
        Some(hex) => hex.to_string(),
        None => cache.master_key_hex().ok_or_else(|| {
            UnkaiError::Auth(
                "Cannot enroll a passphrase while the database is locked — unlock first".into(),
            )
        })?,
    };
    let master_key = hex::decode(&plain_hex)
        .map_err(|e| UnkaiError::Storage(format!("master key hex decode: {e}")))?;
    let salt = fido::generate_salt()?;
    let id = fido::generate_passphrase_id()?;
    let aes_key = fido::derive_passphrase_key(&passphrase, &salt)?;
    let wrap = fido::wrap_master_key(
        WrapKind::Passphrase,
        &master_key,
        &aes_key,
        &id,
        &salt,
        label,
    )?;
    // Single-passphrase invariant: the recovery passphrase is a
    // role, not a per-device entry.  Drop any existing passphrase
    // wrap before adding the new one so re-enrolling cleanly
    // replaces the old one (and so add_wrap's credential-id
    // dedup never lets two passphrase wraps coexist with
    // different ids).
    env.wraps.retain(|w| w.kind != WrapKind::Passphrase);
    env.wraps.push(wrap);
    unkai_store::cache::key::save_envelope(&env)?;
    Ok(())
}

/// Test-only: verify a passphrase wraps unlock the master key.
/// Phase 1B will call this from the lock screen when the user
/// chooses passphrase unlock; today it lets users sanity-check
/// their passphrase entry on Linux without restructuring boot.
/// Returns `true` on success, `false` on a wrong passphrase /
/// no matching wrap, error on storage / crypto failure.
pub fn fido_verify_passphrase(passphrase: String) -> Result<bool, UnkaiError> {
    use unkai_store::fido::{self, WrapKind};
    let env = unkai_store::cache::key::load_envelope()?;
    for wrap in &env.wraps {
        if wrap.kind != WrapKind::Passphrase {
            continue;
        }
        let salt = fido::decode_b64(&wrap.salt)?;
        let aes_key = fido::derive_passphrase_key(&passphrase, &salt)?;
        if fido::unwrap_master_key(wrap, &aes_key).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Mirror of `fido_verify_passphrase` for FIDO PRF wraps.  The
/// frontend has already run WebAuthn `credentials.get` against
/// the credential's stored salt and forwards the PRF output
/// here.  Phase 1B's lock screen will use this; today it lets
/// you sanity-check that a registered hardware key still works.
pub fn fido_verify_prf(
    credential_id_b64: String,
    prf_output_b64: String,
) -> Result<bool, UnkaiError> {
    use unkai_store::fido::{self, WrapKind};
    let env = unkai_store::cache::key::load_envelope()?;
    let prf = fido::decode_b64(&prf_output_b64)?;
    for wrap in &env.wraps {
        if wrap.kind != WrapKind::FidoPrf {
            continue;
        }
        if wrap.credential_id != credential_id_b64 {
            continue;
        }
        if fido::unwrap_master_key(wrap, &prf).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Remove a registered credential.  Refuses to drop the last wrap
/// when the keychain is in FIDO-only mode (would orphan the
/// encrypted DB).
pub fn fido_remove(credential_id_b64: String) -> Result<(), UnkaiError> {
    let env = unkai_store::cache::key::load_envelope()?;
    if env.plain_key.is_none() && env.wraps.len() <= 1 {
        return Err(UnkaiError::Other(
            "Cannot remove the last hardware key while FIDO-only mode is active".into(),
        ));
    }
    unkai_store::cache::key::remove_wrap(&credential_id_b64)?;
    Ok(())
}

// ── Database lock + unlock (#164 Phase 1B) ────────────────────

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseStatusView {
    /// True when no plain key is in the envelope and the cache
    /// pool isn't open yet — the lock screen should be shown.
    pub locked: bool,
    /// True when the keychain envelope has zero registered methods
    /// and zero plain key — the user has wiped everything;
    /// app needs to recreate from scratch.
    pub needs_setup: bool,
    /// One entry per registered unlock method (FIDO PRF or
    /// passphrase), used by the lock screen to render a picker.
    pub methods: Vec<FidoCredentialView>,
    /// Remaining unlock attempts before wipe-on-failure fires.
    /// `None` when the policy is off or has no limit set —
    /// the lock screen renders "X tries remaining" only when this
    /// is `Some(_)`.
    pub attempts_remaining: Option<u32>,
}

/// Snapshot used by `App.svelte` on mount to decide whether to
/// route the user to the lock screen or straight into the inbox.
pub fn database_status(cache: &Cache) -> Result<DatabaseStatusView, UnkaiError> {
    let env = unkai_store::cache::key::load_envelope()?;
    let locked = cache.is_locked();
    let attempts_remaining = match (env.wipe_on_failure, env.max_unlock_attempts) {
        (true, Some(max)) if max > 0 => Some(max.saturating_sub(env.failed_attempts)),
        _ => None,
    };
    Ok(DatabaseStatusView {
        locked,
        needs_setup: env.plain_key.is_none() && env.wraps.is_empty(),
        methods: env
            .wraps
            .into_iter()
            .map(|w| FidoCredentialView {
                kind: match w.kind {
                    unkai_store::fido::WrapKind::FidoPrf => "fido_prf".to_string(),
                    unkai_store::fido::WrapKind::Passphrase => "passphrase".to_string(),
                },
                credential_id: w.credential_id,
                label: w.label,
                salt: w.salt,
                created_at: w.created_at,
            })
            .collect(),
        attempts_remaining,
    })
}

/// Wipe the cache file and clear the keychain envelope.
/// Triggered when the user exhausts their unlock budget OR
/// when the envelope's integrity MAC fails.
pub fn perform_wipe(cache: &Cache) {
    if let Err(e) = cache.wipe_on_disk() {
        tracing::error!("wipe_on_disk failed: {e}");
    }
    let cleared = unkai_store::fido::KeychainEnvelope {
        version: 1,
        plain_key: None,
        wraps: Vec::new(),
        wipe_on_failure: false,
        max_unlock_attempts: None,
        failed_attempts: 0,
        integrity_mac: None,
    };
    if let Err(e) = unkai_store::cache::key::save_envelope(&cleared) {
        tracing::error!("clearing envelope after wipe failed: {e}");
    }
}

/// Bump the persisted failure counter and, if the user has
/// opted into the wipe-on-failure policy, blow away the cache
/// once the configured retry budget is exhausted.  The counter
/// lives in the keychain envelope (not just process memory) so
/// kill+relaunch can't reset the budget.  An invalid envelope
/// MAC trips the wipe immediately on the next failure regardless
/// of where the persisted counter sat.
pub fn note_unlock_failure(cache: &Cache, label: &str) -> UnkaiError {
    let mut env = match unkai_store::cache::key::load_envelope() {
        Ok(e) => e,
        Err(e) => return e,
    };
    let tampered = unkai_store::cache::key::envelope_tampered(&env);
    if tampered {
        tracing::warn!("Keychain envelope MAC mismatch — treating this attempt as terminal.");
    }
    env.failed_attempts = env.failed_attempts.saturating_add(1);
    let attempts = env.failed_attempts;
    if let Err(e) = unkai_store::cache::key::save_envelope(&env) {
        tracing::warn!("could not persist failure counter: {e}");
    }
    if env.wipe_on_failure || tampered {
        let max = env.max_unlock_attempts.unwrap_or(0);
        let trip = tampered || (max > 0 && attempts >= max);
        if trip {
            if tampered {
                tracing::warn!("Wipe fired due to envelope tampering.");
            } else {
                tracing::warn!(
                    "Wipe-on-failure policy fired: {attempts} consecutive failed unlock attempts (limit {max})."
                );
            }
            perform_wipe(cache);
            return UnkaiError::Auth(if tampered {
                "Keychain envelope was modified outside Unkai. The encrypted cache has been wiped."
                    .to_string()
            } else {
                format!(
                    "Too many failed attempts ({attempts}/{max}). The encrypted cache has been wiped."
                )
            });
        }
    }
    UnkaiError::Auth(format!("incorrect {label}"))
}

/// Reset the persisted failure counter on a successful unlock.
pub fn note_unlock_success() {
    let Ok(mut env) = unkai_store::cache::key::load_envelope() else {
        return;
    };
    if env.failed_attempts == 0 {
        return;
    }
    env.failed_attempts = 0;
    if let Err(e) = unkai_store::cache::key::save_envelope(&env) {
        tracing::warn!("could not reset failure counter: {e}");
    }
}

/// Unlock the cache from a passphrase.  Tries every passphrase
/// wrap in the envelope, returns the first match.
pub fn unlock_with_passphrase(passphrase: String, cache: &Cache) -> Result<(), UnkaiError> {
    use unkai_store::fido::{self, WrapKind};
    let env = unkai_store::cache::key::load_envelope()?;
    for wrap in &env.wraps {
        if wrap.kind != WrapKind::Passphrase {
            continue;
        }
        let salt = fido::decode_b64(&wrap.salt)?;
        let aes_key = fido::derive_passphrase_key(&passphrase, &salt)?;
        if let Ok(master) = fido::unwrap_master_key(wrap, &aes_key) {
            let hex = hex::encode(&master);
            cache
                .unlock_with_master_key(hex)
                .map_err(UnkaiError::from)?;
            note_unlock_success();
            return Ok(());
        }
    }
    Err(note_unlock_failure(cache, "passphrase"))
}

/// Unlock the cache from a FIDO PRF assertion.  Frontend has
/// already run WebAuthn `credentials.get` against the
/// credential's stored salt and forwards the resulting PRF
/// output here.
pub fn unlock_with_prf(
    credential_id_b64: String,
    prf_output_b64: String,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    use unkai_store::fido::{self, WrapKind};
    let env = unkai_store::cache::key::load_envelope()?;
    let prf = fido::decode_b64(&prf_output_b64)?;
    for wrap in &env.wraps {
        if wrap.kind != WrapKind::FidoPrf || wrap.credential_id != credential_id_b64 {
            continue;
        }
        let master = match fido::unwrap_master_key(wrap, &prf) {
            Ok(m) => m,
            Err(_) => return Err(note_unlock_failure(cache, "hardware key PRF output")),
        };
        let hex = hex::encode(&master);
        cache
            .unlock_with_master_key(hex)
            .map_err(UnkaiError::from)?;
        note_unlock_success();
        return Ok(());
    }
    Err(UnkaiError::Auth(
        "no registered hardware key matches that credential".into(),
    ))
}

/// Switch the cache into FIDO-only mode: drop the plain master
/// key from the keychain envelope so future cold launches MUST
/// authenticate with one of the registered methods.  Refuses
/// unless the user has at least one passphrase OR ≥ 2 hardware
/// keys registered — without a recovery option we'd lock them
/// out permanently the first time a YubiKey gets lost.
pub fn enable_fido_only_mode() -> Result<(), UnkaiError> {
    use unkai_store::fido::WrapKind;
    let mut env = unkai_store::cache::key::load_envelope()?;
    if env.plain_key.is_none() {
        return Ok(()); // already FIDO-only — idempotent.
    }
    let passphrase_count = env
        .wraps
        .iter()
        .filter(|w| w.kind == WrapKind::Passphrase)
        .count();
    let fido_count = env
        .wraps
        .iter()
        .filter(|w| w.kind == WrapKind::FidoPrf)
        .count();
    if passphrase_count == 0 && fido_count < 2 {
        return Err(UnkaiError::Other(
            "Register at least one passphrase OR two hardware keys before enabling FIDO-only mode \
             — otherwise losing a single key would lock the cache permanently."
                .into(),
        ));
    }
    env.plain_key = None;
    unkai_store::cache::key::save_envelope(&env)?;
    Ok(())
}

/// Snapshot of the wipe-on-failure policy stored in the
/// keychain envelope.  `enabled = false` means unlimited
/// retries.  `max_attempts = None` means the same — the toggle
/// can be on but with no number set; we treat that as
/// effectively off until a number is provided.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WipePolicyView {
    pub enabled: bool,
    pub max_attempts: Option<u32>,
}

pub fn get_wipe_policy() -> Result<WipePolicyView, UnkaiError> {
    let env = unkai_store::cache::key::load_envelope()?;
    Ok(WipePolicyView {
        enabled: env.wipe_on_failure,
        max_attempts: env.max_unlock_attempts,
    })
}

pub fn set_wipe_policy(policy: WipePolicyView) -> Result<(), UnkaiError> {
    let mut env = unkai_store::cache::key::load_envelope()?;
    env.wipe_on_failure = policy.enabled;
    env.max_unlock_attempts = if policy.enabled {
        policy.max_attempts.filter(|n| *n > 0)
    } else {
        None
    };
    unkai_store::cache::key::save_envelope(&env)?;
    Ok(())
}

/// Reverse of `enable_fido_only_mode` — re-store the plain
/// master key in the envelope so the next launch opens the
/// cache without prompting.  Only callable while the cache is
/// already unlocked (we need the in-memory key).
pub fn disable_fido_only_mode(cache: &Cache) -> Result<(), UnkaiError> {
    if cache.is_locked() {
        return Err(UnkaiError::Auth(
            "Database must be unlocked before FIDO-only mode can be disabled".into(),
        ));
    }
    let key_hex = cache.master_key_hex().ok_or_else(|| {
        UnkaiError::Auth(
            "Master key isn't available in memory — unlock the database again before disabling key encryption".into(),
        )
    })?;
    let mut env = unkai_store::cache::key::load_envelope()?;
    if env.plain_key.is_some() {
        return Ok(()); // already plain — idempotent.
    }
    env.plain_key = Some(key_hex);
    unkai_store::cache::key::save_envelope(&env)?;
    Ok(())
}

pub async fn get_app_settings(settings: &SharedSettings) -> Result<AppSettings, UnkaiError> {
    Ok(settings.read().await.clone())
}

pub async fn update_app_settings(
    new_settings: AppSettings,
    settings: &SharedSettings,
    notify: &SettingsSyncNotify,
    mcp: &McpServer,
) -> Result<(), UnkaiError> {
    app_settings::save_settings(&new_settings)?;
    *settings.write().await = new_settings;
    notify.0.notify_one();
    // The MCP server reads `mcp_enabled` / `mcp_port` from these
    // settings — reconcile so a toggle flip takes effect
    // immediately, not on next launch (#438).
    mcp.reconcile().await;
    Ok(())
}

// ── MCP server (#438) ──────────────────────────────────────────
//
// Token management + status for the AI settings page.  The
// keychain (`unkai-mail-mcp` service) is the only place the
// bearer token persists; the running server compares against an
// in-memory copy, so generate/revoke update both in one step.

/// Generate (or rotate) the MCP bearer token.  Returns the secret
/// **once** — the frontend shows it a single time and never asks
/// for it again; afterwards only `mcp_token_status` (a bool) is
/// available.  Rotating invalidates the previous token instantly.
pub async fn mcp_generate_token(mcp: &McpServer) -> Result<String, UnkaiError> {
    let token = unkai_mcp::auth::generate_token()?;
    credentials::store_mcp_token(&token)?;
    mcp.set_token(Some(token.clone())).await;
    Ok(token)
}

/// Revoke the MCP bearer token: every connected client is cut off
/// on its next request.  The server keeps running (if enabled)
/// but answers 401 until a new token is generated.
pub async fn mcp_revoke_token(mcp: &McpServer) -> Result<(), UnkaiError> {
    credentials::delete_mcp_token()?;
    mcp.set_token(None).await;
    Ok(())
}

/// Whether a bearer token currently exists.  Deliberately a bare
/// bool — the secret itself is only ever returned by
/// `mcp_generate_token`.
pub async fn mcp_token_status() -> Result<bool, UnkaiError> {
    credentials::has_mcp_token()
}

/// Live server status (running / bound port / endpoint URL /
/// last start error) for the AI settings page.
pub async fn mcp_server_status(mcp: &McpServer) -> Result<McpServerStatus, UnkaiError> {
    Ok(mcp.status().await)
}

/// One row of the AI settings page's per-tool toggle list: a
/// registry descriptor plus the tool's *effective* enablement
/// (explicit `mcp_tool_enablement` entry, or the class default —
/// reads on, writes off).  Same snake_case wire shape as the
/// other MCP views.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolView {
    pub id: &'static str,
    pub category: &'static str,
    /// `"read"` or `"write"` — drives the visual grouping and the
    /// write-tool warning hints in the settings UI.
    pub access: &'static str,
    pub description: &'static str,
    pub enabled: bool,
}

/// Every MCP tool this build knows about (#439).  The settings
/// page renders whatever the registry advertises rather than a
/// hardcoded list, so the tool surfaces landing in #440/#441
/// appear in the UI without another frontend change.
pub async fn mcp_list_tools(settings: &SharedSettings) -> Result<Vec<McpToolView>, UnkaiError> {
    let settings = settings.read().await;
    Ok(unkai_mcp::registry::ToolRegistry::builtin()
        .iter()
        .map(|tool| McpToolView {
            id: tool.descriptor.id,
            category: tool.descriptor.category,
            access: match tool.descriptor.access {
                unkai_mcp::registry::ToolAccess::Read => "read",
                unkai_mcp::registry::ToolAccess::Write => "write",
            },
            description: tool.descriptor.description,
            enabled: unkai_mcp::registry::is_enabled(&settings, &tool.descriptor),
        })
        .collect())
}

// ── Settings backup & sync (#168) ──────────────────────────────
//
// Lets the user save every preference (app-wide settings, account
// metadata, folder→emoji mappings, signature, locale, theme, …)
// to either a local `settings.json` or to a connected Nextcloud
// under `/Unkai Mail/settings/settings.json`.  Restoring is the
// reverse: pick a file, or — on first NC connect — accept the
// "found a backup, restore?" prompt.
//
// Architecture
//
//   • Bundle = { version, exported_at, app_settings, accounts,
//                local_storage }.  Schema-versioned JSON; secrets
//                (passwords, FIDO wraps, master key) deliberately
//                excluded.  See `unkai_store::settings_bundle`.
//
//   • NC sync runs in a background task.  Frontend calls
//     `notify_settings_changed(local_storage)` after any UI
//     mutation; the worker debounces 2 s, then PUTs the bundle
//     to NC.  Failure flips `settings_sync::SettingsSyncState
//     ::pending` to true so the next opportunity (next change OR
//     the periodic 5-min retry) takes another shot.
//
//   • The worker is started once from `main()` after the cache
//     unlocks (it needs `Cache` access for accounts).  On launch
//     it consults `pending` from disk; if true, it attempts a
//     push immediately so a quit-while-offline still recovers.
//
//   • Sync target (`target_nc_id`) is stored *outside* the
//     bundle — it's a per-machine choice.  Restoring on a new
//     device shouldn't silently start syncing back to the old
//     server.

/// Path inside a user's Nextcloud where the settings bundle
/// lives.  Sits under the existing `/Unkai Mail` root so it
/// shares a folder with the temp area used by Office viewer.
pub const UNKAI_SETTINGS_DIR: &str = "/Unkai Mail/settings";

pub const UNKAI_SETTINGS_FILE: &str = "/Unkai Mail/settings/settings.json";

/// Return the live `AppSettings` + accounts + the frontend's
/// supplied `local_storage` map as one JSON-serialisable bundle.
/// Shared by the export path below and the auto-sync worker.
pub async fn build_settings_bundle(
    local_storage: std::collections::HashMap<String, String>,
    cache: &Cache,
) -> Result<String, UnkaiError> {
    let bundle = settings_bundle::build_bundle(cache, local_storage)?;
    settings_bundle::serialise(&bundle)
}

/// Apply a previously-exported bundle.  Replaces `app_settings`,
/// upserts each account by id, and returns the bundle's
/// `local_storage` map so the frontend can write each key back
/// into its own `localStorage`.  The frontend reloads its UI
/// after this returns — most preferences only re-apply on the
/// next render pass.
pub async fn apply_settings_bundle(
    json: String,
    cache: &Cache,
    settings: &SharedSettings,
    mcp: &McpServer,
) -> Result<std::collections::HashMap<String, String>, UnkaiError> {
    let bundle = settings_bundle::parse(&json)?;
    let new_app_settings = bundle.app_settings.clone();
    let local_storage = settings_bundle::apply(cache, bundle)?;
    *settings.write().await = new_app_settings;
    // The imported bundle may flip `mcp_enabled` / `mcp_port`
    // (#438).  Note the bearer token never travels in a bundle —
    // a restored machine serves 401s until the user generates a
    // fresh token here.
    mcp.reconcile().await;
    Ok(local_storage)
}

/// Build the live settings bundle and write it to `path`.
///
/// #477 — the path comes from a native "Save As" dialog opened by
/// the desktop shell, not from the webview: the frontend can only
/// *trigger* "export my settings", it never chooses (or even sees)
/// where the file lands, so no raw filesystem path crosses the IPC
/// boundary.
pub async fn export_settings_bundle_to_path(
    path: &std::path::Path,
    local_storage: std::collections::HashMap<String, String>,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let json = build_settings_bundle(local_storage, cache).await?;
    std::fs::write(path, json)
        .map_err(|e| UnkaiError::Other(format!("Failed to write {}: {e}", path.display())))
}

/// Read a bundle file and apply it.  Same shape as
/// `export_settings_bundle_to_path`: the path comes from the
/// shell's native open dialog (#477), never from the webview.
/// Post-conditions match `apply_settings_bundle`; the returned
/// `local_storage` map is for the frontend to mirror into its own
/// storage.
pub async fn import_settings_bundle_from_path(
    path: &std::path::Path,
    cache: &Cache,
    settings: &SharedSettings,
    mcp: &McpServer,
) -> Result<std::collections::HashMap<String, String>, UnkaiError> {
    let json = std::fs::read_to_string(path)
        .map_err(|e| UnkaiError::Other(format!("Failed to read {}: {e}", path.display())))?;
    apply_settings_bundle(json, cache, settings, mcp).await
}

/// Frontend-facing view of `settings_sync::SettingsSyncState`.
/// camelCase for the JSON IPC convention used elsewhere in the
/// file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsSyncStateView {
    pub target_nc_id: Option<String>,
    pub pending: bool,
}

pub fn get_settings_sync_state() -> Result<SettingsSyncStateView, UnkaiError> {
    let state = settings_sync::load_state()?;
    Ok(SettingsSyncStateView {
        target_nc_id: state.target_nc_id,
        pending: state.pending,
    })
}

/// Pick (or clear) the connected Nextcloud account that recovery
/// pushes go to.  Passing `None` turns the feature off.  Setting
/// it kicks off a sync immediately so the chosen NC has a fresh
/// copy without waiting for the next settings change.
pub async fn set_settings_sync_target(
    target_nc_id: Option<String>,
    notify: &SettingsSyncNotify,
) -> Result<(), UnkaiError> {
    let mut state = settings_sync::load_state()?;
    if state.target_nc_id == target_nc_id {
        return Ok(());
    }
    state.target_nc_id = target_nc_id;
    // Flipping the target counts as a "settings changed" event —
    // the new NC needs a fresh push so a future restore actually
    // finds something there.
    state.pending = state.target_nc_id.is_some();
    settings_sync::save_state(&state)?;
    notify.0.notify_one();
    Ok(())
}

/// Frontend-side hook: call after any settings mutation that the
/// user could plausibly want backed up.  Stores the latest
/// `localStorage` snapshot in shared state and pings the auto-
/// sync worker, which debounces and pushes to NC if a target is
/// set.  No-ops cleanly when sync is off.
pub async fn notify_settings_changed(
    local_storage: std::collections::HashMap<String, String>,
    storage: &SharedLocalStorage,
    notify: &SettingsSyncNotify,
) -> Result<(), UnkaiError> {
    *storage.write().await = local_storage;
    notify.0.notify_one();
    Ok(())
}

/// Probe a connected NC for a previously-uploaded settings
/// bundle.  Returns `None` if no bundle exists at the canonical
/// path, the parsed bundle's `exported_at` if one is found.
/// Used by the "found a backup, restore?" prompt the frontend
/// shows on a fresh NC connect.
pub async fn nc_probe_settings_bundle(
    nc_id: String,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    match unkai_nextcloud::download_file(
        &account.server_url,
        &account.username,
        &app_password,
        UNKAI_SETTINGS_FILE,
        &account.trusted_certs,
    )
    .await
    {
        Ok(bytes) => {
            let json = String::from_utf8(bytes).map_err(|e| {
                UnkaiError::Storage(format!("settings bundle on NC is not UTF-8: {e}"))
            })?;
            let bundle = settings_bundle::parse(&json)?;
            Ok(Some(bundle.exported_at))
        }
        // 404 = no backup, that's the normal first-time path.
        // We map it through to None so the UI can stay quiet
        // instead of surfacing an error toast.
        Err(UnkaiError::Nextcloud(msg)) if msg.contains("not found") => Ok(None),
        Err(e) => Err(e),
    }
}

/// Download the bundle from NC and apply it.  Used by the
/// "restore on first NC connect" prompt and by a manual "Restore
/// from Nextcloud" button on the Backup & Sync settings page.
pub async fn nc_restore_settings_bundle(
    nc_id: String,
    cache: &Cache,
    settings: &SharedSettings,
) -> Result<std::collections::HashMap<String, String>, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    let bytes = unkai_nextcloud::download_file(
        &account.server_url,
        &account.username,
        &app_password,
        UNKAI_SETTINGS_FILE,
        &account.trusted_certs,
    )
    .await?;
    let json = String::from_utf8(bytes)
        .map_err(|e| UnkaiError::Storage(format!("settings bundle on NC is not UTF-8: {e}")))?;
    let bundle = settings_bundle::parse(&json)?;
    let new_app_settings = bundle.app_settings.clone();
    let local_storage = settings_bundle::apply(cache, bundle)?;
    *settings.write().await = new_app_settings;
    Ok(local_storage)
}

/// Switch the running app's icon (tray, window titlebar, taskbar)
/// to the user's picked logo style and persist the choice in
/// `AppSettings.logo_style`.  The next boot reapplies it.
///
/// Note this only swaps icons that exist *while the app runs*; the
/// `.exe` thumbnail Windows Explorer / macOS Finder shows for the
/// installed binary is baked in at `cargo tauri build` time and
/// can't change at runtime.
pub async fn set_logo_style(
    ui: &dyn UiNotifier,
    style: String,
    settings: &SharedSettings,
) -> Result<(), UnkaiError> {
    // Which bitmaps exist and which surfaces carry an icon is a shell
    // question, so applying the style lives in the `UiNotifier` impl.
    // It validates the slug, which is why this is the one fallible
    // method on the trait — and why we apply *before* persisting: a
    // bad slug must not be able to wedge the user on a style they
    // can't undo.
    ui.apply_logo_style(&style)?;

    let mut s = settings.write().await;
    s.logo_style = style;
    app_settings::save_settings(&s)?;
    Ok(())
}

// ── Custom themes (#132 tier 2) ────────────────────────────────
//
// User picks a Skeleton-shape CSS file in the Settings → Design
// "Import theme…" flow.  The frontend hands us the file's
// absolute path; we copy the bytes under
// `<config>/unkai-mail/themes/<id>.css`, parse out the
// `[data-theme="…"]` slug to use as the picker id, and append a
// `CustomTheme` record to AppSettings.
//
// Removal deletes both the on-disk copy and the AppSettings row;
// the frontend's theme picker rebuilds from `get_app_settings`
// after each operation, so no extra plumbing.

/// Resolve the user-themes directory under the app's config root.
/// Created on demand — first import is what creates the folder.
pub fn custom_themes_dir() -> Result<std::path::PathBuf, UnkaiError> {
    let base = dirs::config_dir()
        .ok_or_else(|| UnkaiError::Other("cannot resolve user config dir".into()))?;
    let dir = base.join("unkai-mail").join("themes");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(UnkaiError::Other(format!(
            "create themes dir {}: {e}",
            dir.display()
        )));
    }
    Ok(dir)
}

/// Pull the theme slug out of an imported CSS file by scanning
/// for the first `[data-theme="…"]` selector.  Falls back to the
/// file stem when the file doesn't follow Skeleton's convention,
/// so the user still gets *something* in the picker — just won't
/// switch unless they edit the CSS to match the slug.
pub fn extract_theme_slug(css: &str, fallback: &str) -> String {
    let needle = "[data-theme=";
    if let Some(idx) = css.find(needle) {
        let tail = &css[idx + needle.len()..];
        // Accept both `"foo"` and `'foo'` quoting, tolerate
        // intra-attribute whitespace.
        let trimmed = tail.trim_start();
        if let Some(rest) = trimmed
            .strip_prefix('"')
            .or_else(|| trimmed.strip_prefix('\''))
            && let Some(end) = rest.find(['"', '\''])
        {
            let slug = rest[..end].trim();
            if !slug.is_empty() {
                return slug.to_string();
            }
        }
    }
    fallback.to_string()
}

/// Copy a user-picked CSS file into the app's themes directory
/// and append a `CustomTheme` record to AppSettings.  Returns the
/// freshly-created record so the frontend can register the
/// runtime stylesheet without re-reading settings.
///
/// Soft-fails on a duplicate slug by overwriting the previous
/// import — that's the natural "I edited the same file and want
/// to re-import" flow, and avoids forcing the user to remove the
/// old row first.
pub async fn import_custom_theme(
    ui: &dyn UiNotifier,
    source_path: String,
    label: Option<String>,
    settings: &SharedSettings,
) -> Result<CustomTheme, UnkaiError> {
    let src = std::path::PathBuf::from(&source_path);
    if !src.exists() {
        return Err(UnkaiError::Other(format!(
            "theme source not found: {source_path}"
        )));
    }
    let css = std::fs::read_to_string(&src)
        .map_err(|e| UnkaiError::Other(format!("read theme source: {e}")))?;
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("custom-theme")
        .to_string();
    let slug = extract_theme_slug(&css, &stem);
    let dir = custom_themes_dir()?;
    let dest = dir.join(format!("{slug}.css"));
    std::fs::write(&dest, &css).map_err(|e| UnkaiError::Other(format!("copy theme file: {e}")))?;

    let record = CustomTheme {
        id: slug.clone(),
        label: label.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
            // Title-case the slug so "my_theme" reads "My theme"
            // rather than something the user has to fix manually.
            stem.replace(['_', '-'], " ")
        }),
        description: "Imported theme".to_string(),
        path: dest.to_string_lossy().to_string(),
    };

    {
        let mut s = settings.write().await;
        // Replace any existing row with the same id (re-import).
        s.custom_themes.retain(|t| t.id != record.id);
        s.custom_themes.push(record.clone());
        app_settings::save_settings(&s)?;
    }

    // Tell every window so a second-window picker stays in sync.
    ui.custom_themes_changed();
    Ok(record)
}

/// Remove a user-imported theme — drops both the on-disk CSS and
/// the AppSettings row.  No-op when the id isn't found so the UI
/// can fire-and-forget without checking first.
pub async fn remove_custom_theme(
    ui: &dyn UiNotifier,
    id: String,
    settings: &SharedSettings,
) -> Result<(), UnkaiError> {
    let path: Option<String> = {
        let mut s = settings.write().await;
        let path = s
            .custom_themes
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.path.clone());
        s.custom_themes.retain(|t| t.id != id);
        // If the removed theme was the active one, drop back to
        // the default so the UI doesn't try to render a missing
        // file on next launch.
        if s.theme_name == id {
            s.theme_name = "cerberus".into();
        }
        app_settings::save_settings(&s)?;
        path
    };
    if let Some(p) = path
        && let Err(e) = std::fs::remove_file(&p)
    {
        tracing::warn!("remove theme file {p}: {e}");
    }
    ui.custom_themes_changed();
    Ok(())
}
