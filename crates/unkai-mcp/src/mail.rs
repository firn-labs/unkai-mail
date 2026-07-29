//! The MCP mail tool set (#440).
//!
//! Five read tools (`search_mail`, `get_message`, `get_thread`,
//! `list_folders`, `list_accounts` — default **on**) and one write
//! tool (`create_draft` — default **off**).  There is deliberately
//! **no send tool**: drafts keep a human between the agent and the
//! wire.  An agent can prepare a mail, but only the user — in
//! Unkai, with the full message in front of them — can send it.
//!
//! ## Encrypted-content policy
//!
//! The local cache stores *decrypted* plaintext for messages on
//! auto-unlock accounts, and the FTS index is built from that
//! plaintext.  Every path that would surface body content — the
//! `get_message` body and the `search_mail` snippets — therefore
//! checks the message's stored `protection` and withholds content
//! of PGP/S-MIME-**encrypted** messages behind a marker, unless
//! the user enabled "Expose decrypted content to AI agents"
//! (`AppSettings::mcp_expose_decrypted_content`, default off) in
//! the AI settings.  Merely *signed* mail is not sensitive and is
//! never redacted.  The flag is read per call from the live
//! settings, so flipping it applies immediately.
//!
//! ## Result shapes
//!
//! Every message is identified by the stable ref triple
//! `account_id` / `folder` / `uid` — the same triple every tool
//! accepts, so an agent can chain `search_mail → get_message →
//! get_thread → create_draft(reply_to)` without translation.
//! Tool output is one JSON text block; snake_case keys matching
//! the parameter names.

use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{Value, json};
use unkai_core::mail_util;
use unkai_core::models::{Account, Email, OutgoingEmail};
use unkai_imap::ImapClient;
use unkai_smtp::build_outgoing_message;
use unkai_store::cache::{SearchFilters, SearchScope};
use unkai_store::credentials;

use crate::registry::{ToolAccess, ToolContext, ToolDescriptor, ToolRegistry};
use crate::util::{
    arg, internal, invalid, json_result, load_accounts, optional_str, optional_str_list,
    optional_u32, require_known_account, required_str, required_str_list, required_u32, schema,
};

/// What replaces bodies and snippets of encrypted messages while
/// the expose toggle is off.  Public so tests (and the docs issue,
/// #442) reference the exact wire value.
pub const REDACTION_MARKER: &str = "[encrypted content withheld]";

/// Hard cap on the body text returned by `get_message`.  Agents
/// work in bounded context windows; a multi-megabyte newsletter
/// body helps nobody.  Counted in characters, cut on a char
/// boundary, flagged via `body_truncated`.
const MAX_BODY_CHARS: usize = 50_000;

const DEFAULT_SEARCH_LIMIT: u32 = 25;
const MAX_SEARCH_LIMIT: u32 = 100;

/// Register the whole mail tool set on the shared registry.
pub(crate) fn register_mail_tools(registry: &mut ToolRegistry) {
    registry.register(
        ToolDescriptor {
            id: "search_mail",
            category: "mail",
            access: ToolAccess::Read,
            requires: None,
            description:
                "Full-text search over the user's locally synced mail. The query combines free \
                 text with operators, all AND-ed: from:, to:, cc:, subject:, body: (quote \
                 multi-word values: subject:\"project x\"), has:attachment, is:unread / is:read \
                 / is:flagged, after:/before:/on: with YYYY-MM-DD (or YYYY-MM / YYYY for whole \
                 periods), and in:<folder> / in:anywhere. Bare words prefix-match; \"quoted \
                 phrases\" match exactly. Returns newest-first hits with a snippet (matches \
                 wrapped in <mark> tags) and the account_id/folder/uid ref that get_message, \
                 get_thread, and create_draft take. Only mail already synced by Unkai is \
                 searchable; iterate with narrower operators rather than raising the limit.",
        },
        schema(json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search text with optional operators. Empty returns the newest messages in scope."
                },
                "account_id": {
                    "type": "string",
                    "description": "Restrict to one account (see list_accounts). Omit to search every account."
                },
                "folder": {
                    "type": "string",
                    "description": "Restrict to one folder (see list_folders). An in: operator in the query wins over this."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SEARCH_LIMIT,
                    "description": "Maximum hits to return. Default 25."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(search_mail(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "get_message",
            category: "mail",
            access: ToolAccess::Read,
            requires: None,
            description:
                "Read one cached message: envelope fields plus the plain-text body (HTML-only \
                 mail is converted to text). Bodies of end-to-end-encrypted messages are \
                 withheld unless the user enabled exposing decrypted content in Unkai Mail's \
                 AI settings. Takes the account_id/folder/uid ref from search_mail or \
                 get_thread; only messages Unkai has already fetched are readable.",
        },
        schema(message_ref_schema()),
        Arc::new(|ctx, args| Box::pin(get_message(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "get_thread",
            category: "mail",
            access: ToolAccess::Read,
            requires: None,
            description: "List every cached message in the same conversation (thread) as the given \
                 message, newest first. Returns envelope data only — no bodies; call \
                 get_message per member for content. Threads are scoped to one folder.",
        },
        schema(message_ref_schema()),
        Arc::new(|ctx, args| Box::pin(get_thread(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "list_folders",
            category: "mail",
            access: ToolAccess::Read,
            requires: None,
            description: "List one account's mail folders as Unkai has them synced, including IMAP \
                 special-use attributes (\\Drafts, \\Sent, \\Trash, …) and unread counts.",
        },
        schema(json!({
            "type": "object",
            "required": ["account_id"],
            "properties": {
                "account_id": {
                    "type": "string",
                    "description": "Account to list folders for (see list_accounts)."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(list_folders(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "list_accounts",
            category: "mail",
            access: ToolAccess::Read,
            requires: None,
            description:
                "List the mail accounts configured in Unkai Mail: id, display name, and email \
                 address. Use the id as account_id in every other mail tool.",
        },
        schema(json!({"type": "object", "properties": {}})),
        Arc::new(|ctx, args| Box::pin(list_accounts(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "create_draft",
            category: "mail",
            access: ToolAccess::Write,
            requires: None,
            description:
                "Create a draft in the account's Drafts folder (IMAP APPEND — the draft also \
                 appears on the user's other devices). The draft is NOT sent and there is no \
                 tool that sends mail: the user reviews and sends from Unkai Mail. To reply to \
                 a message, pass its folder/uid as reply_to — threading headers are set \
                 automatically, but compose the subject yourself (e.g. \"Re: …\"). Plain-text \
                 body only.",
        },
        schema(json!({
            "type": "object",
            "required": ["account_id", "to"],
            "properties": {
                "account_id": {
                    "type": "string",
                    "description": "Account to create the draft in (see list_accounts)."
                },
                "to": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "description": "Recipients: bare addresses or \"Name <addr@example.com>\"."
                },
                "cc": {"type": "array", "items": {"type": "string"}},
                "bcc": {"type": "array", "items": {"type": "string"}},
                "subject": {"type": "string"},
                "body": {"type": "string", "description": "Plain-text message body."},
                "reply_to": {
                    "type": "object",
                    "required": ["folder", "uid"],
                    "properties": {
                        "folder": {"type": "string"},
                        "uid": {"type": "integer"}
                    },
                    "description": "Cached message (same account) this draft replies to; sets In-Reply-To/References for correct threading."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(create_draft(ctx, args))),
    );
}

/// Shared input schema for the tools addressing one message.
fn message_ref_schema() -> Value {
    json!({
        "type": "object",
        "required": ["account_id", "folder", "uid"],
        "properties": {
            "account_id": {"type": "string", "description": "Owning account (see list_accounts)."},
            "folder": {"type": "string", "description": "Folder the message lives in."},
            "uid": {"type": "integer", "description": "IMAP UID within the folder."}
        }
    })
}

// ── Handlers ────────────────────────────────────────────────────

async fn search_mail(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let query = optional_str(&args, "query")?.unwrap_or_default();
    let scope = SearchScope {
        account_id: optional_str(&args, "account_id")?,
        folder: optional_str(&args, "folder")?,
        limit: optional_u32(&args, "limit")?
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_LIMIT),
    };

    let hits = ctx
        .cache
        .search_emails(&query, &scope, &SearchFilters::default())
        .map_err(|e| internal(format!("search failed: {e}")))?;
    let expose = expose_decrypted(&ctx).await;

    let hits: Vec<Value> = hits
        .into_iter()
        .map(|hit| {
            let encrypted = is_encrypted(hit.protection.as_deref());
            // The FTS index holds whatever plaintext the body row
            // holds, so a snippet of an encrypted message IS
            // decrypted content — same policy gate as bodies.
            let snippet = if encrypted && !expose {
                REDACTION_MARKER.to_string()
            } else {
                hit.snippet
            };
            json!({
                "account_id": hit.account_id,
                "folder": hit.folder,
                "uid": hit.uid,
                "from": hit.from,
                "subject": hit.subject,
                "date": hit.date.to_rfc3339(),
                "is_read": hit.is_read,
                "is_starred": hit.is_starred,
                "has_attachments": hit.has_attachments,
                "encrypted": encrypted,
                "snippet": snippet,
            })
        })
        .collect();

    Ok(json_result(json!({
        "result_count": hits.len(),
        "hits": hits,
    })))
}

async fn get_message(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let (account_id, folder, uid) = message_ref(&args)?;

    let email = ctx
        .cache
        .get_message(&account_id, &folder, uid)
        .map_err(|e| internal(format!("cache read failed: {e}")))?
        .ok_or_else(message_not_cached)?;

    let encrypted = is_encrypted(email.protection.as_deref());
    let redacted = encrypted && !expose_decrypted(&ctx).await;

    let (body, body_source, truncated) = if redacted {
        (REDACTION_MARKER.to_string(), "redacted", false)
    } else {
        match (&email.body_text, &email.body_html) {
            (Some(text), _) if !text.trim().is_empty() => {
                let (body, truncated) = truncate_chars(text.clone(), MAX_BODY_CHARS);
                (body, "text", truncated)
            }
            (_, Some(html)) => {
                let (body, truncated) = truncate_chars(html_to_text(html), MAX_BODY_CHARS);
                (body, "html-stripped", truncated)
            }
            _ => (String::new(), "empty", false),
        }
    };

    // Attachment *metadata* comes from the decrypted MIME
    // structure, so filenames of an encrypted message are withheld
    // along with the body.
    let attachments: Vec<Value> = if redacted {
        Vec::new()
    } else {
        email
            .attachments
            .iter()
            .map(|a| {
                json!({
                    "filename": a.filename,
                    "content_type": a.content_type,
                    "size": a.size,
                })
            })
            .collect()
    };

    let mut result = json!({
        "account_id": account_id,
        "folder": folder,
        "uid": uid,
        "from": email.from,
        "to": email.to,
        "cc": email.cc,
        "subject": email.subject,
        "date": email.date.to_rfc3339(),
        "is_read": email.is_read,
        "has_attachments": email.has_attachments,
        "message_id": email.message_id,
        "protection": email.protection,
        "signature_status": email.signature_status,
        "encrypted": encrypted,
        "redacted": redacted,
        "body": body,
        "body_source": body_source,
        "body_truncated": truncated,
        "attachments": attachments,
    });
    if redacted {
        result["note"] = json!(
            "This message is end-to-end encrypted and its content is withheld from AI \
             agents. The user can change this in Unkai Mail's AI settings (\"Expose \
             decrypted content to AI agents\")."
        );
    }
    Ok(json_result(result))
}

async fn get_thread(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let (account_id, folder, uid) = message_ref(&args)?;

    let thread_id = ctx
        .cache
        .get_thread_id(&account_id, &folder, uid)
        .map_err(|e| internal(format!("cache read failed: {e}")))?
        .ok_or_else(message_not_cached)?;
    let envelopes = ctx
        .cache
        .get_envelopes_by_thread(&account_id, &folder, &thread_id)
        .map_err(|e| internal(format!("cache read failed: {e}")))?;

    let messages: Vec<Value> = envelopes
        .iter()
        .map(|e| {
            json!({
                "account_id": account_id,
                "folder": e.folder,
                "uid": e.uid,
                "from": e.from,
                "to": e.to_addrs,
                "subject": e.subject,
                "date": e.date.to_rfc3339(),
                "is_read": e.is_read,
                "is_answered": e.is_answered,
                "encrypted": is_encrypted(e.protection.as_deref()),
                "message_id": e.message_id,
            })
        })
        .collect();

    Ok(json_result(json!({
        "thread_id": thread_id,
        "message_count": messages.len(),
        "messages": messages,
    })))
}

async fn list_folders(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let account_id = required_str(&args, "account_id")?;
    require_known_account(&ctx, &account_id)?;

    let folders = ctx
        .cache
        .get_folders(&account_id)
        .map_err(|e| internal(format!("cache read failed: {e}")))?;
    let folders: Vec<Value> = folders
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "attributes": f.attributes,
                "unread_count": f.unread_count,
            })
        })
        .collect();

    Ok(json_result(json!({"folders": folders})))
}

async fn list_accounts(
    ctx: ToolContext,
    _args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    // Sanitized projection (#440): id, display name, and address
    // only.  Hosts, ports, key fingerprints, and TLS trust
    // material stay inside the app — an agent has no business
    // with connection metadata.
    let accounts: Vec<Value> = load_accounts(&ctx)?
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "display_name": a.display_name,
                "email": a.email,
            })
        })
        .collect();

    Ok(json_result(json!({"accounts": accounts})))
}

async fn create_draft(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let account_id = required_str(&args, "account_id")?;
    let to = required_str_list(&args, "to")?;
    let cc = optional_str_list(&args, "cc")?;
    let bcc = optional_str_list(&args, "bcc")?;
    let subject = optional_str(&args, "subject")?.unwrap_or_default();
    let body = optional_str(&args, "body")?.unwrap_or_default();

    let account = load_accounts(&ctx)?
        .into_iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| {
            invalid(format!(
                "unknown account_id '{account_id}' — call list_accounts for valid ids"
            ))
        })?;
    if account.use_jmap && account.jmap_url.is_some() {
        return Err(ErrorData::invalid_request(
            "this account uses JMAP; creating drafts via MCP currently supports IMAP accounts only",
            None,
        ));
    }

    // Reply threading: resolve the parent from the cache so the
    // agent never has to handle raw Message-IDs.
    let reply_parent = match reply_to_ref(&args)? {
        Some((reply_folder, reply_uid)) => Some(
            ctx.cache
                .get_message(&account_id, &reply_folder, reply_uid)
                .map_err(|e| internal(format!("cache read failed: {e}")))?
                .ok_or_else(|| {
                    invalid(
                        "reply_to message not found in the local cache — use a ref returned \
                         by search_mail or get_thread",
                    )
                })?,
        ),
        None => None,
    };

    let outgoing =
        build_draft_outgoing(&account, to, cc, bcc, subject, body, reply_parent.as_ref());
    let saved = append_draft(&ctx, &account, &outgoing).await?;

    Ok(json_result(json!({
        "status": "draft_created",
        "account_id": account_id,
        "folder": saved.folder,
        "uid": saved.uid,
        "note": "The draft was saved, not sent. The user can review and send it from Unkai Mail.",
    })))
}

/// Where an appended draft landed.  `uid: None` means the UID
/// couldn't be discovered afterwards — the draft itself is fine.
pub(crate) struct SavedDraft {
    pub folder: String,
    pub uid: Option<u32>,
}

/// Build the MIME message for `outgoing` and APPEND it to the
/// account's Drafts folder — the shared write path behind
/// `create_draft` and the `create_meeting_invite` composite
/// (#441).  IMAP only; the caller has already screened out JMAP
/// accounts.
pub(crate) async fn append_draft(
    ctx: &ToolContext,
    account: &Account,
    outgoing: &OutgoingEmail,
) -> Result<SavedDraft, ErrorData> {
    let message = build_outgoing_message(outgoing)
        .map_err(|e| invalid(format!("could not build the draft message: {e}")))?;
    let raw = message.formatted();
    let message_id = mail_util::extract_message_id(&raw);

    let folders = ctx
        .cache
        .get_folders(&account.id)
        .map_err(|e| internal(format!("cache read failed: {e}")))?;
    let drafts_folder = mail_util::pick_drafts_folder(&folders).ok_or_else(|| {
        ErrorData::invalid_request(
            "no Drafts folder found in the account's synced folder list — open the account \
             in Unkai Mail once so its folders are synced",
            None,
        )
    })?;

    let password = credentials::get_imap_password(&account.id).map_err(|e| {
        internal(format!(
            "could not read the account's mail password from the OS keychain: {e}"
        ))
    })?;
    let mut client = ImapClient::connect(
        &account.imap_host,
        account.imap_port,
        &account.email,
        &password,
        &account.trusted_certs,
    )
    .await
    .map_err(|e| internal(format!("IMAP connect failed: {e}")))?;

    // `\Draft` marks it as unfinished, `\Seen` keeps the user's
    // unread badge honest — same flags as the in-app save-draft
    // flow.
    let append_result = client
        .append_message(&drafts_folder, &raw, &["\\Draft", "\\Seen"])
        .await;

    // Best-effort UID discovery (same technique as the in-app
    // flow): SEARCH the folder for the Message-ID we just wrote.
    // `None` just means the ref can't be handed back — the draft
    // itself landed fine.
    let uid = if append_result.is_ok() {
        match &message_id {
            Some(id) => client
                .find_uid_by_message_id(&drafts_folder, id)
                .await
                .unwrap_or_default(),
            None => None,
        }
    } else {
        None
    };
    let _ = client.logout().await;

    append_result.map_err(|e| internal(format!("IMAP APPEND to '{drafts_folder}' failed: {e}")))?;

    Ok(SavedDraft {
        folder: drafts_folder,
        uid,
    })
}

/// Assemble the `OutgoingEmail` for a new draft.  Pure — split out
/// of the handler so the MIME/threading behaviour is unit-testable
/// without an IMAP server.
///
/// Threading (#277 conventions): cached Message-IDs are stored
/// *without* angle brackets, which is exactly the form
/// `OutgoingEmail` expects, so the parent's id passes straight
/// through; `references` grows the parent's chain by the parent
/// itself per RFC 5322 §3.6.4.  A parent without a Message-ID
/// yields a draft that sends fine but threads as an orphan — same
/// trade-off the in-app reply flow makes.
pub(crate) fn build_draft_outgoing(
    account: &Account,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body: String,
    reply_parent: Option<&Email>,
) -> OutgoingEmail {
    let from = if account.display_name.trim().is_empty() {
        account.email.clone()
    } else {
        format!("{} <{}>", account.display_name, account.email)
    };
    let (in_reply_to, references) = match reply_parent.and_then(|p| p.message_id.clone()) {
        Some(parent_id) => {
            let parent = reply_parent.expect("checked Some above");
            let mut references = parent.references_ids.clone();
            references.push(parent_id.clone());
            (Some(parent_id), references)
        }
        None => (None, Vec::new()),
    };

    OutgoingEmail {
        from,
        to,
        cc,
        bcc,
        reply_to: None,
        subject,
        body_text: Some(body),
        body_html: None,
        attachments: Vec::new(),
        calendar_part: None,
        skip_sent_copy: false,
        in_reply_to,
        references,
        encryption_mode: None,
        signing_enabled: false,
        request_read_receipt: false,
    }
}

// ── Policy helpers ──────────────────────────────────────────────

/// Is this stored `protection` value an E2E-encrypted message?
/// (`signed` alone carries no confidential content.)
fn is_encrypted(protection: Option<&str>) -> bool {
    matches!(protection, Some("encrypted") | Some("signed-and-encrypted"))
}

/// The user's live "Expose decrypted content to AI agents" choice.
async fn expose_decrypted(ctx: &ToolContext) -> bool {
    ctx.settings.read().await.mcp_expose_decrypted_content
}

// ── Argument parsing ────────────────────────────────────────────

fn message_not_cached() -> ErrorData {
    invalid(
        "message not found in the local cache — only mail Unkai has already synced is \
         readable; find messages via search_mail, or open the folder in Unkai Mail",
    )
}

/// The `account_id`/`folder`/`uid` triple shared by `get_message`
/// and `get_thread`.
fn message_ref(args: &Option<JsonObject>) -> Result<(String, String, u32), ErrorData> {
    Ok((
        required_str(args, "account_id")?,
        required_str(args, "folder")?,
        required_u32(args, "uid")?,
    ))
}

/// `create_draft`'s optional `reply_to: {folder, uid}` object.
fn reply_to_ref(args: &Option<JsonObject>) -> Result<Option<(String, u32)>, ErrorData> {
    match arg(args, "reply_to") {
        None => Ok(None),
        Some(Value::Object(inner)) => {
            let inner = Some(inner.clone());
            Ok(Some((
                required_str(&inner, "folder")?,
                required_u32(&inner, "uid")?,
            )))
        }
        Some(_) => Err(invalid(
            "parameter 'reply_to' must be an object with 'folder' and 'uid'",
        )),
    }
}

// ── Output helpers ──────────────────────────────────────────────

/// Cut `s` to at most `max` characters on a char boundary.
/// Returns the (possibly cut) string and whether it was cut.
fn truncate_chars(s: String, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        None => (s, false),
        Some((byte_index, _)) => (s[..byte_index].to_string(), true),
    }
}

/// Best-effort HTML → readable plain text for agents, used when a
/// message has no `text/plain` part.  Not a sanitizer and not a
/// renderer: drops `<script>`/`<style>` subtrees, turns block-level
/// boundaries into newlines, strips every other tag, decodes the
/// handful of entities mail HTML actually uses, and collapses
/// blank-line runs.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let mut rest = 0usize;

    while let Some(open) = html[rest..].find('<') {
        let open = rest + open;
        decode_entities_into(&mut out, &html[rest..open]);
        let Some(close) = html[open..].find('>') else {
            // Unterminated tag: drop the trailing fragment.
            rest = html.len();
            break;
        };
        let close = open + close;
        let tag = lower[open + 1..close].trim_start();

        if tag.starts_with("script") || tag.starts_with("style") {
            // Skip the whole subtree — its text is code, not
            // content.  An unclosed element swallows the rest of
            // the document, which is the safe direction.
            let closing = if tag.starts_with("script") {
                "</script"
            } else {
                "</style"
            };
            match lower[close..].find(closing) {
                Some(end) => {
                    let end = close + end;
                    rest = match lower[end..].find('>') {
                        Some(gt) => end + gt + 1,
                        None => html.len(),
                    };
                }
                None => rest = html.len(),
            }
            continue;
        }

        const BLOCK_TAGS: &[&str] = &[
            "br",
            "p",
            "/p",
            "div",
            "/div",
            "li",
            "/li",
            "tr",
            "/tr",
            "table",
            "/table",
            "h1",
            "/h1",
            "h2",
            "/h2",
            "h3",
            "/h3",
            "h4",
            "/h4",
            "h5",
            "/h5",
            "h6",
            "/h6",
            "blockquote",
            "/blockquote",
            "ul",
            "/ul",
            "ol",
            "/ol",
        ];
        let name: String = tag
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '/')
            .collect();
        if BLOCK_TAGS.contains(&name.as_str()) {
            out.push('\n');
        }
        rest = close + 1;
    }
    decode_entities_into(&mut out, &html[rest..]);

    // Collapse whitespace: trim line ends, cap blank runs at one.
    let mut collapsed = String::with_capacity(out.len());
    let mut blank_run = 0usize;
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        if !collapsed.is_empty() {
            collapsed.push('\n');
        }
        collapsed.push_str(line);
    }
    collapsed.trim().to_string()
}

/// Decode the few HTML entities that matter for mail bodies.
fn decode_entities_into(out: &mut String, text: &str) {
    let decoded = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    out.push_str(&decoded);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use chrono::Utc;
    use tokio::sync::RwLock;
    use unkai_core::models::{AppSettings, EmailAttachment, EmailEnvelope, Folder};
    use unkai_store::{Cache, account_store};

    fn test_context(expose: bool) -> ToolContext {
        let settings = AppSettings {
            mcp_expose_decrypted_content: expose,
            ..Default::default()
        };
        ToolContext {
            cache: Cache::open_in_memory().expect("in-memory cache"),
            settings: Arc::new(RwLock::new(settings)),
        }
    }

    fn message(uid: u32, subject: &str, body: &str, protection: Option<&str>) -> Email {
        Email {
            id: format!("INBOX:{uid}"),
            account_id: "acc".into(),
            folder: "INBOX".into(),
            from: "Alex Morgan <alex@example.com>".into(),
            to: vec!["you@example.com".into()],
            cc: vec![],
            subject: subject.into(),
            body_text: Some(body.into()),
            body_html: None,
            date: Utc::now(),
            is_read: false,
            is_starred: false,
            has_attachments: false,
            attachments: vec![],
            message_id: None,
            in_reply_to: None,
            references_ids: Vec::new(),
            protection: protection.map(str::to_string),
            signature_status: None,
            signer_fingerprint: None,
            is_pinned: false,
            priority: None,
            priority_override: None,
            reminder_at: None,
            mdn_requested_to: None,
            mdn_handled: None,
        }
    }

    fn envelope(uid: u32, message_id: Option<&str>, references: &[&str]) -> EmailEnvelope {
        EmailEnvelope {
            uid,
            folder: "INBOX".into(),
            from: "Alex Morgan <alex@example.com>".into(),
            to_addrs: vec!["you@example.com".into()],
            subject: format!("subject {uid}"),
            date: Utc::now(),
            is_read: false,
            is_starred: false,
            is_answered: false,
            replied_kind: None,
            account_id: "acc".into(),
            message_id: message_id.map(str::to_string),
            in_reply_to: None,
            references_ids: references.iter().map(|s| s.to_string()).collect(),
            thread_id: None,
            thread_total_count: None,
            protection: None,
            is_pinned: false,
            priority: None,
            priority_override: None,
            reminder_at: None,
            is_mdn_report: false,
        }
    }

    fn account(id: &str) -> Account {
        Account {
            id: id.into(),
            display_name: "Alex Morgan".into(),
            email: "alex@example.com".into(),
            imap_host: "imap.secret-host.example".into(),
            imap_port: 993,
            smtp_host: "smtp.secret-host.example".into(),
            smtp_port: 465,
            use_jmap: false,
            jmap_url: None,
            signature: None,
            folder_icons: Vec::new(),
            folder_icon_overrides: HashMap::new(),
            trusted_certs: Vec::new(),
            emoji: None,
            sort_order: 0,
            person_name: None,
            pgp_key_fingerprint: Some("9F2ADEADBEEF".into()),
            smime_cert_fingerprint: None,
        }
    }

    async fn invoke(
        ctx: &ToolContext,
        tool: &str,
        args: Value,
    ) -> Result<CallToolResult, ErrorData> {
        let registry = ToolRegistry::builtin();
        let tool = registry.get(tool).expect("tool registered");
        let args = args.as_object().cloned().expect("args are an object");
        tool.invoke(ctx.clone(), Some(args)).await
    }

    fn result_payload(result: &CallToolResult) -> Value {
        let text = result.content[0].as_text().expect("text content");
        serde_json::from_str(&text.text).expect("payload is valid JSON")
    }

    #[test]
    fn mail_tools_registered_with_expected_defaults() {
        let registry = ToolRegistry::builtin();
        let settings = AppSettings::default();
        for id in [
            "search_mail",
            "get_message",
            "get_thread",
            "list_folders",
            "list_accounts",
        ] {
            let tool = registry
                .get(id)
                .unwrap_or_else(|| panic!("{id} registered"));
            assert_eq!(
                tool.descriptor.access,
                ToolAccess::Read,
                "{id} is a read tool"
            );
            assert!(
                crate::registry::is_enabled(&settings, &tool.descriptor),
                "{id} defaults on"
            );
        }
        let draft = registry
            .get("create_draft")
            .expect("create_draft registered");
        assert_eq!(draft.descriptor.access, ToolAccess::Write);
        assert!(
            !crate::registry::is_enabled(&settings, &draft.descriptor),
            "create_draft defaults off"
        );
    }

    #[tokio::test]
    async fn search_round_trip_redacts_encrypted_snippets() {
        let ctx = test_context(false);
        ctx.cache
            .upsert_message(&message(
                1,
                "Alpha secret",
                "the secret rendezvous point",
                Some("encrypted"),
            ))
            .unwrap();
        ctx.cache
            .upsert_message(&message(2, "Alpha public", "the public agenda", None))
            .unwrap();

        let result = invoke(&ctx, "search_mail", json!({"query": "alpha"}))
            .await
            .unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["result_count"], 2);
        let hits = payload["hits"].as_array().unwrap();
        let hit = |uid: u64| hits.iter().find(|h| h["uid"] == uid).unwrap();
        assert_eq!(hit(1)["encrypted"], true);
        assert_eq!(hit(1)["snippet"], REDACTION_MARKER);
        assert_eq!(hit(2)["encrypted"], false);
        assert!(hit(2)["snippet"].as_str().unwrap().contains("<mark>"));

        // Flip the live toggle — no restart — and the snippet flows.
        ctx.settings.write().await.mcp_expose_decrypted_content = true;
        let result = invoke(&ctx, "search_mail", json!({"query": "alpha"}))
            .await
            .unwrap();
        let payload = result_payload(&result);
        let hits = payload["hits"].as_array().unwrap();
        let hit = |uid: u64| hits.iter().find(|h| h["uid"] == uid).unwrap();
        assert_ne!(hit(1)["snippet"], REDACTION_MARKER);
        assert!(hit(1)["snippet"].as_str().unwrap().contains("<mark>"));
    }

    #[tokio::test]
    async fn get_message_withholds_encrypted_content_by_default() {
        let ctx = test_context(false);
        let mut m = message(1, "Secret", "attack at dawn", Some("signed-and-encrypted"));
        m.has_attachments = true;
        m.attachments = vec![EmailAttachment {
            filename: "plan.pdf".into(),
            content_type: "application/pdf".into(),
            size: Some(123),
            part_id: 0,
            content_id: None,
        }];
        ctx.cache.upsert_message(&m).unwrap();

        let args = json!({"account_id": "acc", "folder": "INBOX", "uid": 1});
        let result = invoke(&ctx, "get_message", args.clone()).await.unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["redacted"], true);
        assert_eq!(payload["body"], REDACTION_MARKER);
        assert_eq!(payload["body_source"], "redacted");
        assert_eq!(payload["attachments"].as_array().unwrap().len(), 0);
        assert!(payload["note"].as_str().unwrap().contains("AI settings"));
        // The envelope stays readable — headers aren't encrypted.
        assert_eq!(payload["subject"], "Secret");

        ctx.settings.write().await.mcp_expose_decrypted_content = true;
        let result = invoke(&ctx, "get_message", args).await.unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["redacted"], false);
        assert_eq!(payload["body"], "attack at dawn");
        assert_eq!(payload["body_source"], "text");
        assert_eq!(payload["attachments"].as_array().unwrap().len(), 1);
        assert!(payload.get("note").is_none());
    }

    #[tokio::test]
    async fn get_message_signed_only_is_not_redacted() {
        let ctx = test_context(false);
        ctx.cache
            .upsert_message(&message(
                1,
                "Signed",
                "public but authentic",
                Some("signed"),
            ))
            .unwrap();

        let result = invoke(
            &ctx,
            "get_message",
            json!({"account_id": "acc", "folder": "INBOX", "uid": 1}),
        )
        .await
        .unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["encrypted"], false);
        assert_eq!(payload["redacted"], false);
        assert_eq!(payload["body"], "public but authentic");
    }

    #[tokio::test]
    async fn get_message_strips_html_when_no_text_part() {
        let ctx = test_context(false);
        let mut m = message(1, "Newsletter", "", None);
        m.body_text = None;
        m.body_html = Some(
            "<style>.x{color:red}</style><p>Hello <b>world</b> &amp; friends</p>\
             <script>alert('nope')</script><div>Second line</div>"
                .into(),
        );
        ctx.cache.upsert_message(&m).unwrap();

        let result = invoke(
            &ctx,
            "get_message",
            json!({"account_id": "acc", "folder": "INBOX", "uid": 1}),
        )
        .await
        .unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["body_source"], "html-stripped");
        let body = payload["body"].as_str().unwrap();
        assert!(body.contains("Hello world & friends"));
        assert!(body.contains("Second line"));
        assert!(!body.contains("alert"));
        assert!(!body.contains("color:red"));
    }

    #[tokio::test]
    async fn get_message_missing_from_cache_is_an_error() {
        let ctx = test_context(false);
        let result = invoke(
            &ctx,
            "get_message",
            json!({"account_id": "acc", "folder": "INBOX", "uid": 999}),
        )
        .await;
        let err = result.expect_err("uncached message should error");
        assert!(err.message.contains("not found in the local cache"));
    }

    #[tokio::test]
    async fn get_thread_returns_the_whole_conversation() {
        let ctx = test_context(false);
        ctx.cache
            .upsert_envelopes_for_account(
                "acc",
                &[
                    envelope(1, Some("root@example.com"), &[]),
                    envelope(2, Some("reply@example.com"), &["root@example.com"]),
                    envelope(3, Some("unrelated@example.com"), &[]),
                ],
            )
            .unwrap();

        let result = invoke(
            &ctx,
            "get_thread",
            json!({"account_id": "acc", "folder": "INBOX", "uid": 2}),
        )
        .await
        .unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["message_count"], 2);
        let uids: Vec<u64> = payload["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["uid"].as_u64().unwrap())
            .collect();
        assert!(uids.contains(&1) && uids.contains(&2) && !uids.contains(&3));
    }

    #[tokio::test]
    async fn list_accounts_is_a_sanitized_projection() {
        let ctx = test_context(false);
        account_store::add_account(&ctx.cache, account("acc")).unwrap();

        let result = invoke(&ctx, "list_accounts", json!({})).await.unwrap();
        let raw = result.content[0].as_text().unwrap().text.clone();
        let payload = result_payload(&result);
        let accounts = payload["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["id"], "acc");
        assert_eq!(accounts[0]["display_name"], "Alex Morgan");
        assert_eq!(accounts[0]["email"], "alex@example.com");
        // Connection metadata and key material must not leak.
        assert!(!raw.contains("secret-host"));
        assert!(!raw.contains("993"));
        assert!(!raw.contains("9F2ADEADBEEF"));
    }

    #[tokio::test]
    async fn list_folders_lists_and_rejects_unknown_accounts() {
        let ctx = test_context(false);
        account_store::add_account(&ctx.cache, account("acc")).unwrap();
        ctx.cache
            .upsert_folders(
                "acc",
                &[
                    Folder {
                        name: "INBOX".into(),
                        delimiter: Some("/".into()),
                        attributes: vec![],
                        unread_count: Some(3),
                    },
                    Folder {
                        name: "Drafts".into(),
                        delimiter: Some("/".into()),
                        attributes: vec!["\\Drafts".into()],
                        unread_count: None,
                    },
                ],
            )
            .unwrap();

        let result = invoke(&ctx, "list_folders", json!({"account_id": "acc"}))
            .await
            .unwrap();
        let payload = result_payload(&result);
        let names: Vec<&str> = payload["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["INBOX", "Drafts"]);

        let err = invoke(&ctx, "list_folders", json!({"account_id": "nope"}))
            .await
            .expect_err("unknown account should error");
        assert!(err.message.contains("unknown account_id"));
    }

    #[tokio::test]
    async fn create_draft_validates_before_touching_the_network() {
        let ctx = test_context(false);

        // Unknown account.
        let err = invoke(
            &ctx,
            "create_draft",
            json!({"account_id": "nope", "to": ["r@example.com"]}),
        )
        .await
        .expect_err("unknown account should error");
        assert!(err.message.contains("unknown account_id"));

        // Missing recipients.
        account_store::add_account(&ctx.cache, account("acc")).unwrap();
        let err = invoke(&ctx, "create_draft", json!({"account_id": "acc"}))
            .await
            .expect_err("missing 'to' should error");
        assert!(err.message.contains("'to'"));

        // JMAP accounts are refused with a clear message.
        let mut jmap = account("jmap-acc");
        jmap.use_jmap = true;
        jmap.jmap_url = Some("https://mail.example.com/jmap".into());
        account_store::add_account(&ctx.cache, jmap).unwrap();
        let err = invoke(
            &ctx,
            "create_draft",
            json!({"account_id": "jmap-acc", "to": ["r@example.com"]}),
        )
        .await
        .expect_err("JMAP account should error");
        assert!(err.message.contains("JMAP"));

        // No Drafts folder synced yet.
        let err = invoke(
            &ctx,
            "create_draft",
            json!({"account_id": "acc", "to": ["r@example.com"]}),
        )
        .await
        .expect_err("missing Drafts folder should error");
        assert!(err.message.contains("Drafts"));
    }

    #[tokio::test]
    async fn create_draft_reply_ref_must_be_cached() {
        let ctx = test_context(false);
        account_store::add_account(&ctx.cache, account("acc")).unwrap();
        let err = invoke(
            &ctx,
            "create_draft",
            json!({
                "account_id": "acc",
                "to": ["r@example.com"],
                "reply_to": {"folder": "INBOX", "uid": 42}
            }),
        )
        .await
        .expect_err("uncached reply_to should error");
        assert!(err.message.contains("reply_to"));
    }

    #[test]
    fn draft_outgoing_carries_threading_headers() {
        let account = account("acc");
        let mut parent = message(7, "Original", "body", None);
        // Stored form: bare ids, no angle brackets (#277).
        parent.message_id = Some("root@example.com".into());
        parent.references_ids = vec!["grand@example.com".into()];

        let outgoing = build_draft_outgoing(
            &account,
            vec!["r@example.com".into()],
            vec![],
            vec![],
            "Re: Original".into(),
            "Sounds good.".into(),
            Some(&parent),
        );
        assert_eq!(outgoing.from, "Alex Morgan <alex@example.com>");
        assert_eq!(outgoing.in_reply_to.as_deref(), Some("root@example.com"));
        assert_eq!(
            outgoing.references,
            vec![
                "grand@example.com".to_string(),
                "root@example.com".to_string()
            ]
        );

        // Through the real MIME builder: the wire form gets its
        // angle brackets back and a fresh Message-ID we can later
        // find the APPENDed copy by.
        let raw = build_outgoing_message(&outgoing).unwrap().formatted();
        let raw = String::from_utf8_lossy(&raw).to_string();
        assert!(raw.contains("In-Reply-To: <root@example.com>"));
        assert!(raw.contains("<grand@example.com> <root@example.com>"));
        assert!(raw.contains("Sounds good."));
        assert!(mail_util::extract_message_id(raw.as_bytes()).is_some());
    }

    #[test]
    fn draft_outgoing_without_parent_or_display_name() {
        let mut acc = account("acc");
        acc.display_name = "  ".into();
        let outgoing = build_draft_outgoing(
            &acc,
            vec!["r@example.com".into()],
            vec![],
            vec![],
            "Hi".into(),
            "".into(),
            None,
        );
        assert_eq!(outgoing.from, "alex@example.com");
        assert_eq!(outgoing.in_reply_to, None);
        assert!(outgoing.references.is_empty());
    }

    #[test]
    fn truncation_cuts_on_char_boundaries() {
        let (s, cut) = truncate_chars("äöü".repeat(10), 4);
        assert!(cut);
        assert_eq!(s.chars().count(), 4);
        let (s, cut) = truncate_chars("short".into(), 10);
        assert!(!cut);
        assert_eq!(s, "short");
    }
}
