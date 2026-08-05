//! Sending, drafts, and the outbox queue.
//!
//! Mirrors `ui/src/lib/api/compose.ts`.

use unkai_core::UnkaiError;
use unkai_core::models::Account;
use unkai_core::models::OutgoingEmail;
use unkai_imap::ImapClient;
use unkai_jmap::JmapClient;
use unkai_smtp::SmtpClient;
use unkai_smtp::build_outgoing_message;
use unkai_store::Cache;
use unkai_store::credentials;

use crate::crypto_bridge::AppCryptoBridge;
use crate::mail::emit_mail_flags_updated;
use crate::notify::OutboxUpdatedPayload;
use crate::notify::UiNotifier;
use crate::state::AppContext;
use crate::support::{
    connect_jmap, load_account, pick_drafts_folder, should_clean_cache_for_delete, uses_jmap,
};

// ── SMTP commands ───────────────────────────────────────────────

/// Reference to the original message a Compose send is responding to
/// (#255).  Set by Compose's reply / reply-all / "respond with
/// meeting" flows so the backend can flip the IMAP `\Answered` flag
/// (or JMAP `$answered` keyword) on the original and stamp the
/// per-kind `replied_kind` into the local cache, which drives the
/// reply-icon prefix on the mail-list row.  `None` for fresh
/// composes / forwards / drafts — none of which are "answers".
///
/// `Serialize` so the Outbox (#276) can stash this alongside the
/// queued `OutgoingEmail` and replay it on a successful drain
/// retry.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepliedToRef {
    pub folder: String,
    pub uid: u32,
    /// `"reply"` / `"reply-all"` / `"meeting"`.  Anything else falls
    /// through to a generic answered icon — the validation here is
    /// loose because the backend treats this as opaque metadata.
    pub kind: String,
}

/// Source row for the edit-from-outbox flow (#276).  Tells
/// `send_email` "I'm replacing the queued row with this id" —
/// the row is removed before the new copy is enqueued so the
/// queue never holds two versions of the same message during a
/// resend.  Optional on every send; absent for ordinary sends
/// (compose / reply / forward) and for retries that re-fire
/// the existing queued row in place.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxSourceRef {
    pub id: i64,
}

/// Pre-computed display fields for an Outbox row.  Cheap to render
/// straight onto the row without re-deserialising the full
/// `OutgoingEmail` JSON for every list refresh.
pub fn outbox_display_fields(email: &OutgoingEmail) -> (String, String, String) {
    let to_display = email.to.join(", ");
    (email.from.clone(), to_display, email.subject.clone())
}

/// Send an email via the account's configured SMTP server (#276).
///
/// **Always queue first.**  Every send routes through the local
/// `outbox_messages` table before touching SMTP.  Validation
/// (build the lettre `Message`) runs synchronously so the user
/// still gets a Compose-modal error for malformed addresses; the
/// row is then enqueued and a Tokio task spawned to attempt the
/// drain.  On a healthy network the drain finishes in the same
/// tick and the row never paints in the UI; on failure the row
/// stays for the periodic retry sweep in `background_sync_loop`.
///
/// The post-send work that used to live here (Sent APPEND,
/// answered-flag flip, JMAP send) is factored into
/// `try_drain_outbox_entry` — the spawned task and the retry
/// sweep call into the same helper so the success path is
/// identical regardless of when the drain fires.
///
/// `replied_to` (#255) is preserved through queue + retry so a
/// reply that takes a few sweeps to land still flips `\Answered`
/// on the original message.
///
/// `outbox_source` (#276 follow-up) carries the id of a queued
/// row this send is replacing — the edit-from-outbox path.  When
/// set, the row is removed before the new copy is enqueued so
/// the queue never briefly holds both versions.  Cancelling
/// Compose never reaches this command, so the original row stays
/// put on cancel.
pub async fn send_email(
    account_id: String,
    email: OutgoingEmail,
    replied_to: Option<RepliedToRef>,
    outbox_source: Option<OutboxSourceRef>,
    pgp_passphrase: Option<String>,
    ctx: &AppContext,
) -> Result<i64, UnkaiError> {
    let cache = &ctx.cache;
    // PGP passphrase (#57): the Compose UI prompts the user when
    // they tick "Encrypt" and submit, then hands the value through
    // this IPC.  We don't store it anywhere — it's threaded straight
    // into the first send attempt and dropped when this command
    // returns.  Background outbox retries that fire later won't have
    // it, so encrypted rows that fail to drain surface a clear
    // "needs interactive retry" error and the Compose retry path
    // can re-prompt.
    let _ = pgp_passphrase.as_deref(); // referenced lower in the drain branch

    // Validate up-front: building the lettre Message rejects bad
    // addresses, missing bodies, etc.  Doing it here means
    // user-facing input errors still surface in Compose's modal
    // rather than landing silently in the Outbox.  The IMAP /
    // JMAP routing decision (uses_jmap) doesn't care — both paths
    // need a valid OutgoingEmail.
    let _ = build_outgoing_message(&email)?;

    let (from_header, to_display, subject) = outbox_display_fields(&email);
    let outgoing_json = serde_json::to_string(&email)
        .map_err(|e| UnkaiError::Other(format!("serialize OutgoingEmail for outbox: {e}")))?;
    let replied_to_json =
        match replied_to.as_ref() {
            Some(rt) => Some(serde_json::to_string(rt).map_err(|e| {
                UnkaiError::Other(format!("serialize RepliedToRef for outbox: {e}"))
            })?),
            None => None,
        };

    // #276 follow-up — drop the source row before enqueueing the
    // edit so the queue holds at most one copy of this message at
    // any moment.  Idempotent: a `remove_outbox` for an id that's
    // already drained / been deleted is a no-op (zero rows
    // affected, no error).  Done before the new INSERT so a
    // failure in this branch can't leak a duplicate.
    if let Some(src) = outbox_source.as_ref()
        && let Err(e) = cache.remove_outbox(src.id)
    {
        tracing::warn!("remove source outbox row {} failed: {e}", src.id);
    }

    let entry_id = cache.enqueue_outbox(&unkai_store::OutboxEnqueue {
        account_id: account_id.clone(),
        outgoing_json,
        replied_to_json,
        from_header,
        to_display,
        subject,
        skip_sent_copy: email.skip_sent_copy,
    })?;

    // Tell the frontend the queue grew so the synthetic Outbox
    // folder appears in the sidebar (no-op when the drain task
    // beats us — the row has already been removed by the time the
    // listener acts).
    emit_outbox_updated(cache, ctx.ui.as_ref());

    // Kick off the drain attempt immediately on a background
    // task.  The task captures its own `AppContext` clone (a couple
    // of `Arc` bumps) so it outlives this command's return.  Cheap:
    // ~tens of microseconds per spawn.  Carries the freshly-prompted
    // PGP passphrase (#57) inline so the *first* drain attempt can
    // encrypt without re-prompting; subsequent retries from the
    // periodic sweep don't get it, by design.
    let spawned = ctx.clone();
    tokio::spawn(async move {
        let _ = try_drain_outbox_entry_with_passphrase(
            spawned.ui.as_ref(),
            &spawned.cache,
            entry_id,
            pgp_passphrase.as_deref(),
            false,
        )
        .await;
    });

    // #276 follow-up: return the new row id so Compose can hand
    // it to App.svelte's `onsentenqueued` callback.  The
    // edit-from-outbox path uses it to surface the new (or
    // still-failing) row in the right pane immediately, so the
    // user sees their edit in the queue without manually
    // re-clicking the row.
    Ok(entry_id)
}

/// Drive one queued outbox row through SMTP / JMAP.  Removes the
/// row on success, records the error on failure (the row stays
/// for the next sweep).  Used by:
///
///   * the spawned task `send_email` kicks off after enqueue,
///   * the `retry_outbox_entry` Tauri command (manual retry from
///     the UI),
///   * the periodic drain sweep in `background_sync_loop`.
///
/// Best-effort by design: any failure in the post-send Sent
/// APPEND / answered-flag flip is logged and the row is still
/// removed (the SMTP succeeded, the user's mail is out, the
/// missing local-side bookkeeping will reconcile on the next
/// envelope fetch).
pub async fn try_drain_outbox_entry(ui: &dyn UiNotifier, cache: &Cache, entry_id: i64) {
    let _ = try_drain_outbox_entry_with_passphrase(ui, cache, entry_id, None, false).await;
}

/// Variant of [`try_drain_outbox_entry`] that carries a freshly-
/// prompted PGP passphrase forward to the SMTP send (#57).  Used by
/// the IPC entry point right after `send_email` enqueues; every
/// other caller (periodic sweep, manual retry) drops back to the
/// no-passphrase shape above and the encryption path surfaces a
/// clear "needs interactive retry" error.
///
/// Returns the inner send result so callers that need to surface a
/// precise error inline (the Outbox encrypted-retry UI in
/// `retry_outbox_entry_with_passphrase`, #341) can do so.  The cache
/// is still mutated internally either way — `remove_outbox` on
/// success, `record_outbox_failure` on error — so fire-and-forget
/// callers see no behavioural change and can ignore the result.
/// Returns `Ok(())` for the "row vanished mid-drain" and "claim held
/// by another drain" no-op branches; those aren't errors the user
/// needs to see.
///
/// `force_claim` (#341 follow-up): the CAS-style claim in
/// `claim_outbox_for_drain` refuses a re-claim inside a 30 s window
/// to keep the post-enqueue spawn and the periodic sweep from
/// racing.  That guard is wrong for the user-driven retry path: a
/// freshly-failed row has `last_attempt_at = now`, so a user click
/// inside the next 30 s would be refused and this function would
/// return `Ok` without actually running — closing the passphrase
/// panel deceptively.  `force_claim = true` switches to the
/// unconditional `force_claim_outbox_for_drain` for the manual-
/// retry case (no concurrent drain exists — the previous attempt
/// already failed, otherwise the row would be gone).  All
/// automatic callers pass `false` so the existing race protection
/// stays in force for them.
pub async fn try_drain_outbox_entry_with_passphrase(
    ui: &dyn UiNotifier,
    cache: &Cache,
    entry_id: i64,
    pgp_passphrase: Option<&str>,
    force_claim: bool,
) -> Result<(), UnkaiError> {
    // Claim the row before doing any real work (#292 follow-up).
    // Without this guard, the spawned drain `send_email` kicks off
    // and the periodic `drain_outbox_sweep` can both reach this
    // function for the same `entry_id` — each reads the row, each
    // pushes it through SMTP + APPEND-to-Sent, and the recipient
    // receives the same mail twice.  A 30 s TTL is comfortable for
    // any healthy SMTP roundtrip and short enough that a crashed
    // drain stops blocking retries quickly.
    let claim_outcome = if force_claim {
        cache.force_claim_outbox_for_drain(entry_id)
    } else {
        cache.claim_outbox_for_drain(entry_id, 30)
    };
    match claim_outcome {
        Ok(true) => {}
        Ok(false) => {
            // Force-claim returns `false` only when the row has
            // vanished — same shape as the TTL claim's "row gone"
            // outcome.  TTL claim also returns `false` when another
            // drain holds the row inside the 30 s window; that
            // branch is unreachable in `force_claim = true` calls.
            tracing::debug!(
                "try_drain_outbox_entry: skipping entry {entry_id}, claim held by another drain or row gone"
            );
            return Ok(());
        }
        Err(e) => {
            tracing::warn!("claim_outbox_for_drain({entry_id}) failed: {e}");
            return Err(e.into());
        }
    }

    let row = match cache.get_outbox(entry_id) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()), // Already removed (manual delete, race with another drain).
        Err(e) => {
            tracing::warn!("get_outbox({entry_id}) failed: {e}");
            return Err(e.into());
        }
    };

    let email: OutgoingEmail = match serde_json::from_str(&row.outgoing_json) {
        Ok(e) => e,
        Err(e) => {
            // Hard-failed deserialise — almost certainly a schema
            // change upstream.  Record the error so the user can
            // see it on the row and decide to delete; don't keep
            // retrying forever on a malformed row.
            let msg = format!("malformed outbox payload: {e}");
            if let Err(c) = cache.record_outbox_failure(entry_id, &msg) {
                tracing::warn!("record_outbox_failure failed: {c}");
            }
            return Err(UnkaiError::Other(msg));
        }
    };
    let replied_to: Option<RepliedToRef> =
        row.replied_to_json
            .as_deref()
            .and_then(|s| match serde_json::from_str(s) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("malformed outbox replied_to_json: {e}");
                    None
                }
            });

    let account = match load_account(cache, &row.account_id) {
        Ok(a) => a,
        Err(e) => {
            // Account was removed while a row was queued.  Drop
            // the row — there's nowhere to send from.
            tracing::warn!(
                "outbox drain dropping row {entry_id}: account '{}' missing: {e}",
                row.account_id
            );
            let _ = cache.remove_outbox(entry_id);
            emit_outbox_updated(cache, ui);
            return Err(e);
        }
    };

    // Outbox sweeps (no passphrase) surface "needs interactive
    // retry" for encrypted rows; the first-attempt path from
    // `send_email` carries the freshly-prompted passphrase forward.
    let send_result: Result<(), UnkaiError> = run_send_pipeline(
        ui,
        cache,
        &account,
        &email,
        replied_to.as_ref(),
        pgp_passphrase,
    )
    .await;

    match send_result {
        Ok(()) => {
            if let Err(e) = cache.remove_outbox(entry_id) {
                tracing::warn!("remove_outbox after success failed: {e}");
            }
            emit_outbox_updated(cache, ui);
            Ok(())
        }
        Err(e) => {
            let msg = format!("{e}");
            tracing::info!(
                "outbox drain for entry {entry_id} (account '{}') failed: {msg}",
                row.account_id
            );
            if let Err(c) = cache.record_outbox_failure(entry_id, &msg) {
                tracing::warn!("record_outbox_failure failed: {c}");
            }
            emit_outbox_updated(cache, ui);
            Err(e)
        }
    }
}

/// Inner send pipeline shared by `try_drain_outbox_entry` (every
/// outbox attempt) and any future direct-send caller.  Mirrors the
/// pre-#276 `send_email` body verbatim — JMAP path returns after
/// `client.send_email`, IMAP path runs SMTP + best-effort Sent
/// APPEND + best-effort answered-flag flip.
///
/// `pgp_passphrase` (#57): when the email asks for PGP encryption
/// and we have one, build a `AppCryptoBridge` on the spot and
/// route the send through `SmtpClient::send_with_crypto`.  A
/// background retry from the outbox sweep won't have a passphrase
/// available, so encrypted rows surface a clear "passphrase
/// needed" error rather than silently sending plaintext.
pub async fn run_send_pipeline(
    ui: &dyn UiNotifier,
    cache: &Cache,
    account: &Account,
    email: &OutgoingEmail,
    replied_to: Option<&RepliedToRef>,
    pgp_passphrase: Option<&str>,
) -> Result<(), UnkaiError> {
    if uses_jmap(account) {
        let mode = email.encryption_mode.as_deref();
        if mode == Some("pgp")
            || mode == Some("smime")
            || mode == Some("smime-sign")
            || email.signing_enabled
        {
            // We don't yet wrap the JMAP submission path in
            // `multipart/encrypted` / `multipart/signed` / `pkcs7-mime`
            // (the SMTP submission method on JMAP servers tends to want a
            // fully-built MIME and the server-side relay handles
            // transport).  Surface that mismatch loudly so the user
            // sends via SMTP instead.
            return Err(UnkaiError::Protocol(
                "Encrypted/signed send over the JMAP submission path is not yet wired — \
                 switch the account to IMAP/SMTP for encrypted or signed sends"
                    .into(),
            ));
        }
        let client = connect_jmap(account).await?;
        client.send_email(email).await?;
        if let Some(rt) = replied_to {
            mark_original_answered_jmap(account, cache, &client, rt).await;
            emit_mail_flags_updated(ui, &account.id, &rt.folder);
        }
        return Ok(());
    }

    // Build the lettre message once so the same bytes go to both
    // the SMTP recipients and the IMAP `APPEND` to Sent.  Avoids
    // the body diverging between the two paths if MIME generation
    // ever becomes non-deterministic.
    let message = build_outgoing_message(email)?;
    let raw = message.formatted();

    let password = credentials::get_imap_password(&account.id)?;
    let smtp = SmtpClient::connect(
        &account.smtp_host,
        account.smtp_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await?;
    let mode = email.encryption_mode.as_deref();
    let smime_active = mode == Some("smime") || mode == Some("smime-sign");
    // PGP keeps its historical trigger (`encryption_mode == "pgp"` or the
    // bare `signing_enabled` sign-only flag) but must not fire when an
    // explicit S/MIME mode is selected — those carry their own stack.
    let pgp_active = !smime_active && (mode == Some("pgp") || email.signing_enabled);

    if pgp_active {
        // #341 — caller passphrase wins; empty / missing falls back
        // to the keychain entry from the per-account Unlock-
        // automatically opt-in.  Only when both are absent do we
        // surface the historic "retry from Compose" Auth error.
        // Same precedence whether the user picked encrypt + sign or
        // sign-only — both unlock the same private key.
        let resolved = match pgp_passphrase {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => credentials::get_pgp_passphrase(&account.id).map_err(|_| {
                UnkaiError::Auth(
                    "PGP send requested but no passphrase supplied — \
                     retry from Compose so we can prompt"
                        .into(),
                )
            })?,
        };
        let bridge = AppCryptoBridge::for_account(&account.id, &resolved, cache.clone())?;
        smtp.send_with_crypto(email, Some(&bridge)).await?;
    } else if smime_active {
        // S/MIME send (#338).  The encrypt-only path (`smime`) needs only
        // the recipients' public certs, so we build the bridge without a
        // private identity.  The sign-only path (`smime-sign`) must unlock
        // our own `.p12` to produce the detached CMS signature, so it
        // resolves a passphrase with the same precedence as the PGP path.
        let bridge = if mode == Some("smime-sign") {
            let resolved = match pgp_passphrase {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => credentials::get_smime_passphrase(&account.id).map_err(|_| {
                    UnkaiError::Auth(
                        "S/MIME signing requested but no passphrase supplied — \
                         retry from Compose so we can prompt"
                            .into(),
                    )
                })?,
            };
            AppCryptoBridge::for_account_smime_send(&account.id, Some(&resolved), cache.clone())?
        } else {
            AppCryptoBridge::for_account_smime_send(&account.id, None, cache.clone())?
        };
        smtp.send_with_crypto(email, Some(&bridge)).await?;
    } else {
        smtp.send(email).await?;
    }

    // #416: the mail asked for a read receipt — remember the sent
    // Message-ID (lettre stamped it during `build_outgoing_message`)
    // so an incoming `message/disposition-notification` can be
    // matched back to this mail and surfaced as receipt status.
    // Recorded only after the SMTP send succeeded: a failed send
    // stays in the outbox and will re-run this pipeline with a
    // *fresh* Message-ID, and a pending row for never-sent mail
    // would hold the receipt-scan gate open for nothing.
    if email.request_read_receipt
        && let Some(mid) = extract_message_id(&raw)
    {
        // sent_receipts keys on the bracket-free form (#277).
        let mid = mid
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string();
        if let Err(e) = cache.record_receipt_request(&account.id, &mid) {
            tracing::warn!("record_receipt_request failed: {e}");
        }
    }

    // Best-effort APPEND to Sent (same behaviour as before #276):
    // the user's mail is already out, a failure here is logged
    // but doesn't roll the send back.
    if !email.skip_sent_copy
        && let Err(e) = append_to_sent(account, &raw, cache).await
    {
        tracing::warn!(
            "Sent OK but failed to append a copy to Sent for account '{}': {e}",
            account.id
        );
    }

    if let Some(rt) = replied_to {
        mark_original_answered_imap(account, cache, rt).await;
        emit_mail_flags_updated(ui, &account.id, &rt.folder);
    }
    Ok(())
}

/// Fire `outbox-updated` so the frontend re-reads the queue.
/// Best-effort — a dropped event just means the user has to wait
/// for the next sync tick to see the new state.
pub fn emit_outbox_updated(cache: &Cache, ui: &dyn UiNotifier) {
    let by_account = match cache.count_outbox_by_account() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("count_outbox_by_account for event payload failed: {e}");
            return;
        }
    };
    let total = by_account.values().copied().sum();
    ui.outbox_updated(&OutboxUpdatedPayload { total, by_account });
}

/// Frontend-facing shape of one Outbox row (#276).  The serde
/// rename keeps the JS side reading camelCase fields without
/// the Rust side caring about the wire format.  `outgoing` is
/// the full `OutgoingEmail` re-deserialised from
/// `outgoing_json` so the frontend can hand it straight back to
/// Compose for the edit flow without parsing.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxRowDto {
    pub id: i64,
    pub account_id: String,
    pub from_header: String,
    pub to_display: String,
    pub subject: String,
    pub queued_at: i64,
    pub attempt_count: u32,
    pub last_attempt_at: Option<i64>,
    pub last_error: Option<String>,
    pub skip_sent_copy: bool,
    /// Full `OutgoingEmail` JSON.  Parsed on the frontend by
    /// `edit_outbox_entry`'s caller; opaque on the list view.
    pub outgoing_json: String,
    pub replied_to_json: Option<String>,
}

pub fn dto_from_row(row: unkai_store::OutboxRow) -> OutboxRowDto {
    OutboxRowDto {
        id: row.id,
        account_id: row.account_id,
        from_header: row.from_header,
        to_display: row.to_display,
        subject: row.subject,
        queued_at: row.queued_at,
        attempt_count: row.attempt_count,
        last_attempt_at: row.last_attempt_at,
        last_error: row.last_error,
        skip_sent_copy: row.skip_sent_copy,
        outgoing_json: row.outgoing_json,
        replied_to_json: row.replied_to_json,
    }
}

/// Per-account Outbox list (#276).  Used by the Outbox MailList
/// variant to render the queue.
pub async fn list_outbox(
    account_id: String,
    cache: &Cache,
) -> Result<Vec<OutboxRowDto>, UnkaiError> {
    let rows = cache.list_outbox(&account_id)?;
    Ok(rows.into_iter().map(dto_from_row).collect())
}

/// Outbox list across every account (#276).  Used by unified-inbox
/// mode and by anything that needs the global queue (e.g. a tray
/// "queued mail" indicator).
pub async fn list_all_outbox(cache: &Cache) -> Result<Vec<OutboxRowDto>, UnkaiError> {
    let rows = cache.list_all_outbox()?;
    Ok(rows.into_iter().map(dto_from_row).collect())
}

/// Total queued rows across every account.  Cheap aggregate
/// query — retained for callers that want the global figure
/// (tray indicators, future global badges).
pub async fn count_outbox(cache: &Cache) -> Result<u32, UnkaiError> {
    Ok(cache.count_outbox()?)
}

/// Queued-row counts grouped by `account_id` (#290).  Used as the
/// startup seed for the Sidebar's per-account "render synthetic
/// Outbox folder?" decision so a queue carried over from a prior
/// session shows up without waiting for the first `outbox-updated`
/// event.  Accounts with zero queued rows are omitted.
pub async fn count_outbox_by_account(
    cache: &Cache,
) -> Result<std::collections::HashMap<String, u32>, UnkaiError> {
    Ok(cache.count_outbox_by_account()?)
}

/// Force a drain attempt on a specific row (#276).  Used by the
/// "Retry now" button in the Outbox row UI.  Same code path the
/// background sweep uses — succeeds, fails, or no-ops if the row
/// vanished.  Doesn't block: the actual SMTP work runs on a
/// spawned task so the UI returns instantly.
pub async fn retry_outbox_entry(id: i64, ctx: &AppContext) -> Result<(), UnkaiError> {
    let spawned = ctx.clone();
    tokio::spawn(async move {
        try_drain_outbox_entry(spawned.ui.as_ref(), &spawned.cache, id).await;
    });
    Ok(())
}

/// Awaiting variant of [`retry_outbox_entry`] that threads a fresh
/// PGP passphrase forward and surfaces the precise send error inline
/// (#341).  Backs the Outbox's "Retry with passphrase" panel: a row
/// that failed to drain because the background sweep had no
/// passphrase is retried with the one the user just typed.  Unlike
/// the fire-and-forget sibling, this awaits the drain so the panel
/// can re-prompt on a `Crypto: ...` (wrong-passphrase) error without
/// racing the `outbox-updated` event back to the list.
///
/// `pgp_passphrase` may be empty — that's the auto-unlock fast path
/// where the account has [`pgp_has_unlock_automatically`] turned on
/// and `run_send_pipeline`'s precedence (caller → keychain) resolves
/// from the keychain entry.  The frontend pre-checks the toggle and
/// submits an empty string when it's on, sparing the user a prompt.
pub async fn retry_outbox_entry_with_passphrase(
    id: i64,
    pgp_passphrase: String,
    ctx: &AppContext,
) -> Result<(), UnkaiError> {
    // `force_claim = true`: the previous attempt already failed
    // (otherwise the row would be gone), so the 30 s TTL guard would
    // refuse the re-claim and silently no-op back to the panel —
    // closing it without actually retrying.  No concurrent drain
    // exists in the manual-retry case, so force is safe.
    try_drain_outbox_entry_with_passphrase(
        ctx.ui.as_ref(),
        &ctx.cache,
        id,
        Some(pgp_passphrase.as_str()),
        true,
    )
    .await
}

/// Drop a queued row without sending (#276).  Used by the
/// "Delete" button in the Outbox row UI.  Idempotent — deleting
/// a row that's already drained is a no-op.
pub async fn delete_outbox_entry(
    id: i64,
    cache: &Cache,
    ui: &dyn UiNotifier,
) -> Result<(), UnkaiError> {
    cache.remove_outbox(id)?;
    emit_outbox_updated(cache, ui);
    Ok(())
}

/// Pull a queued row's `OutgoingEmail` (and replied-to ref) for
/// re-opening in Compose (#276).  Removes the row from the queue
/// — the new send Compose triggers will create a fresh row.  If
/// the user cancels Compose without sending, the original
/// content is gone; the user can resend manually if needed.
pub async fn edit_outbox_entry(
    id: i64,
    cache: &Cache,
    ui: &dyn UiNotifier,
) -> Result<OutboxRowDto, UnkaiError> {
    let row = cache
        .get_outbox(id)?
        .ok_or_else(|| UnkaiError::Other(format!("outbox row {id} not found")))?;
    cache.remove_outbox(id)?;
    emit_outbox_updated(cache, ui);
    Ok(dto_from_row(row))
}

/// Best-effort: stamp the local cache row + the IMAP `\Answered`
/// flag on the original message that a Compose reply just answered
/// (#255).  Logs on failure rather than propagating — the user's
/// mail already left the building.
pub async fn mark_original_answered_imap(account: &Account, cache: &Cache, rt: &RepliedToRef) {
    if let Err(e) = cache.mark_envelope_replied(&account.id, &rt.folder, rt.uid, &rt.kind) {
        tracing::warn!(
            "answered-cache update failed for account '{}', folder '{}', uid {}: {e}",
            account.id,
            rt.folder,
            rt.uid
        );
    }

    let password = match credentials::get_imap_password(&account.id) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("answered-flag IMAP STORE skipped — keychain lookup failed: {e}");
            return;
        }
    };
    let mut client = match ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("answered-flag IMAP STORE skipped — connect failed: {e}");
            return;
        }
    };
    if let Err(e) = client.mark_as_answered(&rt.folder, rt.uid).await {
        tracing::warn!(
            "answered-flag IMAP STORE failed for '{}' uid {}: {e}",
            rt.folder,
            rt.uid
        );
    }
    let _ = client.logout().await;
}

/// JMAP analogue of `mark_original_answered_imap` — uses the
/// already-connected JMAP client (no second connect needed since
/// JMAP is HTTPS-pooled, not a long-lived session).
pub async fn mark_original_answered_jmap(
    account: &Account,
    cache: &Cache,
    client: &JmapClient,
    rt: &RepliedToRef,
) {
    if let Err(e) = cache.mark_envelope_replied(&account.id, &rt.folder, rt.uid, &rt.kind) {
        tracing::warn!(
            "answered-cache update failed for account '{}', folder '{}', uid {}: {e}",
            account.id,
            rt.folder,
            rt.uid
        );
    }
    if let Err(e) = client.mark_as_answered(&rt.folder, rt.uid).await {
        tracing::warn!(
            "answered-keyword JMAP set failed for '{}' uid {}: {e}",
            rt.folder,
            rt.uid
        );
    }
}

/// Locate the account's Sent folder (via the IMAP `\Sent` attribute,
/// or a name-based fallback) and `APPEND` the raw RFC 822 bytes there.
/// Marked `\Seen` so it doesn't add to the unread badge.
pub async fn append_to_sent(
    account: &Account,
    raw: &[u8],
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let sent_folder = pick_sent_folder(&account.id, cache);
    let Some(sent) = sent_folder else {
        return Err(UnkaiError::Other(
            "no Sent folder found in cached folder list".into(),
        ));
    };

    let password = credentials::get_imap_password(&account.id)?;
    let mut client = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await?;
    let result = client.append_message(&sent, raw, &["\\Seen"]).await;
    let _ = client.logout().await;
    result
}

/// Payload for the "this save replaces an existing draft" flow.
/// When Compose opens an existing draft for editing, the frontend
/// hands the source UID + folder back here so `save_draft` can
/// APPEND-then-delete inside the same IMAP session — avoiding the
/// split-connection race where a separate `delete_message` call
/// would run after the APPEND and sometimes leave the original
/// behind.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftReplaceSource {
    pub folder: String,
    pub uid: u32,
}

/// What `save_draft` reports back to the caller (#292).
///
/// `folder` is the IMAP folder we APPENDed into (either the
/// `replace_source.folder` when editing, or the result of
/// `pick_drafts_folder` for a fresh draft). `uid` is the new
/// server-assigned UID discovered via a `UID SEARCH HEADER
/// Message-ID` round-trip after the APPEND — `None` when the
/// search failed or returned no hits, in which case the caller
/// has to treat the next save as a fresh APPEND and accept that
/// the previous copy will remain in Drafts as a duplicate.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedDraft {
    pub folder: String,
    pub uid: Option<u32>,
}

/// Pull the `Message-ID` header value out of a raw RFC 822 message.
///
/// Thin re-export shim: the implementation moved to
/// `unkai_core::mail_util` (#440) so the MCP `create_draft` tool
/// can share it.
pub fn extract_message_id(raw: &[u8]) -> Option<String> {
    unkai_core::mail_util::extract_message_id(raw)
}

/// Save an in-progress message to the account's IMAP Drafts folder.
///
/// Mirrors `send_email` structurally (same `OutgoingEmail` input, same
/// MIME builder) but skips SMTP entirely — the point is to hand the
/// message to the server so it shows up in the Drafts mailbox across
/// devices and the user can finish / send it later. IMAP-only for now;
/// JMAP accounts get a clear error until the equivalent `Email/set`
/// create-in-Drafts flow is wired up (tracked separately).
///
/// When `replace_source` is set, the save is treated as a
/// continuation of an existing draft the user opened from Drafts:
/// we APPEND the new copy into that *same folder* (not whatever
/// `pick_drafts_folder` thinks Drafts is — the server might have
/// multiple drafts-like folders and we want the edit to land where
/// the user is looking) and then EXPUNGE the source UID in the
/// same session, so from the user's perspective the draft they
/// were editing is updated in place.
pub async fn save_draft(
    account_id: String,
    email: OutgoingEmail,
    replace_source: Option<DraftReplaceSource>,
    cache: &Cache,
) -> Result<SavedDraft, UnkaiError> {
    let account = load_account(cache, &account_id)?;

    if uses_jmap(&account) {
        return Err(UnkaiError::Other(
            "Saving drafts via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }

    let message = build_outgoing_message(&email)?;
    let raw = message.formatted();
    // Pulled before APPEND so the post-APPEND SEARCH has a value
    // to match against even if some later step (e.g. the replace
    // delete) panics — `None` here just means we can't dedup the
    // next save, not that the user's draft was lost.
    let message_id = extract_message_id(&raw);

    // Prefer the source folder when replacing an existing draft so
    // APPEND and DELETE both target the folder the user actually
    // opened the draft from. Otherwise fall back to the "find the
    // account's Drafts folder" heuristic for brand-new drafts.
    let target_folder = match replace_source.as_ref() {
        Some(src) => src.folder.clone(),
        None => pick_drafts_folder(&account.id, cache).ok_or_else(|| {
            UnkaiError::Other("no Drafts folder found in cached folder list".into())
        })?,
    };

    // Optimistic-UI tombstone (#292 follow-up): mark the source
    // draft as pending-delete BEFORE the IMAP roundtrip so any
    // mid-flight `fetch_envelopes` (folder switch, sync tick) sees
    // the cached row already filtered out.  Without this the
    // frontend's `mergeEnvelopes` keeps the old UID alive in
    // `existing` (it preserves rows the fresh batch didn't return,
    // to support pagination) so the user briefly sees both copies
    // until the eventual sync evicts the stale one.  Mirrors the
    // pattern in `delete_message`.
    //
    // `upsert_message_pending` (not plain `mark_message_pending`)
    // because chained minimize-saves leave the source UID without
    // a corresponding cache row: the first minimize APPENDs uid N
    // but never writes the envelope into the cache, so a second
    // minimize trying to tombstone uid N as a UPDATE finds zero
    // rows and silently misses.  A concurrent `poll_folder` mid-
    // save then inserts the row from IMAP with `pending_action`
    // NULL and the draft pops back into the visible list.
    if let Some(src) = replace_source.as_ref()
        && let Err(e) = cache.upsert_message_pending(&account_id, &src.folder, src.uid, "delete")
    {
        tracing::warn!("save_draft upsert_message_pending(delete) failed: {e}");
    }

    let password = credentials::get_imap_password(&account.id)?;
    let connect_result = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await;
    let mut client = match connect_result {
        Ok(c) => c,
        Err(e) => {
            // IMAP unreachable: un-tombstone the row so the user
            // doesn't lose sight of their existing draft while
            // we couldn't even attempt the replace.
            if let Some(src) = replace_source.as_ref()
                && let Err(c) = cache.clear_message_pending(&account_id, &src.folder, src.uid)
            {
                tracing::warn!("clear_message_pending after save_draft connect failure: {c}");
            }
            return Err(e);
        }
    };

    // `\Draft` marks the message as an unfinished draft. `\Seen`
    // keeps it out of the unread badge — there's no point notifying
    // the user about a mail they themselves just composed.
    let append_result = client
        .append_message(&target_folder, &raw, &["\\Draft", "\\Seen"])
        .await;

    // APPEND failure: the new copy never landed, so the old draft
    // is still authoritative — un-tombstone it so the user can
    // see (and retry from) their unchanged source.
    if append_result.is_err()
        && let Some(src) = replace_source.as_ref()
        && let Err(c) = cache.clear_message_pending(&account_id, &src.folder, src.uid)
    {
        tracing::warn!("clear_message_pending after save_draft APPEND failure: {c}");
    }

    // Only attempt the delete if the APPEND actually succeeded —
    // otherwise a flaky APPEND would have us destroy the user's
    // only remaining copy. We also want to clear the cached envelope
    // for the source UID whether the server-side delete hit an
    // existing UID or complained that the UID wasn't there (ghost
    // envelope left over from a previous expunge) — either way the
    // cached row is wrong and hanging onto it just makes the next
    // edit attempt fail the same way.
    let delete_result = if append_result.is_ok() {
        if let Some(src) = replace_source.as_ref() {
            let delete_result = client.delete_message(&src.folder, src.uid).await;
            let should_clean = should_clean_cache_for_delete(&delete_result);
            if should_clean && let Err(e) = cache.remove_envelope(&account_id, &src.folder, src.uid)
            {
                tracing::warn!("remove_envelope after save_draft replace failed: {e}");
            }
            // Real DELETE failure (not the stale-UID case the cleanup
            // heuristic absorbs): the old draft is still on the
            // server even though APPEND succeeded.  Un-tombstone so
            // the user sees it again — the new copy is also in
            // place, so the result is two visible drafts and the
            // user can manually discard whichever they want.
            if !should_clean
                && delete_result.is_err()
                && let Err(c) = cache.clear_message_pending(&account_id, &src.folder, src.uid)
            {
                tracing::warn!("clear_message_pending after save_draft DELETE failure: {c}");
            }
            match delete_result {
                Ok(()) => Ok(()),
                Err(e) => Err(UnkaiError::Other(format!(
                    "Draft saved, but removing the previous copy (UID {}) failed: {e}",
                    src.uid
                ))),
            }
        } else {
            Ok(())
        }
    } else {
        append_result
    };

    // SEARCH the target folder for the just-APPENDed message by
    // Message-ID so the caller can pass the new UID as
    // `replace_source` on the next save (#292) — keeps Drafts
    // pruned to one copy per in-flight Compose instead of letting
    // every minimize stack a fresh duplicate. Best-effort: a
    // missing Message-ID, a server that rejects the SEARCH, or a
    // server that hasn't yet indexed the new mail all collapse
    // back to `uid: None`, and the caller treats the next save as
    // a fresh APPEND.
    let new_uid = if delete_result.is_ok() {
        match &message_id {
            Some(id) => match client.find_uid_by_message_id(&target_folder, id).await {
                Ok(uid) => uid,
                Err(e) => {
                    tracing::warn!("SEARCH after save_draft APPEND failed: {e}");
                    None
                }
            },
            None => {
                tracing::warn!(
                    "save_draft: could not extract Message-ID from raw bytes; \
                     next save will not be able to replace this copy"
                );
                None
            }
        }
    } else {
        None
    };

    let _ = client.logout().await;
    delete_result.map(|()| SavedDraft {
        folder: target_folder,
        uid: new_uid,
    })
}

/// Synchronously tombstone a Drafts row that's about to be expunged
/// by the send pipeline (#292 follow-up).
///
/// Compose's `send()` closes the modal immediately (#156's instant-
/// close UX) and bumps `refreshToken` via the parent's
/// `closeCompose`.  That bump triggers MailList's `load()` BEFORE
/// the background `runSendPipeline` reaches its
/// `invoke('delete_message')` call — so without an upfront
/// tombstone, the fresh fetch returns the source draft and
/// `mergeEnvelopes` puts it back in the visible list, where it
/// hangs around until the next sync evicts it.
///
/// Calling this from the frontend BEFORE `onclose()` plants the
/// tombstone in time: `get_cached_envelopes` filters on
/// `pending_action IS NULL`, and `upsert_envelopes_for_account`
/// doesn't include `pending_action` in its ON CONFLICT UPDATE list,
/// so a concurrent sync writing the same row preserves the
/// tombstone.  The eventual `delete_message` call still does the
/// real IMAP work and either removes the row entirely (success)
/// or clears the tombstone (real failure) — same semantics as
/// calling `delete_message` alone, just split so the cache flag
/// lands before the visible refresh.
pub async fn tombstone_draft_for_expunge(
    account_id: String,
    folder: String,
    uid: u32,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    // `upsert_message_pending` (not plain `mark_message_pending`)
    // because a minimize-saved draft lives on the IMAP server
    // without a corresponding cache row — `save_draft` only
    // touches the cache on the replace path.  Without the upsert,
    // an UPDATE-only tombstone would miss those UIDs entirely
    // and the next poll would re-insert them sans `pending_action`,
    // flashing the row back into the visible list (#292 follow-up).
    cache
        .upsert_message_pending(&account_id, &folder, uid, "delete")
        .map_err(Into::into)
}

/// Permanently expunge a Drafts UID after the user sent its
/// contents (#292 follow-up).
///
/// Different from the user-facing `delete_message` command in two
/// important ways:
///
/// 1. **Skips move-to-Trash.**  `delete_message` routes "delete from
///    a non-Trash folder" through a `UID COPY` to Trash followed by
///    an EXPUNGE of the source.  That's right for a manual delete
///    (user can recover from Trash) but wrong here: the draft was
///    *consumed* by the send, depositing a duplicate in Trash
///    would just clutter the user's mailbox.  Matches the inline
///    expunge the `save_draft` replace path uses for the same
///    reason.
///
/// 2. **Keeps the tombstone on IMAP failure.**  `delete_message`
///    clears `pending_action` on real failures so the row reappears
///    on the next poll — which is correct for a delete that the
///    user can retry from the visible list, but produces the
///    "draft flicks back into Drafts after sending" symptom here:
///    the mail itself shipped, so the user expects the draft to
///    be gone whether or not the cleanup IMAP DELETE landed.  We
///    leave the tombstone in place; if the row really survived on
///    the server, a folder-wipe reconcile or a fresh poll will
///    eventually re-surface it, but the immediate post-send
///    experience is correct.
pub async fn expunge_draft_after_send(
    account_id: String,
    folder: String,
    uid: u32,
    cache: &Cache,
) -> Result<(), UnkaiError> {
    let account = load_account(cache, &account_id)?;

    if uses_jmap(&account) {
        return Err(UnkaiError::Other(
            "Expunging drafts via JMAP is not yet implemented — this account uses JMAP".into(),
        ));
    }

    // Tombstone the row (creating a placeholder if absent — the
    // minimize-saved UID case where the cache row doesn't exist
    // yet) so any concurrent poll keeps the row hidden across the
    // IMAP roundtrip.
    if let Err(e) = cache.upsert_message_pending(&account_id, &folder, uid, "delete") {
        tracing::warn!("expunge_draft_after_send upsert_message_pending failed: {e}");
    }

    let password = credentials::get_imap_password(&account.id)?;
    let connect_result = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await;
    let mut client = match connect_result {
        Ok(c) => c,
        Err(e) => {
            // No tombstone clear here — see fn docs for the
            // post-send UX rationale.  The user already shipped
            // the mail; surfacing a half-deleted source draft
            // doesn't help them.
            return Err(e);
        }
    };

    let delete_result = client.delete_message(&folder, uid).await;
    let _ = client.logout().await;

    // Deliberately *not* dropping the cache row on success here
    // (#292 follow-up).  Some IMAP servers (Gmail, certain
    // Exchange variants) take a moment to propagate an EXPUNGE
    // to fresh sessions — long enough that a `poll_folder` racing
    // ahead of the propagation will re-fetch the just-deleted UID,
    // and if the row is already gone from the cache the INSERT
    // path of `upsert_envelopes_for_account` writes a fresh row
    // *without* `pending_action`, so the draft pops back into the
    // visible list.  Leaving the tombstone planted keeps the row
    // hidden across that window — the next reconcile pass in
    // `poll_folder` removes it cleanly once the server confirms
    // it's gone from `list_all_uids`.
    //
    // The stale-UID case ("isn't in folder") is also fine to
    // leave tombstoned: the row already isn't on the server, so
    // reconcile will drop it on the next poll.
    //
    // Real IMAP failure (server unreachable mid-EXPUNGE,
    // permission error, etc.) → tombstone also stays.  The
    // user already shipped the mail; surfacing a half-deleted
    // source draft would just confuse.  If the server really
    // never removed the message, a future poll's reconcile keeps
    // the row cached and tombstoned; that's a soft leak but
    // user-invisible.
    delete_result
}

/// Pick the most likely Sent folder name from the cached folder list.
/// Prefers folders flagged with the IMAP `\Sent` special-use attribute
/// (the canonical, locale-independent answer) and falls back to common
/// English / German / French names so accounts that haven't been
/// re-synced after their first launch still get a copy filed somewhere
/// sensible. Returns `None` if nothing matches — the caller surfaces
/// that as a warning rather than an error.
pub fn pick_sent_folder(account_id: &str, cache: &Cache) -> Option<String> {
    let folders = cache.get_folders(account_id).ok()?;

    if let Some(by_attr) = folders.iter().find(|f| {
        f.attributes
            .iter()
            .any(|a| a.eq_ignore_ascii_case("sent") || a.eq_ignore_ascii_case("\\sent"))
    }) {
        return Some(by_attr.name.clone());
    }

    const NAME_HINTS: &[&str] = &[
        "sent",
        "sent items",
        "sent messages",
        "sent mail",
        "gesendet",
        "gesendete elemente",
        "envoyés",
    ];
    folders
        .iter()
        .find(|f| {
            let lower = f.name.to_lowercase();
            NAME_HINTS.iter().any(|h| lower.contains(h))
        })
        .map(|f| f.name.clone())
}
