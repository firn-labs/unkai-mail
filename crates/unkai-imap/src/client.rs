//! IMAP client — connects to a mail server via TLS and provides
//! methods to interact with mailboxes.

use async_imap::Session;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use mail_parser::{MessageParser, MimeHeaders};
use rustls_pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};
use unkai_core::crypto::CryptoBridge;
use unkai_core::error::UnkaiError;
use unkai_core::models::{Email, EmailAttachment, EmailEnvelope, Folder, TrustedCert};
use unkai_core::tls;

use crate::attachment_filename::decode_attachment_filename;

use crate::mutf7;

/// Parse a raw RFC 5322 message into our `Email` shape.  Same MIME
/// walk + transfer-decoding + charset logic `fetch_message` runs
/// against IMAP-fetched bytes, factored out so the Tauri layer can
/// reuse it for offline `.eml` opens (#254) without an account
/// context.  Defaults `is_read = true` and `is_starred = false`
/// because there are no IMAP flags on a file from disk.
///
/// Thin wrapper that always parses as plaintext — equivalent to the
/// `_with_crypto` variant with `bridge = None`.  Existing call sites
/// keep their historical behaviour unchanged.
pub fn parse_eml_bytes(
    raw: &[u8],
    id: &str,
    account_id: &str,
    folder: &str,
) -> Result<Email, UnkaiError> {
    parse_eml_bytes_with_crypto(raw, id, account_id, folder, None)
}

/// Parse a raw RFC 5322 message and, when the caller supplies a
/// [`CryptoBridge`], transparently unwrap an RFC-3156 PGP/MIME
/// envelope before parsing the inner content (#57).
///
/// Detection is opt-in by way of the `bridge` parameter — pass `None`
/// to skip every crypto path and behave identically to
/// [`parse_eml_bytes`].  Pass `Some(&bridge)` to opt into:
///
/// - `multipart/encrypted; protocol="application/pgp-encrypted"` →
///   extract the second (`application/octet-stream`) part, call
///   `bridge.decrypt`, re-parse the recovered plaintext, stamp
///   `protection`, `signature_status`, `signer_fingerprint` from the
///   bridge's outcome.
///
/// - `multipart/signed; protocol="application/pgp-signature"` —
///   currently falls through to plaintext parsing.  Full canonical
///   verification needs access to the on-the-wire signed-body bytes
///   (RFC 3156 §5 canonicalisation) which mail-parser doesn't expose
///   in v0.11; the wrapper-recognising path is in place so adding
///   verification is a localised follow-up, not another receive-path
///   refactor.  TODO(#57): wire detached-signature verification.
///
/// The same call also recognises the S/MIME (X.509 / CMS, #338)
/// envelopes through [`detect_smime_envelope`] / [`apply_smime_envelope`]:
///
/// - `application/pkcs7-mime; smime-type=enveloped-data` → lift the CMS
///   `EnvelopedData` DER out of the part, call `bridge.decrypt_smime`,
///   re-parse the recovered plaintext, stamp `protection = "encrypted"`.
///
/// - `multipart/signed; protocol="application/pkcs7-signature"` —
///   detection-only, same canonicalisation-access limitation as the
///   OpenPGP signed path above; stamps `protection = "signed"`.
///
/// PGP is checked first because the two envelope shapes are mutually
/// exclusive at the top level (a message is either `pgp-*` or `pkcs7-*`,
/// never both), so the order only decides which detector runs the
/// no-op pass on plain mail.
pub fn parse_eml_bytes_with_crypto(
    raw: &[u8],
    id: &str,
    account_id: &str,
    folder: &str,
    bridge: Option<&dyn CryptoBridge>,
) -> Result<Email, UnkaiError> {
    if let Some(b) = bridge {
        if let Some(envelope) = detect_pgp_mime_envelope(raw)? {
            return apply_pgp_envelope(envelope, b, id, account_id, folder, raw);
        }
        if let Some(envelope) = detect_smime_envelope(raw)? {
            return apply_smime_envelope(envelope, b, id, account_id, folder, raw);
        }
    }
    parse_plaintext_eml_bytes(raw, id, account_id, folder)
}

/// Plain-text MIME → `Email`.  The body of [`parse_eml_bytes`] before
/// the PGP/MIME interceptor lifted it out — extracted so the
/// decrypted-plaintext path can recurse into it cleanly.
fn parse_plaintext_eml_bytes(
    raw: &[u8],
    id: &str,
    account_id: &str,
    folder: &str,
) -> Result<Email, UnkaiError> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| UnkaiError::Protocol("Failed to parse message".into()))?;

    let subject = parsed.subject().unwrap_or("").to_string();
    let from = parsed
        .from()
        .and_then(|list| list.first())
        .map(|addr| {
            let name = addr.name().unwrap_or("");
            let email = addr.address().unwrap_or("");
            if name.is_empty() {
                email.to_string()
            } else {
                format!("{name} <{email}>")
            }
        })
        .unwrap_or_default();

    let to = parsed
        .to()
        .map(|list| {
            list.iter()
                .filter_map(|a| a.address().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let cc = parsed
        .cc()
        .map(|list| {
            list.iter()
                .filter_map(|a| a.address().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let body_text = (0..parsed.text_body_count())
        .filter_map(|i| parsed.body_text(i).map(|s| s.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    let body_text = if body_text.is_empty() {
        None
    } else {
        Some(body_text.replace("\r\n", "\n").replace('\r', "\n"))
    };

    let body_html = (0..parsed.html_body_count())
        .filter_map(|i| parsed.body_html(i).map(|s| s.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    let body_html = if body_html.is_empty() {
        None
    } else {
        Some(body_html)
    };

    let has_attachments = parsed.attachment_count() > 0;
    let attachments: Vec<EmailAttachment> = parsed
        .attachments()
        .enumerate()
        .map(|(idx, part)| {
            let part_id = idx as u32;
            let filename = decode_attachment_filename(&parsed, part);
            let content_type = part
                .content_type()
                .map(|ct| {
                    let ctype = ct.ctype();
                    match ct.subtype() {
                        Some(sub) => format!("{ctype}/{sub}"),
                        None => ctype.to_string(),
                    }
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let size = Some(part.contents().len() as u64);
            let content_id = part.content_id().map(|s| s.to_string());
            EmailAttachment {
                filename,
                content_type,
                size,
                part_id,
                content_id,
            }
        })
        .collect();

    let date = parsed
        .date()
        .and_then(|d| {
            DateTime::parse_from_rfc3339(&d.to_rfc3339())
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        })
        .unwrap_or_else(Utc::now);

    // RFC 5322 threading headers (#277).  `mail_parser::Message`
    // exposes Message-ID and References as `Vec<String>` (the
    // headers are repeatable in theory).  In practice
    // `Message-ID:` is unique per message and `In-Reply-To:`
    // is normally a single ID, while `References:` is the
    // ancestor chain.  Brackets are stripped at storage time
    // for consistent comparison.
    let header_first = |name: &str| {
        parsed
            .header(name)
            .and_then(|h| h.as_text())
            .map(str::to_string)
    };
    let message_id = header_first("Message-ID")
        .or_else(|| header_first("Message-Id"))
        .as_deref()
        .and_then(strip_msgid_brackets);
    let in_reply_to = header_first("In-Reply-To")
        .as_deref()
        .and_then(strip_msgid_brackets);
    let references_ids = header_first("References")
        .as_deref()
        .map(parse_references_header)
        .unwrap_or_default();

    Ok(Email {
        id: id.to_string(),
        account_id: account_id.to_string(),
        folder: folder.to_string(),
        from,
        to,
        cc,
        subject,
        body_text,
        body_html,
        date,
        is_read: true,
        is_starred: false,
        has_attachments,
        attachments,
        message_id,
        in_reply_to,
        references_ids,
        // Encryption metadata is populated by the receive-path
        // interceptor in Phase 5 of #57; this parse-only path
        // leaves them as None so legacy callers keep their
        // historical "no chip" rendering.
        protection: None,
        signature_status: None,
        signer_fingerprint: None,
    })
}

/// What we found at the top level of an inbound MIME message when we
/// went looking for an RFC-3156 PGP/MIME envelope.  Used internally by
/// [`parse_eml_bytes_with_crypto`] to decide whether to delegate to the
/// crypto bridge or pass straight through to the plaintext parser.
enum PgpMimeEnvelope {
    /// `multipart/encrypted; protocol="application/pgp-encrypted"`.
    /// The wrapped `Vec<u8>` is the armored ciphertext extracted from
    /// the `application/octet-stream` part (RFC 3156 §4).
    Encrypted { ciphertext_armor: Vec<u8> },
    /// `multipart/signed; protocol="application/pgp-signature"`.
    /// Detection only — actual signature verification is a follow-up
    /// (see TODO on [`parse_eml_bytes_with_crypto`]).  We carry no
    /// payload because nothing downstream looks at it yet; treating
    /// the variant as a marker lets us extend the receive path
    /// without yet another enum reshuffle.
    Signed,
}

/// Look at the top-level Content-Type of `raw` and tell the caller
/// whether they're holding a PGP/MIME envelope.  Returns `Ok(None)`
/// for plain mail (the common case) and `Ok(Some(...))` for the two
/// flavours we recognise.  An `Err` here means we couldn't even
/// parse the headers — the same condition the plaintext path treats
/// as a hard error, so we propagate it unchanged.
fn detect_pgp_mime_envelope(raw: &[u8]) -> Result<Option<PgpMimeEnvelope>, UnkaiError> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| UnkaiError::Protocol("Failed to parse message headers".into()))?;

    let top_ct = match parsed.content_type() {
        Some(ct) => ct,
        None => return Ok(None),
    };
    if !top_ct.ctype().eq_ignore_ascii_case("multipart") {
        return Ok(None);
    }
    let subtype = top_ct.subtype().unwrap_or("");

    // The `protocol` parameter is what distinguishes a PGP/MIME wrapper
    // from a generic multipart/encrypted (e.g. S/MIME would carry
    // `protocol="application/pkcs7-mime"` — handled separately in #338).
    let protocol = top_ct.attribute("protocol").unwrap_or("");

    if subtype.eq_ignore_ascii_case("encrypted")
        && protocol.eq_ignore_ascii_case("application/pgp-encrypted")
    {
        // RFC 3156 §4 fixes the layout: part 1 is `application/pgp-encrypted`
        // (just the `Version: 1` literal), part 2 is the
        // `application/octet-stream` carrying the OpenPGP message.  We
        // scan for the first part whose content-type matches because
        // `mail-parser` flattens nested multiparts and the indices
        // aren't always exactly `(1, 2)` — e.g. when an MUA wraps the
        // envelope inside an extra `multipart/mixed` for an attached
        // public key, which Autocrypt allows.
        let ciphertext = (0..).map_while(|i| parsed.part(i)).find_map(|p| {
            let ct = p.content_type()?;
            if ct.ctype().eq_ignore_ascii_case("application")
                && ct
                    .subtype()
                    .is_some_and(|s| s.eq_ignore_ascii_case("octet-stream"))
            {
                Some(p.contents().to_vec())
            } else {
                None
            }
        });
        return match ciphertext {
            Some(c) => Ok(Some(PgpMimeEnvelope::Encrypted {
                ciphertext_armor: c,
            })),
            None => Err(UnkaiError::Protocol(
                "multipart/encrypted envelope missing application/octet-stream ciphertext part"
                    .into(),
            )),
        };
    }

    if subtype.eq_ignore_ascii_case("signed")
        && protocol.eq_ignore_ascii_case("application/pgp-signature")
    {
        return Ok(Some(PgpMimeEnvelope::Signed));
    }

    Ok(None)
}

/// Apply a detected PGP envelope by calling into the bridge and
/// (for the encrypted case) re-parsing the recovered plaintext as
/// a complete MIME message.  The protection / signature / signer
/// fields on the resulting `Email` carry the bridge's outcome so
/// the UI can render the right status chip in MailView.
fn apply_pgp_envelope(
    envelope: PgpMimeEnvelope,
    bridge: &dyn CryptoBridge,
    id: &str,
    account_id: &str,
    folder: &str,
    raw: &[u8],
) -> Result<Email, UnkaiError> {
    match envelope {
        PgpMimeEnvelope::Encrypted { ciphertext_armor } => {
            let payload = bridge.decrypt(&ciphertext_armor)?;
            let mut email = parse_plaintext_eml_bytes(&payload.plaintext, id, account_id, folder)?;
            // If the inner OpenPGP packets carried a one-pass signature
            // the bridge will have surfaced that as `signature_status` —
            // bump the envelope tag accordingly so the UI can render
            // "signed and decrypted" rather than just "decrypted".
            email.protection = Some(
                if payload.signature_status.is_some() {
                    "signed-and-encrypted"
                } else {
                    "encrypted"
                }
                .to_string(),
            );
            email.signature_status = payload.signature_status;
            email.signer_fingerprint = payload.signer_fingerprint;
            Ok(email)
        }
        PgpMimeEnvelope::Signed => {
            // TODO(#57): canonicalise the signed body part and call
            // `bridge.verify`.  For now we render the message as
            // plaintext and tag it `protection = "signed"` so the
            // UI can still show a "signature detected, not verified
            // yet" chip without crashing on the verify path.
            let mut email = parse_plaintext_eml_bytes(raw, id, account_id, folder)?;
            email.protection = Some("signed".to_string());
            Ok(email)
        }
    }
}

/// The S/MIME (X.509 / CMS, #338) counterpart to [`PgpMimeEnvelope`].
/// What we found at the top level of an inbound message when we went
/// looking for one of the two RFC 8551 wire shapes.
enum SmimeEnvelope {
    /// `application/pkcs7-mime; smime-type=enveloped-data` — the opaque
    /// encrypted form (RFC 8551 §3.2).  The wrapped `Vec<u8>` is the
    /// raw CMS `EnvelopedData` DER, transfer-decoded out of the part by
    /// mail-parser (the part carries `Content-Transfer-Encoding: base64`
    /// on the wire; `contents()` hands us the decoded binary).
    Enveloped { cms_der: Vec<u8> },
    /// `multipart/signed; protocol="application/pkcs7-signature"` — the
    /// detached (clear-signed) form (RFC 8551 §3.4).  Detection only —
    /// CMS signature verification needs the canonical on-the-wire signed
    /// part bytes the same way the OpenPGP `multipart/signed` path does
    /// (see the TODO on [`apply_pgp_envelope`]), so this carries no
    /// payload and the apply step just stamps `protection = "signed"`.
    Signed,
}

/// Look at the top-level Content-Type of `raw` and tell the caller
/// whether they're holding one of the two S/MIME envelope shapes we
/// recognise.  Sibling to [`detect_pgp_mime_envelope`] — kept separate
/// rather than folded into one detector because the wire stacks share
/// no MIME structure (PGP is always `multipart/*`; S/MIME's encrypted
/// form is a bare `application/pkcs7-mime` single part) and keeping the
/// two detectors independent stops one stack's quirks leaking into the
/// other.
///
/// Returns `Ok(None)` for plain mail and for the S/MIME shapes this
/// chunk doesn't handle yet — notably the *opaque* signed form
/// (`application/pkcs7-mime; smime-type=signed-data`), where the body
/// is wrapped inside the CMS `SignedData` and can't be read without
/// unwrapping it (a follow-up once the verify path lands).  We only
/// claim a message we can actually act on, so an unrecognised
/// `smime-type` falls through to plaintext parsing rather than being
/// mislabelled.
///
/// The legacy `x-pkcs7-*` content/protocol spellings (RFC 2633-era
/// senders) are accepted alongside the modern RFC 5751 forms — some
/// MUAs still emit the `x-` variants.
fn detect_smime_envelope(raw: &[u8]) -> Result<Option<SmimeEnvelope>, UnkaiError> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| UnkaiError::Protocol("Failed to parse message headers".into()))?;

    let top_ct = match parsed.content_type() {
        Some(ct) => ct,
        None => return Ok(None),
    };
    let ctype = top_ct.ctype();
    let subtype = top_ct.subtype().unwrap_or("");

    // Encrypted form: `application/pkcs7-mime; smime-type=enveloped-data`.
    // The top-level part is (usually) a bare single part, but some MUAs
    // nest it inside an outer `multipart/mixed` (e.g. to staple a
    // plaintext "this is an encrypted message" note), so we scan every
    // part for the pkcs7-mime body rather than assuming it's at the root
    // — exactly the flattening tolerance the PGP detector applies.
    if is_pkcs7_mime(ctype, subtype) {
        let smime_type = top_ct.attribute("smime-type").unwrap_or("");
        if smime_type.eq_ignore_ascii_case("enveloped-data") {
            let cms_der = (0..).map_while(|i| parsed.part(i)).find_map(|p| {
                let ct = p.content_type()?;
                if is_pkcs7_mime(ct.ctype(), ct.subtype().unwrap_or("")) {
                    Some(p.contents().to_vec())
                } else {
                    None
                }
            });
            return match cms_der {
                Some(der) => Ok(Some(SmimeEnvelope::Enveloped { cms_der: der })),
                None => Err(UnkaiError::Protocol(
                    "application/pkcs7-mime enveloped-data envelope carried no CMS body".into(),
                )),
            };
        }
        // signed-data / certs-only / compressed-data — not handled in
        // this chunk; fall through so the message still renders (as an
        // attachment, the historical behaviour) rather than being
        // mislabelled as something we can decrypt.
        return Ok(None);
    }

    // Detached signed form: `multipart/signed; protocol="application/pkcs7-signature"`.
    if ctype.eq_ignore_ascii_case("multipart") && subtype.eq_ignore_ascii_case("signed") {
        let protocol = top_ct.attribute("protocol").unwrap_or("");
        if protocol.eq_ignore_ascii_case("application/pkcs7-signature")
            || protocol.eq_ignore_ascii_case("application/x-pkcs7-signature")
        {
            return Ok(Some(SmimeEnvelope::Signed));
        }
    }

    Ok(None)
}

/// `true` for both the modern `application/pkcs7-mime` and the legacy
/// `application/x-pkcs7-mime` content type.  Factored out because the
/// enveloped-data detection checks it twice (top-level header, then
/// each part during the flatten scan).
fn is_pkcs7_mime(ctype: &str, subtype: &str) -> bool {
    ctype.eq_ignore_ascii_case("application")
        && (subtype.eq_ignore_ascii_case("pkcs7-mime")
            || subtype.eq_ignore_ascii_case("x-pkcs7-mime"))
}

/// Apply a detected S/MIME envelope.  Counterpart to
/// [`apply_pgp_envelope`]: the encrypted form goes through the bridge's
/// `decrypt_smime`, the inner plaintext is re-parsed as a full MIME
/// message, and the `protection` / `signature_status` / `signer_fingerprint`
/// fields carry the bridge's outcome so MailView renders the same status
/// chip it does for the OpenPGP stack.
fn apply_smime_envelope(
    envelope: SmimeEnvelope,
    bridge: &dyn CryptoBridge,
    id: &str,
    account_id: &str,
    folder: &str,
    raw: &[u8],
) -> Result<Email, UnkaiError> {
    match envelope {
        SmimeEnvelope::Enveloped { cms_der } => {
            let payload = bridge.decrypt_smime(&cms_der)?;
            let mut email = parse_plaintext_eml_bytes(&payload.plaintext, id, account_id, folder)?;
            // `signature_status` is `Some` only once the nested
            // sign-then-encrypt form (RFC 8551 §3.6) is wired through
            // `decrypt_smime`; until then this is always `"encrypted"`.
            // Sharing the exact label set with the PGP path means the UI
            // chip code stays protocol-agnostic.
            email.protection = Some(
                if payload.signature_status.is_some() {
                    "signed-and-encrypted"
                } else {
                    "encrypted"
                }
                .to_string(),
            );
            email.signature_status = payload.signature_status;
            email.signer_fingerprint = payload.signer_fingerprint;
            Ok(email)
        }
        SmimeEnvelope::Signed => {
            // Detection-only, mirroring the OpenPGP `multipart/signed`
            // path: the clear-signed body is already readable, so we
            // render it as plaintext and tag `protection = "signed"`.
            // Actual CMS verification waits on the same canonical-bytes
            // access the PGP verify path is blocked on.
            let mut email = parse_plaintext_eml_bytes(raw, id, account_id, folder)?;
            email.protection = Some("signed".to_string());
            Ok(email)
        }
    }
}

/// #341 follow-up to #57 — pull the bytes of a single attachment out
/// of a PGP/MIME encrypted message, decrypting through the supplied
/// bridge so the part_id indexes into the *decrypted inner MIME tree*
/// rather than the encrypted outer envelope.
///
/// Counterpart to `IMAPClient::fetch_attachment` for the encrypted
/// case.  The receive path parses the decrypted plaintext and stamps
/// `EmailAttachment.part_id` as a sequential index into the *inner*
/// tree (the real user attachments), but `fetch_attachment` re-parses
/// the raw IMAP bytes — which are still the `multipart/encrypted`
/// outer envelope — and walks those with the inner index.  For a
/// real attachment at inner `part_id = 0` that lookup returns the
/// outer envelope's `application/pgp-encrypted` "Version: 1" header
/// instead of the actual file bytes.  This function bridges the gap
/// by decrypting first and walking the inner tree with the same
/// `attachments()` / `parts` fallback `fetch_attachment` uses.
///
/// Handles both the OpenPGP `multipart/encrypted` and the S/MIME
/// `application/pkcs7-mime; smime-type=enveloped-data` shapes — the
/// inner-tree walk is identical once the bridge has handed back the
/// decrypted plaintext, so the only stack-specific step is which
/// detector + bridge call produces those bytes.
///
/// Returns `Ok(None)` if `raw` isn't an encrypted envelope of either
/// stack (including a `multipart/signed` clear-signed message, whose
/// parts are already in the clear) so the caller can fall back to the
/// plaintext path.  Returns `Err(Protocol)` when the inner tree doesn't
/// carry the requested `part_id` — typically a sign the caller is mixing
/// inner / outer indices and should be routed through `fetch_attachment`
/// instead.
pub fn extract_decrypted_attachment(
    raw: &[u8],
    bridge: &dyn CryptoBridge,
    part_id: u32,
) -> Result<Option<(EmailAttachment, Vec<u8>)>, UnkaiError> {
    // Resolve the decrypted inner MIME bytes from whichever encrypted
    // stack this message uses.  `multipart/signed` (either stack) and
    // plain mail return `Ok(None)` so the caller drops to the regular
    // attachment path.
    let plaintext = if let Some(envelope) = detect_pgp_mime_envelope(raw)? {
        match envelope {
            PgpMimeEnvelope::Encrypted { ciphertext_armor } => {
                bridge.decrypt(&ciphertext_armor)?.plaintext
            }
            PgpMimeEnvelope::Signed => return Ok(None),
        }
    } else if let Some(envelope) = detect_smime_envelope(raw)? {
        match envelope {
            SmimeEnvelope::Enveloped { cms_der } => bridge.decrypt_smime(&cms_der)?.plaintext,
            SmimeEnvelope::Signed => return Ok(None),
        }
    } else {
        return Ok(None);
    };
    let parsed = MessageParser::default()
        .parse(plaintext.as_slice())
        .ok_or_else(|| UnkaiError::Protocol("Failed to parse decrypted message".into()))?;

    // Same primary-then-fallback lookup `fetch_attachment` uses so
    // a part_id assigned during the listing parse always resolves
    // to the same byte slice during fetch — wherever the index was
    // first stamped from.
    let part = parsed
        .attachment(part_id)
        .or_else(|| parsed.parts.get(part_id as usize))
        .ok_or_else(|| {
            UnkaiError::Protocol(format!("Decrypted inner MIME tree has no part #{part_id}"))
        })?;

    let filename = decode_attachment_filename(&parsed, part);
    let content_type = part
        .content_type()
        .map(|ct| {
            let ctype = ct.ctype();
            match ct.subtype() {
                Some(sub) => format!("{ctype}/{sub}"),
                None => ctype.to_string(),
            }
        })
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let data = part.contents().to_vec();
    let size = Some(data.len() as u64);
    let content_id = part.content_id().map(|s| s.to_string());
    Ok(Some((
        EmailAttachment {
            filename,
            content_type,
            size,
            part_id,
            content_id,
        },
        data,
    )))
}

/// `async-imap`'s `Session` is generic over its underlying I/O. We
/// pin the alias to the concrete `Compat<TlsStream<TcpStream>>` so
/// downstream callers don't have to think about the four layers of
/// generics — and so the `session: Option<...>` field below has a
/// nameable type.
type ImapSession = Session<Compat<TlsStream<TcpStream>>>;

/// Encode a UTF-8 mailbox name into the IMAP Modified UTF-7 form that
/// `SELECT` / `EXAMINE` / `STATUS` / `APPEND` etc. expect on the wire.
/// Pure ASCII names round-trip unchanged so this is a no-op for the
/// common case (`INBOX`, `Sent`, `Drafts`, …).
///
/// **Quoting is the caller's responsibility — but the `async-imap`
/// crate handles it for most commands.**  `select`, `examine`,
/// `create`, `delete`, `subscribe`, `unsubscribe`, `status`,
/// `append`, `rename`, `list`, etc. all run their mailbox argument
/// through `validate_str` internally, which calls the `quote!`
/// macro and emits `"<name>"` on the wire when needed.  The one
/// exception is `uid_copy` (and `uid_move`), which passes the
/// mailbox argument straight through to the wire — names with
/// atom-special characters (space, `(`, `)`, `{`, `*`, `%`, `\`,
/// `"`, controls) get parsed up to the first such char and the
/// rest becomes syntax junk.  Use [`quoted_mailbox_arg`] at those
/// call sites; everywhere else, hand the bare `to_wire` result
/// straight to async-imap and let its built-in quoter do its job.
fn to_wire(name: &str) -> String {
    mutf7::encode(name)
}

/// Wrap a `to_wire`-encoded mailbox name in IMAP quoted-string
/// form (`"..."`, with `\` and `"` backslash-escaped) when it
/// contains any RFC 3501 atom-special character.  Pure-ASCII
/// names without specials round-trip unquoted.  Used **only**
/// for `uid_copy` / `uid_move` arguments — async-imap doesn't
/// auto-quote those, and a folder named `"Audi TT"` would
/// otherwise be parsed by the server as the bare atom `Audi`
/// with `TT` becoming dangling syntax.
fn quoted_mailbox_arg(name: &str) -> String {
    let encoded = to_wire(name);
    let needs_quoting = encoded.bytes().any(|b| {
        matches!(
            b,
            b' ' | b'(' | b')' | b'{' | b'*' | b'%' | b'\\' | b'"' | 0x00..=0x1f | 0x7f
        )
    });
    if !needs_quoting {
        return encoded;
    }
    let mut out = String::with_capacity(encoded.len() + 2);
    out.push('"');
    for c in encoded.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Open a TCP+TLS connection to the IMAP server, returning a stream
/// adapted to the `futures-io` traits that `async-imap` expects.
async fn tls_connect(
    host: &str,
    port: u16,
    trusted_certs: &[TrustedCert],
) -> Result<Compat<TlsStream<TcpStream>>, UnkaiError> {
    let addr = format!("{host}:{port}");
    let tcp = TcpStream::connect(&addr)
        .await
        .map_err(|e| UnkaiError::Network(format!("Failed to connect to {addr}: {e}")))?;
    debug!("TCP connection established to {addr}");

    let config = tls::build_client_config(trusted_certs);
    let connector = TlsConnector::from(config);
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| UnkaiError::Protocol(format!("invalid IMAP hostname '{host}': {e}")))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| UnkaiError::Network(format!("TLS handshake failed with {host}: {e}")))?;
    debug!("TLS handshake completed");

    Ok(tls.compat())
}

/// Probe the IMAP server's TLS certificate without verifying it.
/// Used by the "trust this server?" flow: when the regular connect
/// fails because the cert isn't in any trust store we know about,
/// the UI calls this to capture the chain (leaf + intermediates)
/// so the user can be shown the fingerprints and decide whether to
/// trust the server.
///
/// Returns every cert the server presented in handshake order
/// (leaf first, then intermediates). Trusting the whole chain — not
/// just the leaf — is the robust thing to do: the server may
/// reorder certs, the active leaf may be reissued under the same
/// intermediate, and the verifier matches against the trust list
/// by walking the entire presented chain anyway. Caller is
/// responsible for never using this for actual mail traffic — we
/// drop the connection immediately after the handshake succeeds.
pub async fn probe_server_certificate(host: &str, port: u16) -> Result<Vec<Vec<u8>>, UnkaiError> {
    let addr = format!("{host}:{port}");
    let tcp = TcpStream::connect(&addr)
        .await
        .map_err(|e| UnkaiError::Network(format!("Failed to connect to {addr}: {e}")))?;

    let connector = TlsConnector::from(tls::no_verify_config());
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| UnkaiError::Protocol(format!("invalid IMAP hostname '{host}': {e}")))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| UnkaiError::Network(format!("TLS probe failed with {host}: {e}")))?;

    let (_io, conn) = tls.get_ref();
    let chain: Vec<Vec<u8>> = conn
        .peer_certificates()
        .map(|certs| certs.iter().map(|c| c.to_vec()).collect())
        .unwrap_or_default();
    if chain.is_empty() {
        return Err(UnkaiError::Protocol(format!(
            "server '{host}' returned no certificate"
        )));
    }
    Ok(chain)
}

use tracing::{debug, info, warn};

/// An authenticated IMAP session, ready to interact with mailboxes.
///
/// # Usage
/// ```ignore
/// let client = ImapClient::connect("imap.example.com", 993, "user@example.com", "password").await?;
/// let folders = client.list_folders().await?;
/// client.logout().await?;
/// ```
pub struct ImapClient {
    /// The underlying async-imap session, wrapped in TLS.
    /// `Option` so we can take it out during logout.
    session: Option<ImapSession>,
}

/// Result of a sync fetch — envelopes plus the folder's `UIDVALIDITY`.
///
/// Callers store the `uidvalidity` alongside the envelopes. On the next
/// sync they compare the server's value against the stored one; if it
/// changed, the cached UIDs point at different messages (or no messages)
/// and the folder's local cache must be wiped and rebuilt.
#[derive(Debug, Clone)]
pub struct EnvelopeBatch {
    pub uidvalidity: Option<u32>,
    pub envelopes: Vec<EmailEnvelope>,
}

/// IMAP system-flag snapshot for a single message UID.
///
/// Returned by `fetch_flags`, which exists to refresh the
/// `\Seen` / `\Flagged` / `\Answered` bits on already-cached
/// envelopes — the standard envelope-fetch path is incremental and
/// doesn't re-read flags on UIDs the cache already knows about, so
/// flag changes another mail client makes (mark-read on a phone,
/// answer from webmail, etc.) need this catch-up to round-trip.
#[derive(Debug, Clone, Copy)]
pub struct FlagSnapshot {
    pub uid: u32,
    pub is_read: bool,
    pub is_starred: bool,
    pub is_answered: bool,
}

impl ImapClient {
    /// Connect to an IMAP server over TLS and log in.
    ///
    /// `trusted_certs` is the per-account list of additional roots
    /// (the user's self-signed certs they've explicitly trusted in
    /// settings). Empty for "trust webpki-roots only" — the
    /// historical behaviour.
    pub async fn connect(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        trusted_certs: &[TrustedCert],
    ) -> Result<Self, UnkaiError> {
        info!(host, port, username, "Connecting to IMAP server");

        let stream = tls_connect(host, port, trusted_certs).await?;
        let imap_client = async_imap::Client::new(stream);

        let session = imap_client.login(username, password).await.map_err(|e| {
            // login() returns (error, client) on failure — we only need the error
            UnkaiError::Auth(format!("IMAP login failed: {}", e.0))
        })?;

        info!("Successfully logged in as {username}");

        Ok(Self {
            session: Some(session),
        })
    }

    /// List all folders (mailboxes) on the server.
    ///
    /// Uses the IMAP `LIST` command with a wildcard to get everything.
    /// Each folder comes back with a name, hierarchy delimiter, and attributes
    /// (like \Sent, \Trash, etc.) that tell us what the folder is for.
    pub async fn list_folders(&mut self) -> Result<Vec<Folder>, UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        // LIST "" "*" means: starting from root (""), list all folders ("*")
        // This returns an async Stream, so we collect all results with try_collect().
        let mailboxes: Vec<_> = session
            .list(Some(""), Some("*"))
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to list folders: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read folder list: {e}")))?;

        // Build folder list, then query each folder for its unread count.
        // Mailbox names come over the wire in IMAP Modified UTF-7
        // (RFC 3501 §5.1.3) — decode them to plain UTF-8 here so the
        // cache and the UI never see the encoded form. We re-encode
        // when sending names back to the server (`STATUS`, `SELECT`,
        // `APPEND`, etc.) via `to_wire`.
        let mut folders: Vec<Folder> = mailboxes
            .iter()
            .map(|mailbox| {
                let attributes = mailbox
                    .attributes()
                    .iter()
                    .map(|attr| format!("{attr:?}"))
                    .collect();

                Folder {
                    name: mutf7::decode(mailbox.name()),
                    delimiter: mailbox.delimiter().map(|d| d.to_string()),
                    attributes,
                    unread_count: None,
                }
            })
            .collect();

        // For each folder, ask the server for the UNSEEN count via STATUS.
        // STATUS returns the *number* of unseen messages (unlike SELECT/EXAMINE
        // where `unseen` is the sequence number of the first unseen message).
        for folder in &mut folders {
            let wire_name = to_wire(&folder.name);
            match session.status(&wire_name, "(UNSEEN)").await {
                Ok(mailbox_status) => {
                    folder.unread_count = mailbox_status.unseen;
                    debug!(
                        "  Folder: {} — unread: {:?} (attrs: {:?})",
                        folder.name, folder.unread_count, folder.attributes
                    );
                }
                Err(e) => {
                    // Some folders (e.g. \Noselect) don't support STATUS — that's fine,
                    // we just leave unread_count as None.
                    debug!(
                        "  Folder: {} — could not get STATUS: {e} (attrs: {:?})",
                        folder.name, folder.attributes
                    );
                }
            }
        }

        info!("Found {} folders", folders.len());
        Ok(folders)
    }

    /// Create a new mailbox on the server via IMAP `CREATE`.
    ///
    /// `name` is the full hierarchy path in display form (e.g.
    /// `"Projects"` for a top-level folder, `"INBOX/Projects/2026"`
    /// for a subfolder using the `/` delimiter that most servers
    /// report via LIST). We re-encode to Modified UTF-7 on the wire
    /// via `to_wire` — the same path every other mailbox-naming
    /// command uses — so non-ASCII folder names round-trip correctly.
    pub async fn create_folder(&mut self, name: &str) -> Result<(), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;
        session
            .create(to_wire(name))
            .await
            .map_err(|e| UnkaiError::Protocol(format!("CREATE '{name}' failed: {e}")))?;
        info!("Created mailbox '{name}'");
        Ok(())
    }

    /// Delete a mailbox via IMAP `DELETE`.
    ///
    /// Most servers refuse to delete a folder that still holds
    /// messages — the error bubbles up to the UI unchanged so the
    /// user sees a real reason ("Mailbox has children" / "Mailbox
    /// is not empty"). Callers that want "delete even if full"
    /// semantics should first move the messages to Trash.
    pub async fn delete_folder(&mut self, name: &str) -> Result<(), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;
        session
            .delete(to_wire(name))
            .await
            .map_err(|e| UnkaiError::Protocol(format!("DELETE '{name}' failed: {e}")))?;
        info!("Deleted mailbox '{name}'");
        Ok(())
    }

    /// Rename a mailbox via IMAP `RENAME`.
    ///
    /// The server rewrites all UIDs server-side but keeps messages
    /// intact; our local cache needs a parallel update so envelopes
    /// and bodies that were stored under the old name carry over
    /// to the new one. That's handled in the caller (`main.rs`)
    /// via `Cache::rename_folder` — this method only drives the
    /// IMAP side.
    pub async fn rename_folder(&mut self, from: &str, to: &str) -> Result<(), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;
        session
            .rename(to_wire(from), to_wire(to))
            .await
            .map_err(|e| UnkaiError::Protocol(format!("RENAME '{from}' -> '{to}' failed: {e}")))?;
        info!("Renamed mailbox '{from}' -> '{to}'");
        Ok(())
    }

    /// Select a folder for reading (uses EXAMINE — read-only, no state changes).
    ///
    /// In IMAP you must SELECT (or EXAMINE) a folder before you can fetch messages
    /// from it. EXAMINE is like SELECT but opens the mailbox read-only, so marking
    /// messages as seen, etc. won't happen as a side effect. Returns the number
    /// of messages (`exists`) and the folder's `UIDVALIDITY` — a server-assigned
    /// counter that changes whenever the folder is recreated or its UID space
    /// resets. Callers compare this against a cached copy to detect when their
    /// cached UIDs are no longer valid.
    async fn select_folder(&mut self, folder: &str) -> Result<(u32, Option<u32>), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        let mailbox = session.examine(to_wire(folder)).await.map_err(|e| {
            UnkaiError::Protocol(format!("Failed to select folder '{folder}': {e}"))
        })?;

        info!(
            "Selected '{folder}' ({} messages, uidvalidity={:?})",
            mailbox.exists, mailbox.uid_validity
        );
        Ok((mailbox.exists, mailbox.uid_validity))
    }

    /// Fetch envelopes for the mail list.
    ///
    /// `since_uid` toggles the strategy:
    ///
    /// - `None` → full mode: pull the newest `limit` messages by sequence number.
    ///   Used on a cold cache or after a UIDVALIDITY reset.
    /// - `Some(u)` → incremental mode: pull everything with UID `> u` via
    ///   `UID FETCH (u+1):*`. Cheap because only genuinely new messages come
    ///   back; the cache already has everything up to `u`.
    ///
    /// Returns the folder's `UIDVALIDITY` alongside the envelopes so the caller
    /// can notice when the server has invalidated its cached UIDs.
    ///
    /// IMAP messages have two kinds of identifiers:
    /// - **sequence numbers**: 1..N in current session, change as messages are deleted
    /// - **UIDs**: stable across sessions — this is what we store and return
    pub async fn fetch_envelopes(
        &mut self,
        folder: &str,
        limit: u32,
        since_uid: Option<u32>,
    ) -> Result<EnvelopeBatch, UnkaiError> {
        let (total, uidvalidity) = self.select_folder(folder).await?;
        if total == 0 {
            return Ok(EnvelopeBatch {
                uidvalidity,
                envelopes: Vec::new(),
            });
        }

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        // Two FETCH forms depending on mode. `uid_fetch` uses UIDs directly
        // (survives server-side deletions), while `fetch` uses sequence numbers
        // — the only way to say "newest N" without knowing UIDs in advance.
        let fetches: Vec<_> = match since_uid {
            Some(hi) => {
                // `hi+1:*` — everything strictly newer than the last UID we saw.
                // `*` means "the largest UID in the folder", so this always
                // terminates even when there's nothing new (returns empty).
                let range = format!("{}:*", hi.saturating_add(1));
                debug!("Incremental UID FETCH {folder} range={range}");
                session
                    .uid_fetch(
                        range,
                        "(UID FLAGS INTERNALDATE ENVELOPE \
                         BODY.PEEK[HEADER.FIELDS (REFERENCES CONTENT-TYPE)])",
                    )
                    .await
                    .map_err(|e| UnkaiError::Protocol(format!("UID FETCH failed: {e}")))?
                    .try_collect()
                    .await
                    .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID FETCH: {e}")))?
            }
            None => {
                // Newest `limit` by sequence number. Higher seq = newer.
                let start = total.saturating_sub(limit.saturating_sub(1)).max(1);
                let range = format!("{start}:{total}");
                debug!("Full FETCH {folder} range={range}");
                session
                    .fetch(
                        &range,
                        "(UID FLAGS INTERNALDATE ENVELOPE \
                         BODY.PEEK[HEADER.FIELDS (REFERENCES CONTENT-TYPE)])",
                    )
                    .await
                    .map_err(|e| UnkaiError::Protocol(format!("FETCH failed: {e}")))?
                    .try_collect()
                    .await
                    .map_err(|e| {
                        UnkaiError::Protocol(format!("Failed to read FETCH response: {e}"))
                    })?
            }
        };

        let mut envelopes: Vec<EmailEnvelope> = fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?;
                let envelope = fetch.envelope()?;

                // Subject — decode the RFC 2047 header if needed. async-imap
                // returns raw bytes; mail-parser's header_to_string handles
                // the encoded-word decoding for us.
                let subject = envelope
                    .subject
                    .as_ref()
                    .map(|s| decode_header(s))
                    .unwrap_or_default();

                // From — take the first address, formatted as "Name <addr>"
                let from = envelope
                    .from
                    .as_ref()
                    .and_then(|addrs| addrs.first())
                    .map(format_address)
                    .unwrap_or_default();

                let date = envelope
                    .date
                    .as_ref()
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .and_then(parse_rfc2822)
                    .or_else(|| {
                        fetch.internal_date().map(|dt| {
                            // INTERNALDATE is a chrono::DateTime<FixedOffset>; convert to UTC
                            dt.with_timezone(&Utc)
                        })
                    })
                    .unwrap_or_else(Utc::now);

                // Flags: \Seen → read, \Flagged → starred,
                // \Answered → user-or-other-client replied to this
                // message (#255).
                let mut is_read = false;
                let mut is_starred = false;
                let mut is_answered = false;
                for flag in fetch.flags() {
                    match flag {
                        async_imap::types::Flag::Seen => is_read = true,
                        async_imap::types::Flag::Flagged => is_starred = true,
                        async_imap::types::Flag::Answered => is_answered = true,
                        _ => {}
                    }
                }

                let (message_id, in_reply_to, references_ids) = extract_threading_headers(fetch);
                // #341 background-decrypt: detect a PGP/MIME envelope
                // from the Content-Type header we pulled in the same
                // FETCH — lets the mail-list lock chip appear the
                // moment new mail arrives, instead of waiting for the
                // user to open the message once.
                let protection = extract_envelope_protection(fetch);

                Some(EmailEnvelope {
                    uid,
                    folder: folder.to_string(),
                    from,
                    subject,
                    date,
                    is_read,
                    is_starred,
                    is_answered,
                    // The IMAP client doesn't track *how* the user
                    // replied — that's Unkai-only metadata stamped
                    // by the send path (#255), so leave it None
                    // here; the cache merge preserves whatever's
                    // already on disk.
                    replied_kind: None,
                    // The IMAP client doesn't carry the account id; the
                    // caller stamps it into the cache via
                    // `upsert_envelopes_for_account`, and cache reads
                    // populate the field on the way back out.
                    account_id: String::new(),
                    message_id,
                    in_reply_to,
                    references_ids,
                    // #334: cache populates these on upsert; off-the-wire
                    // envelopes don't know their thread identity yet.
                    thread_id: None,
                    thread_total_count: None,
                    protection,
                })
            })
            .collect();

        // Server returns oldest-first within our range; reverse so newest is first
        envelopes.reverse();

        info!(
            "Fetched {} envelopes from '{folder}' ({})",
            envelopes.len(),
            if since_uid.is_some() {
                "incremental"
            } else {
                "full"
            }
        );
        Ok(EnvelopeBatch {
            uidvalidity,
            envelopes,
        })
    }

    /// Fetch the raw RFC 5322 bytes for a single message, with no
    /// parsing.  Used by the encrypted-message decrypt flow (#57) at
    /// the Tauri layer: it builds a `CryptoBridge` from the user's
    /// freshly-prompted passphrase and feeds the bytes here to
    /// `parse_eml_bytes_with_crypto` so the decryption + re-parse
    /// happens in one place.  Mirrors the wire shape of
    /// `fetch_message` exactly (UID FETCH BODY.PEEK[]) — same FOLDER
    /// SELECT cost, same `MessageGone` semantics, just no parse step.
    pub async fn fetch_raw_message(
        &mut self,
        folder: &str,
        uid: u32,
    ) -> Result<Vec<u8>, UnkaiError> {
        let _ = self.select_folder(folder).await?;

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        let fetches: Vec<_> = session
            .uid_fetch(uid.to_string(), "(BODY.PEEK[])")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID FETCH failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID FETCH: {e}")))?;

        let fetch = fetches.into_iter().next().ok_or(UnkaiError::MessageGone)?;
        let raw = fetch
            .body()
            .ok_or_else(|| UnkaiError::Protocol("FETCH returned no body".into()))?;
        Ok(raw.to_vec())
    }

    /// Fetch a single full message (headers + body) by its UID.
    ///
    /// This uses UID FETCH BODY.PEEK[] to grab the entire raw RFC 5322 message,
    /// then hands it to `mail-parser` to split out text/HTML parts, decode
    /// transfer encodings (base64, quoted-printable), and convert charsets.
    ///
    /// BODY.PEEK[] is used instead of BODY[] so the server does NOT mark the
    /// message as \Seen — we want marking-as-read to be an explicit action.
    pub async fn fetch_message(
        &mut self,
        folder: &str,
        uid: u32,
        account_id: &str,
    ) -> Result<Email, UnkaiError> {
        let _ = self.select_folder(folder).await?;

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        let fetches: Vec<_> = session
            .uid_fetch(uid.to_string(), "(UID FLAGS INTERNALDATE BODY.PEEK[])")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID FETCH failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID FETCH: {e}")))?;

        // No fetch row back means the server's expunged this UID
        // since we cached the envelope — surface `MessageGone` so the
        // Tauri layer can evict the dead row + the UI can auto-advance.
        let fetch = fetches.into_iter().next().ok_or(UnkaiError::MessageGone)?;

        let raw = fetch
            .body()
            .ok_or_else(|| UnkaiError::Protocol("FETCH returned no body".into()))?;

        // mail-parser does the heavy lifting: MIME tree, charset decoding, etc.
        let parsed = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| UnkaiError::Protocol("Failed to parse message".into()))?;

        let subject = parsed.subject().unwrap_or("").to_string();
        let from = parsed
            .from()
            .and_then(|list| list.first())
            .map(|addr| {
                let name = addr.name().unwrap_or("");
                let email = addr.address().unwrap_or("");
                if name.is_empty() {
                    email.to_string()
                } else {
                    format!("{name} <{email}>")
                }
            })
            .unwrap_or_default();

        let to = parsed
            .to()
            .map(|list| {
                list.iter()
                    .filter_map(|a| a.address().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let cc = parsed
            .cc()
            .map(|list| {
                list.iter()
                    .filter_map(|a| a.address().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // body_text: concatenate all text/plain parts (usually just one).
        // body_html: same for text/html. Either may be absent.
        //
        // mail-parser returns text with CRLF (\r\n) line endings as required
        // by the MIME RFC. We normalise to LF-only here so the frontend's
        // `white-space: pre-wrap` renders line breaks correctly — some
        // WebKit builds treat a bare \r as a carriage-return (cursor-to-BOL)
        // rather than a newline, collapsing multi-line text onto one line.
        let body_text = (0..parsed.text_body_count())
            .filter_map(|i| parsed.body_text(i).map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        let body_text = if body_text.is_empty() {
            None
        } else {
            Some(body_text.replace("\r\n", "\n").replace('\r', "\n"))
        };

        let body_html = (0..parsed.html_body_count())
            .filter_map(|i| parsed.body_html(i).map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        let body_html = if body_html.is_empty() {
            None
        } else {
            Some(body_html)
        };

        let has_attachments = parsed.attachment_count() > 0;

        // Metadata for each attachment. We store only name/type/size
        // here — the bytes are left on the server and fetched on demand
        // when the user clicks "Download" or "Save to Nextcloud". This
        // keeps the message payload (and its cache row) small even for
        // messages with 20 MB of PDFs.
        let attachments: Vec<EmailAttachment> = parsed
            .attachments()
            .enumerate()
            .map(|(idx, part)| {
                let part_id = idx as u32;
                let filename = decode_attachment_filename(&parsed, part);
                // `content_type()` returns a structured ContentType;
                // rebuild the `type/subtype` string for the UI icon lookup.
                let content_type = part
                    .content_type()
                    .map(|ct| {
                        let ctype = ct.ctype();
                        match ct.subtype() {
                            Some(sub) => format!("{ctype}/{sub}"),
                            None => ctype.to_string(),
                        }
                    })
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                // Decoded contents length. mail-parser has already
                // resolved base64/QP by the time we see `contents()`,
                // so this matches what the user will actually download.
                let size = Some(part.contents().len() as u64);
                // RFC 2392 Content-ID, when the part carried one. The
                // body's `<a href="cid:abc-123">` anchors resolve to
                // this attachment via case-insensitive equality with
                // the cid value (no angle brackets — mail-parser
                // strips them already).
                let content_id = part.content_id().map(|s| s.to_string());
                EmailAttachment {
                    filename,
                    content_type,
                    size,
                    part_id,
                    content_id,
                }
            })
            .collect();

        let date = parsed
            .date()
            .and_then(|d| {
                // mail_parser::DateTime -> RFC3339 string -> chrono
                DateTime::parse_from_rfc3339(&d.to_rfc3339())
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            })
            .or_else(|| fetch.internal_date().map(|dt| dt.with_timezone(&Utc)))
            .unwrap_or_else(Utc::now);

        let mut is_read = false;
        let mut is_starred = false;
        for flag in fetch.flags() {
            match flag {
                async_imap::types::Flag::Seen => is_read = true,
                async_imap::types::Flag::Flagged => is_starred = true,
                _ => {}
            }
        }

        info!(
            "Fetched message UID {uid} from '{folder}' ({} bytes, {} attachments)",
            raw.len(),
            parsed.attachment_count()
        );

        // RFC 5322 threading headers — same parse as in
        // `parse_eml_bytes`.  Mirror that helper inline because
        // the two paths walk slightly different `parsed` shapes.
        let header_first = |name: &str| {
            parsed
                .header(name)
                .and_then(|h| h.as_text())
                .map(str::to_string)
        };
        let message_id = header_first("Message-ID")
            .or_else(|| header_first("Message-Id"))
            .as_deref()
            .and_then(strip_msgid_brackets);
        let in_reply_to = header_first("In-Reply-To")
            .as_deref()
            .and_then(strip_msgid_brackets);
        let references_ids = header_first("References")
            .as_deref()
            .map(parse_references_header)
            .unwrap_or_default();

        // PGP/MIME detection (#57).  Stamping `protection` here even
        // without a bridge lets MailView render a Decrypt affordance
        // and the inline chips instead of silently falling back to an
        // empty body (which is what the user sees today when an
        // encrypted message lands — the application/octet-stream
        // ciphertext has no text/plain peer, so `body_text` is
        // `None`).  The Tauri layer's `decrypt_message` command will
        // re-fetch with a bridge to actually populate the body.
        let protection = match detect_pgp_mime_envelope(raw)? {
            Some(PgpMimeEnvelope::Encrypted { .. }) => Some("encrypted".to_string()),
            Some(PgpMimeEnvelope::Signed) => Some("signed".to_string()),
            None => None,
        };

        Ok(Email {
            id: format!("{folder}:{uid}"),
            account_id: account_id.to_string(),
            folder: folder.to_string(),
            from,
            to,
            cc,
            subject,
            body_text,
            body_html,
            date,
            is_read,
            is_starred,
            has_attachments,
            attachments,
            message_id,
            in_reply_to,
            references_ids,
            protection,
            // Inner-signature fields stay None until a bridge is
            // available — the unauthenticated fetch path can only
            // tell the user "this is encrypted", not "and signed by
            // X".  `decrypt_message` overwrites all three once the
            // passphrase comes in.
            signature_status: None,
            signer_fingerprint: None,
        })
    }

    /// Fetch the raw decoded bytes of a single attachment.
    ///
    /// We re-fetch the whole message body (BODY.PEEK[]) and re-parse it
    /// to extract the attachment at `part_id`. That's simpler than
    /// issuing a targeted BODYSTRUCTURE + BODY[part] pair, which would
    /// mean teaching the UI about MIME section numbers — and re-fetching
    /// is cheap enough for the "user clicked Download" case. BODY.PEEK[]
    /// keeps the message unread.
    /// Find any iCalendar payload in the message and return its
    /// raw bytes — regardless of whether mail-parser classified
    /// it as an attachment or a body alternative.  Walks
    /// `Message::parts` directly so canonical iMIP messages
    /// (where `text/calendar` lives inside
    /// `multipart/alternative` with no separate `.ics`
    /// download) still surface their calendar payload.
    /// Returns `None` when no calendar-shaped part exists in
    /// the message — caller treats that as "this isn't an
    /// invite mail at all".
    pub async fn fetch_calendar_payload(
        &mut self,
        folder: &str,
        uid: u32,
    ) -> Result<Option<Vec<u8>>, UnkaiError> {
        let _ = self.select_folder(folder).await?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;
        let fetches: Vec<_> = session
            .uid_fetch(uid.to_string(), "(BODY.PEEK[])")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID FETCH failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID FETCH: {e}")))?;
        let fetch = fetches
            .into_iter()
            .next()
            .ok_or_else(|| UnkaiError::Protocol(format!("No message with UID {uid}")))?;
        let raw = fetch
            .body()
            .ok_or_else(|| UnkaiError::Protocol("FETCH returned no body".into()))?;
        let parsed = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| UnkaiError::Protocol("Failed to parse message".into()))?;
        for part in parsed.parts.iter() {
            let ct = match part.content_type() {
                Some(ct) => ct,
                None => continue,
            };
            let ctype = ct.ctype().to_ascii_lowercase();
            let subtype = ct
                .subtype()
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let is_calendar = (ctype == "text" && subtype == "calendar")
                || (ctype == "application" && (subtype == "ics" || subtype == "ical"));
            if is_calendar {
                return Ok(Some(part.contents().to_vec()));
            }
        }
        Ok(None)
    }

    pub async fn fetch_attachment(
        &mut self,
        folder: &str,
        uid: u32,
        part_id: u32,
    ) -> Result<(EmailAttachment, Vec<u8>), UnkaiError> {
        let _ = self.select_folder(folder).await?;

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        let fetches: Vec<_> = session
            .uid_fetch(uid.to_string(), "(BODY.PEEK[])")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID FETCH failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID FETCH: {e}")))?;

        let fetch = fetches
            .into_iter()
            .next()
            .ok_or_else(|| UnkaiError::Protocol(format!("No message with UID {uid}")))?;

        let raw = fetch
            .body()
            .ok_or_else(|| UnkaiError::Protocol("FETCH returned no body".into()))?;

        let parsed = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| UnkaiError::Protocol("Failed to parse message".into()))?;

        // Try mail-parser's `attachments()` iterator first
        // (matches the listing path's primary indexing) and
        // fall back to the parts-array.  The fallback rescues
        // metadata that was cached during an earlier build
        // where part_ids referenced the parts-array directly —
        // without it those legacy entries fail to download
        // and any UI keying off `download_email_attachment`
        // (RSVP card, attachment download button) silently
        // breaks for the affected messages.
        let part = parsed
            .attachment(part_id)
            .or_else(|| parsed.parts.get(part_id as usize))
            .ok_or_else(|| {
                UnkaiError::Protocol(format!("Message UID {uid} has no part #{part_id}"))
            })?;

        let filename = decode_attachment_filename(&parsed, part);
        let content_type = part
            .content_type()
            .map(|ct| {
                let ctype = ct.ctype();
                match ct.subtype() {
                    Some(sub) => format!("{ctype}/{sub}"),
                    None => ctype.to_string(),
                }
            })
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let data = part.contents().to_vec();
        let size = Some(data.len() as u64);
        let content_id = part.content_id().map(|s| s.to_string());

        Ok((
            EmailAttachment {
                filename,
                content_type,
                size,
                part_id,
                content_id,
            },
            data,
        ))
    }

    /// Clear the `\Seen` flag on a message — i.e. mark it unread.
    /// Mirror of `mark_as_read`; uses `UID STORE -FLAGS (\Seen)`.
    pub async fn mark_as_unread(&mut self, folder: &str, uid: u32) -> Result<(), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        session.select(to_wire(folder)).await.map_err(|e| {
            UnkaiError::Protocol(format!("Failed to select folder '{folder}': {e}"))
        })?;

        let _updates: Vec<_> = session
            .uid_store(uid.to_string(), "-FLAGS (\\Seen)")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID STORE failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID STORE: {e}")))?;

        info!("Cleared \\Seen on UID {uid} in '{folder}'");
        Ok(())
    }

    /// Mark a message as read by setting the `\Seen` flag on the server.
    ///
    /// Uses `UID STORE <uid> +FLAGS (\Seen)` — idempotent, so calling it on
    /// an already-read message is a no-op. We SELECT (not EXAMINE) here
    /// because EXAMINE opens the folder read-only and rejects STORE.
    pub async fn mark_as_read(&mut self, folder: &str, uid: u32) -> Result<(), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        // Read-write SELECT so the server accepts the STORE.
        session.select(to_wire(folder)).await.map_err(|e| {
            UnkaiError::Protocol(format!("Failed to select folder '{folder}': {e}"))
        })?;

        // uid_store returns a stream of updated flag sets — we don't need them,
        // just drain so the command completes.
        let _updates: Vec<_> = session
            .uid_store(uid.to_string(), "+FLAGS (\\Seen)")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID STORE failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID STORE: {e}")))?;

        info!("Marked UID {uid} as \\Seen in '{folder}'");
        Ok(())
    }

    /// Set the IMAP `\Answered` system flag on a message (#255).
    ///
    /// Called after Compose's send path delivers a successful reply
    /// (or reply-all, or "respond with meeting") so the original
    /// message is marked answered on the server — round-trips to
    /// other mail clients the user might have open, and gives
    /// Unkai's mail-list a stable signal across cache rebuilds.
    /// Uses `UID STORE <uid> +FLAGS (\Answered)`; idempotent.
    pub async fn mark_as_answered(&mut self, folder: &str, uid: u32) -> Result<(), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        // Read-write SELECT so the server accepts the STORE.
        session.select(to_wire(folder)).await.map_err(|e| {
            UnkaiError::Protocol(format!("Failed to select folder '{folder}': {e}"))
        })?;

        let _updates: Vec<_> = session
            .uid_store(uid.to_string(), "+FLAGS (\\Answered)")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID STORE failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID STORE: {e}")))?;

        info!("Marked UID {uid} as \\Answered in '{folder}'");
        Ok(())
    }

    /// Re-read the IMAP system flags on a set of UIDs, returning a
    /// snapshot of `\Seen` / `\Flagged` / `\Answered` per UID.
    ///
    /// The standard envelope-fetch path (`fetch_envelopes`) only
    /// pulls UIDs strictly newer than the cache bookmark — so flag
    /// changes another client makes (marked-read on a phone,
    /// answered from webmail) never round-trip to Unkai on their
    /// own.  This is the catch-up: cheap (`UID FETCH x,y,z (UID
    /// FLAGS)`), no envelope payload, just the flag bits.
    ///
    /// Returns an entry per UID the server actually reports back
    /// (a UID that was expunged between calls just drops out).
    pub async fn fetch_flags(
        &mut self,
        folder: &str,
        uids: &[u32],
    ) -> Result<Vec<FlagSnapshot>, UnkaiError> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        // EXAMINE (read-only) is enough — we're not flipping flags
        // here, just observing them, and read-only avoids
        // accidentally clearing the server's `\Recent` flag
        // bookkeeping for the folder.
        session.examine(to_wire(folder)).await.map_err(|e| {
            UnkaiError::Protocol(format!("Failed to examine folder '{folder}': {e}"))
        })?;

        // Comma-separated UID set is the canonical IMAP way to
        // address a discrete list — no "1:50" range wastefulness if
        // the cached UIDs aren't contiguous, and the server folds
        // adjacent ones into ranges in its parser anyway.
        let uid_set = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetches: Vec<_> = session
            .uid_fetch(&uid_set, "(UID FLAGS)")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID FETCH (FLAGS) failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID FETCH (FLAGS): {e}")))?;

        let snapshots: Vec<FlagSnapshot> = fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?;
                let mut is_read = false;
                let mut is_starred = false;
                let mut is_answered = false;
                for flag in fetch.flags() {
                    match flag {
                        async_imap::types::Flag::Seen => is_read = true,
                        async_imap::types::Flag::Flagged => is_starred = true,
                        async_imap::types::Flag::Answered => is_answered = true,
                        _ => {}
                    }
                }
                Some(FlagSnapshot {
                    uid,
                    is_read,
                    is_starred,
                    is_answered,
                })
            })
            .collect();

        debug!(
            "Refreshed flags for {} UID(s) in '{folder}' (asked: {}, got: {})",
            snapshots.len(),
            uids.len(),
            snapshots.len()
        );
        Ok(snapshots)
    }

    /// Append a raw RFC 822 message to a folder via IMAP `APPEND`.
    ///
    /// Used by the "save sent mail to Sent folder" path: SMTP delivers
    /// the message to recipients, then we APPEND a copy here so the
    /// user can see what they sent. `flags` is the literal IMAP flag
    /// list (e.g. `&["\\Seen"]` — pre-marked read because the user
    /// just wrote it themselves).
    ///
    /// `raw` must already be properly CRLF-terminated RFC 822 bytes —
    /// `lettre::Message::formatted()` produces exactly that.
    pub async fn append_message(
        &mut self,
        folder: &str,
        raw: &[u8],
        flags: &[&str],
    ) -> Result<(), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        // async-imap 0.10's `append` takes the flag list as a single
        // pre-formatted parenthesised IMAP atom. We pass `\Seen` so
        // the appended copy doesn't add to the unread badge — the
        // user wrote it themselves and has already "read" it.
        let flag_atom = if flags.is_empty() {
            None
        } else {
            Some(format!("({})", flags.join(" ")))
        };
        debug!(
            "APPEND {} bytes to '{folder}' (flags: {})",
            raw.len(),
            flag_atom.as_deref().unwrap_or("(none)"),
        );

        session
            .append(to_wire(folder), flag_atom.as_deref(), None, raw)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("APPEND to '{folder}' failed: {e}")))?;

        info!("Appended {} bytes to '{folder}'", raw.len());
        Ok(())
    }

    /// Look up the UID of a just-APPENDed message by its `Message-ID`
    /// header.
    ///
    /// The `APPEND` command in `async-imap` 0.10 doesn't surface the
    /// server's `APPENDUID` response, so we can't learn the assigned
    /// UID directly from the APPEND result. For the minimize-as-draft
    /// flow (#292) we need that UID so a subsequent save can use
    /// `replaceSource` to atomically supersede the previous copy in
    /// place of leaving a trail of duplicate drafts behind.
    ///
    /// `Message-ID` works as a stable handle because lettre stamps
    /// every outgoing message with a UUID-anchored `<uuid@host>` value
    /// (see `build_outgoing_message`), so the search criterion is
    /// effectively unique. Returns the highest matching UID — on the
    /// unlikely event of a collision the most recent server-assigned
    /// UID is the one we just wrote.
    ///
    /// `message_id` is the bare header value including the angle
    /// brackets (e.g. `<abc@host>`), matching what the IMAP server
    /// stored.
    pub async fn find_uid_by_message_id(
        &mut self,
        folder: &str,
        message_id: &str,
    ) -> Result<Option<u32>, UnkaiError> {
        let _ = self.select_folder(folder).await?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        // IMAP quoted strings only need to escape `\` and `"` — the
        // Message-ID's `<uuid@host>` shape contains neither, but
        // escape defensively anyway in case a future caller passes
        // a less well-behaved value through.
        let escaped = message_id.replace('\\', "\\\\").replace('"', "\\\"");
        let criterion = format!("HEADER \"Message-ID\" \"{escaped}\"");
        let uids: Vec<u32> = session
            .uid_search(&criterion)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID SEARCH HEADER Message-ID failed: {e}")))?
            .into_iter()
            .collect();

        Ok(uids.into_iter().max())
    }

    /// Move a message between folders via `UID COPY` + delete.
    ///
    /// Why not `UID MOVE` (RFC 6851)? MOVE is cleaner but requires the
    /// server to advertise the `MOVE` capability, which still isn't
    /// universal in 2026 — the COPY+EXPUNGE fallback works on every
    /// IMAP4rev1 server. We pay for one extra round-trip vs MOVE but
    /// never surprise the user with a "your server doesn't support
    /// that" error on an Archive/Delete button press.
    ///
    /// Used by the Archive and (future) Trash flows in MailView. The
    /// destination folder must already exist — callers locate it via
    /// `pick_archive_folder` / `pick_trash_folder` before calling.
    pub async fn move_message(
        &mut self,
        from_folder: &str,
        uid: u32,
        to_folder: &str,
    ) -> Result<(), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        session.select(to_wire(from_folder)).await.map_err(|e| {
            UnkaiError::Protocol(format!("Failed to select folder '{from_folder}': {e}"))
        })?;

        // UID COPY leaves the source copy in place with its flags
        // intact; the destination gets a server-assigned UID that
        // we don't need to track here.
        session
            .uid_copy(uid.to_string(), quoted_mailbox_arg(to_folder))
            .await
            .map_err(|e| {
                UnkaiError::Protocol(format!(
                    "UID COPY {uid} from '{from_folder}' to '{to_folder}' failed: {e}"
                ))
            })?;

        // Now remove the source: mark + expunge, same dance as
        // `delete_message` below.
        let _updates: Vec<_> = session
            .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID STORE (\\Deleted) failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID STORE: {e}")))?;

        let _expunged: Vec<_> = session
            .expunge()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("EXPUNGE failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read EXPUNGE: {e}")))?;

        info!("Moved UID {uid} from '{from_folder}' to '{to_folder}'");
        Ok(())
    }

    /// Move a *batch* of messages between folders on the current
    /// session.  Same COPY+STORE+EXPUNGE shape as `move_message`,
    /// but does the UID COPY and UID STORE with a comma-joined UID
    /// set so the server processes the lot in one round-trip, and
    /// EXPUNGEs once at the end.  Single SELECT, single COPY,
    /// single STORE, single EXPUNGE — N×3 round-trips collapse to
    /// 4, and there's no chance of racing per-message connection
    /// state across rapid sequential calls.
    ///
    /// Used by the multi-select drag-and-drop and right-click move
    /// flows in MailList where N can easily be 5–50 messages and a
    /// per-message connect/login/logout dance was both slow and,
    /// on some servers, dropping the last move outright due to
    /// rate-limiting / connection-recycling weirdness.
    pub async fn move_messages_batch(
        &mut self,
        from_folder: &str,
        uids: &[u32],
        to_folder: &str,
    ) -> Result<(), UnkaiError> {
        if uids.is_empty() {
            return Ok(());
        }
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        session.select(to_wire(from_folder)).await.map_err(|e| {
            UnkaiError::Protocol(format!("Failed to select folder '{from_folder}': {e}"))
        })?;

        // IMAP allows comma-separated UID sets in UID COPY / UID
        // STORE — one round-trip moves the whole batch.
        let uid_set: String = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");

        session
            .uid_copy(&uid_set, quoted_mailbox_arg(to_folder))
            .await
            .map_err(|e| {
                UnkaiError::Protocol(format!(
                    "UID COPY {uid_set} from '{from_folder}' to '{to_folder}' failed: {e}"
                ))
            })?;

        let _updates: Vec<_> = session
            .uid_store(&uid_set, "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID STORE (\\Deleted) failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID STORE: {e}")))?;

        let _expunged: Vec<_> = session
            .expunge()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("EXPUNGE failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read EXPUNGE: {e}")))?;

        info!(
            "Moved {} UIDs from '{from_folder}' to '{to_folder}'",
            uids.len()
        );
        Ok(())
    }

    /// Permanently remove a message from a folder via the two-step IMAP
    /// dance: `UID STORE +FLAGS (\Deleted)` to mark it, then `EXPUNGE`
    /// to actually reclaim it from the mailbox. Without the EXPUNGE the
    /// message would stay visible in every other client until the next
    /// sync.
    ///
    /// Used by the "replace a draft" flow: after appending the edited
    /// copy to the Drafts folder, we delete the source UID the user
    /// started editing from so there's exactly one draft on the server
    /// per mail the user is composing.
    pub async fn delete_message(&mut self, folder: &str, uid: u32) -> Result<(), UnkaiError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        info!("delete_message: SELECT '{folder}' for UID {uid}");
        let mailbox = session.select(to_wire(folder)).await.map_err(|e| {
            UnkaiError::Protocol(format!("Failed to select folder '{folder}': {e}"))
        })?;
        info!(
            "delete_message: selected '{folder}' (exists={}, uidvalidity={:?}, uidnext={:?})",
            mailbox.exists, mailbox.uid_validity, mailbox.uid_next
        );

        // Probe the UID first. If this comes back empty, the UID we
        // were handed isn't in this folder at all — the envelope
        // cache is out of sync with the server, or (far more likely
        // in practice) the backend is driving the wrong folder for
        // the message the user is looking at. Surfacing *which* of
        // those it is saves a guessing game next time this fails.
        let probe: Vec<_> = session
            .uid_fetch(uid.to_string(), "UID")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID FETCH probe failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID FETCH probe: {e}")))?;
        info!(
            "delete_message: UID FETCH {uid} in '{folder}' -> {} hit(s)",
            probe.len()
        );
        if probe.is_empty() {
            return Err(UnkaiError::Protocol(format!(
                "UID {uid} isn't in folder '{folder}' (exists={}, uidvalidity={:?}). \
                 The envelope cache is out of sync with the server, or the delete is \
                 being driven against the wrong folder.",
                mailbox.exists, mailbox.uid_validity
            )));
        }

        // STORE the `\Deleted` flag and keep the returned FETCH
        // responses — if the set is empty the server accepted the
        // STORE but didn't actually modify anything, which almost
        // always means the SELECT landed on a read-only view or the
        // server suppresses the FETCH echo for \Deleted (rare). We
        // press on to EXPUNGE anyway, but log loudly so it shows up
        // in traces.
        let updates: Vec<_> = session
            .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID STORE (\\Deleted) failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read UID STORE: {e}")))?;

        if updates.is_empty() {
            tracing::warn!(
                "delete_message: UID STORE (\\Deleted) on UID {uid} in '{folder}' \
                 returned no FETCH updates even though the UID probe found the message — \
                 proceeding to EXPUNGE anyway, the flag may have been set silently"
            );
        } else {
            info!(
                "delete_message: UID STORE flagged {uid} as \\Deleted ({} response(s))",
                updates.len()
            );
        }

        // Prefer `UID EXPUNGE` (RFC 4315 / UIDPLUS) — it only expunges
        // the specific UID we just marked, leaving any other
        // `\Deleted`-flagged messages other clients might be juggling
        // in the same mailbox untouched. Most servers advertise
        // UIDPLUS; on the ones that don't we fall back to the
        // broader plain EXPUNGE below, which is still correct for
        // our use (we only flagged one UID in this session).
        //
        // The inner helper consumes the returned stream fully before
        // returning, which is what lets us fall back to a second
        // mutable borrow of `session` on the outer error branch
        // without tripping the borrow checker — if we kept the
        // Stream around we'd be holding a mutable borrow into the
        // Err arm.
        let uid_set = uid.to_string();
        let try_uid_expunge = async {
            let stream = session
                .uid_expunge(&uid_set)
                .await
                .map_err(|e| format!("UID EXPUNGE failed: {e}"))?;
            stream
                .try_collect::<Vec<_>>()
                .await
                .map_err(|e| format!("Failed to read UID EXPUNGE: {e}"))
        };

        let expunged_count = match try_uid_expunge.await {
            Ok(expunged) => {
                info!(
                    "delete_message: UID EXPUNGE {uid} removed {} message(s)",
                    expunged.len()
                );
                expunged.len()
            }
            Err(e) => {
                // UIDPLUS not supported (or the server rejected the
                // command for another reason). Fall back to plain
                // EXPUNGE — we only flagged one UID in this session
                // so the broader command is still targeted enough.
                tracing::warn!("delete_message: UID EXPUNGE failed ({e}), falling back to EXPUNGE");
                let expunged: Vec<_> = session
                    .expunge()
                    .await
                    .map_err(|e| UnkaiError::Protocol(format!("EXPUNGE failed: {e}")))?
                    .try_collect()
                    .await
                    .map_err(|e| UnkaiError::Protocol(format!("Failed to read EXPUNGE: {e}")))?;
                info!(
                    "delete_message: EXPUNGE removed {} message(s) (fallback)",
                    expunged.len()
                );
                expunged.len()
            }
        };

        if expunged_count == 0 {
            return Err(UnkaiError::Protocol(format!(
                "EXPUNGE in '{folder}' removed 0 messages after flagging UID {uid} — \
                 the \\Deleted flag didn't stick on this server"
            )));
        }

        info!("Deleted UID {uid} from '{folder}'");
        Ok(())
    }

    /// Return every UID currently in the folder (via `UID SEARCH ALL`).
    ///
    /// Used by the envelope-cache reconciler to spot ghost rows: any
    /// UID in our local cache that isn't in this set has been expunged
    /// on the server and should be dropped. Cheap on small folders
    /// (Drafts, Trash, Archive); on large inboxes it's a few KB of
    /// wire traffic but still a single command, much cheaper than
    /// re-fetching bodies.
    pub async fn list_all_uids(&mut self, folder: &str) -> Result<Vec<u32>, UnkaiError> {
        let _ = self.select_folder(folder).await?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        let uids: Vec<u32> = session
            .uid_search("ALL")
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID SEARCH ALL failed: {e}")))?
            .into_iter()
            .collect();
        Ok(uids)
    }

    /// Server-side search fallback for messages that aren't cached locally.
    ///
    /// Runs `UID SEARCH` on the given folder with a criterion built from
    /// the user's query, then fetches envelopes for up to `limit` hits.
    /// Used when the FTS5 cache misses (e.g. the user is looking for an
    /// old message that was never opened on this machine).
    ///
    /// IMAP SEARCH is server-implementation-dependent and can be slow —
    /// this is the "last resort" path. The frontend calls it only after
    /// the local cache search returns fewer results than expected, or on
    /// explicit user action ("search server too").
    pub async fn search_envelopes(
        &mut self,
        folder: &str,
        criterion: &str,
        limit: u32,
    ) -> Result<Vec<EmailEnvelope>, UnkaiError> {
        let (_total, _uidvalidity) = self.select_folder(folder).await?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        debug!("UID SEARCH in '{folder}' with criterion: {criterion}");
        let uids: Vec<u32> = session
            .uid_search(criterion)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID SEARCH failed: {e}")))?
            .into_iter()
            .collect();

        if uids.is_empty() {
            return Ok(Vec::new());
        }

        // Newest-first: SEARCH returns in UID ascending order, but the
        // mail list shows newest first. Sort desc, then cap to limit.
        let mut uids = uids;
        uids.sort_unstable_by(|a, b| b.cmp(a));
        uids.truncate(limit as usize);

        // Build a UID set like `42,17,9` — async-imap accepts this form.
        let set = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetches: Vec<_> = session
            .uid_fetch(
                set,
                "(UID FLAGS INTERNALDATE ENVELOPE \
                 BODY.PEEK[HEADER.FIELDS (REFERENCES CONTENT-TYPE)])",
            )
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID FETCH after SEARCH failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read SEARCH FETCH: {e}")))?;

        let mut envelopes: Vec<EmailEnvelope> = fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?;
                let envelope = fetch.envelope()?;

                let subject = envelope
                    .subject
                    .as_ref()
                    .map(|s| decode_header(s))
                    .unwrap_or_default();
                let from = envelope
                    .from
                    .as_ref()
                    .and_then(|addrs| addrs.first())
                    .map(format_address)
                    .unwrap_or_default();
                let date = envelope
                    .date
                    .as_ref()
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .and_then(parse_rfc2822)
                    .or_else(|| fetch.internal_date().map(|dt| dt.with_timezone(&Utc)))
                    .unwrap_or_else(Utc::now);

                let mut is_read = false;
                let mut is_starred = false;
                let mut is_answered = false;
                for flag in fetch.flags() {
                    match flag {
                        async_imap::types::Flag::Seen => is_read = true,
                        async_imap::types::Flag::Flagged => is_starred = true,
                        async_imap::types::Flag::Answered => is_answered = true,
                        _ => {}
                    }
                }

                let (thread_msg_id, thread_in_reply_to, thread_refs) =
                    extract_threading_headers(fetch);
                let protection = extract_envelope_protection(fetch);

                Some(EmailEnvelope {
                    uid,
                    folder: folder.to_string(),
                    from,
                    subject,
                    date,
                    is_read,
                    is_starred,
                    is_answered,
                    replied_kind: None,
                    account_id: String::new(),
                    message_id: thread_msg_id,
                    in_reply_to: thread_in_reply_to,
                    references_ids: thread_refs,
                    // #334: cache populates these on upsert; off-the-wire
                    // envelopes don't know their thread identity yet.
                    thread_id: None,
                    thread_total_count: None,
                    protection,
                })
            })
            .collect();
        envelopes.sort_unstable_by_key(|e| std::cmp::Reverse(e.date));

        info!("SEARCH '{folder}' '{criterion}' → {} hits", envelopes.len());
        Ok(envelopes)
    }

    /// Server-side search variant of `search_envelopes` that returns
    /// only matches with UIDs strictly less than `before_uid`. Used
    /// by SearchResults' infinite-scroll path (#194 follow-up): when
    /// the user has clicked "Search server too" and wants to keep
    /// loading deeper into the server's results.
    ///
    /// Same SEARCH-then-FETCH shape as `search_envelopes`: the UID
    /// criterion is AND'd into the query so the server-side filter
    /// runs as part of one SEARCH instead of two; we then sort the
    /// returned UIDs descending and FETCH just the top `limit`.
    pub async fn search_envelopes_older(
        &mut self,
        folder: &str,
        criterion: &str,
        before_uid: u32,
        limit: u32,
    ) -> Result<Vec<EmailEnvelope>, UnkaiError> {
        if before_uid <= 1 {
            return Ok(Vec::new());
        }
        let _ = self.select_folder(folder).await?;
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        // AND the user's query with `UID 1:<before_uid-1>` so the
        // server returns only matches strictly older than the
        // anchor — saves us a client-side filter pass.
        let combined = format!("UID 1:{} {}", before_uid - 1, criterion);
        debug!("UID SEARCH (older) in '{folder}': {combined}");
        let mut uids: Vec<u32> = session
            .uid_search(&combined)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID SEARCH (older) failed: {e}")))?
            .into_iter()
            .collect();

        if uids.is_empty() {
            return Ok(Vec::new());
        }

        uids.sort_unstable_by(|a, b| b.cmp(a));
        uids.truncate(limit as usize);

        let set = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let fetches: Vec<_> = session
            .uid_fetch(
                set,
                "(UID FLAGS INTERNALDATE ENVELOPE \
                 BODY.PEEK[HEADER.FIELDS (REFERENCES CONTENT-TYPE)])",
            )
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID FETCH (older search) failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read older SEARCH FETCH: {e}")))?;

        let mut envelopes: Vec<EmailEnvelope> = fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?;
                let envelope = fetch.envelope()?;
                let subject = envelope
                    .subject
                    .as_ref()
                    .map(|s| decode_header(s))
                    .unwrap_or_default();
                let from = envelope
                    .from
                    .as_ref()
                    .and_then(|addrs| addrs.first())
                    .map(format_address)
                    .unwrap_or_default();
                let date = envelope
                    .date
                    .as_ref()
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .and_then(parse_rfc2822)
                    .or_else(|| fetch.internal_date().map(|dt| dt.with_timezone(&Utc)))
                    .unwrap_or_else(Utc::now);
                let mut is_read = false;
                let mut is_starred = false;
                let mut is_answered = false;
                for flag in fetch.flags() {
                    match flag {
                        async_imap::types::Flag::Seen => is_read = true,
                        async_imap::types::Flag::Flagged => is_starred = true,
                        async_imap::types::Flag::Answered => is_answered = true,
                        _ => {}
                    }
                }
                let (thread_msg_id, thread_in_reply_to, thread_refs) =
                    extract_threading_headers(fetch);
                let protection = extract_envelope_protection(fetch);
                Some(EmailEnvelope {
                    uid,
                    folder: folder.to_string(),
                    from,
                    subject,
                    date,
                    is_read,
                    is_starred,
                    is_answered,
                    replied_kind: None,
                    account_id: String::new(),
                    message_id: thread_msg_id,
                    in_reply_to: thread_in_reply_to,
                    references_ids: thread_refs,
                    // #334: cache populates these on upsert; off-the-wire
                    // envelopes don't know their thread identity yet.
                    thread_id: None,
                    thread_total_count: None,
                    protection,
                })
            })
            .collect();
        envelopes.sort_unstable_by_key(|e| std::cmp::Reverse(e.date));
        Ok(envelopes)
    }

    /// Fetch up to `limit` envelopes whose UIDs are strictly less than
    /// `before_uid`, sorted newest-first.  Used by MailList's
    /// infinite-scroll "load older" path (#194): the cold-cache
    /// `fetch_envelopes("newest N")` only walks the tail of the
    /// folder, so anything older than the Nth-newest message never
    /// reaches the local cache. This method runs `UID SEARCH UID
    /// 1:<before_uid-1>` to get every older UID, sorts descending,
    /// truncates to `limit`, and fetches just those envelopes.
    ///
    /// Returns the freshly-fetched envelopes; the caller is
    /// responsible for writing them through to the cache. An empty
    /// return means there's nothing older — frontend can stop
    /// asking.
    ///
    /// Two round trips (SEARCH then FETCH) on purpose: a single
    /// `UID FETCH 1:<before_uid-1>` would parse envelope metadata
    /// for every older message in the folder, even though we only
    /// want the newest `limit` of them. SEARCH returns just UIDs
    /// (small payload), FETCH then asks for the slice we keep.
    pub async fn fetch_older_envelopes(
        &mut self,
        folder: &str,
        before_uid: u32,
        limit: u32,
    ) -> Result<EnvelopeBatch, UnkaiError> {
        let (_total, uidvalidity) = self.select_folder(folder).await?;
        if before_uid == 0 || before_uid == 1 {
            // Nothing can be older than UID 1.  (Some servers don't
            // assign UID 0 at all, others reserve it; either way an
            // empty response here is correct.)
            return Ok(EnvelopeBatch {
                uidvalidity,
                envelopes: Vec::new(),
            });
        }

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| UnkaiError::Protocol("Session is closed".into()))?;

        let criterion = format!("UID 1:{}", before_uid - 1);
        debug!("UID SEARCH for older in '{folder}': {criterion}");
        let mut uids: Vec<u32> = session
            .uid_search(&criterion)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID SEARCH (older) failed: {e}")))?
            .into_iter()
            .collect();

        if uids.is_empty() {
            return Ok(EnvelopeBatch {
                uidvalidity,
                envelopes: Vec::new(),
            });
        }

        // Top `limit` by descending UID — those are the newest
        // among "older than before_uid".
        uids.sort_unstable_by(|a, b| b.cmp(a));
        uids.truncate(limit as usize);

        let set = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let fetches: Vec<_> = session
            .uid_fetch(
                set,
                "(UID FLAGS INTERNALDATE ENVELOPE \
                 BODY.PEEK[HEADER.FIELDS (REFERENCES CONTENT-TYPE)])",
            )
            .await
            .map_err(|e| UnkaiError::Protocol(format!("UID FETCH (older) failed: {e}")))?
            .try_collect()
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to read older FETCH: {e}")))?;

        let mut envelopes: Vec<EmailEnvelope> = fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid?;
                let envelope = fetch.envelope()?;

                let subject = envelope
                    .subject
                    .as_ref()
                    .map(|s| decode_header(s))
                    .unwrap_or_default();
                let from = envelope
                    .from
                    .as_ref()
                    .and_then(|addrs| addrs.first())
                    .map(format_address)
                    .unwrap_or_default();
                let date = envelope
                    .date
                    .as_ref()
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .and_then(parse_rfc2822)
                    .or_else(|| fetch.internal_date().map(|dt| dt.with_timezone(&Utc)))
                    .unwrap_or_else(Utc::now);

                let mut is_read = false;
                let mut is_starred = false;
                let mut is_answered = false;
                for flag in fetch.flags() {
                    match flag {
                        async_imap::types::Flag::Seen => is_read = true,
                        async_imap::types::Flag::Flagged => is_starred = true,
                        async_imap::types::Flag::Answered => is_answered = true,
                        _ => {}
                    }
                }

                let (thread_msg_id, thread_in_reply_to, thread_refs) =
                    extract_threading_headers(fetch);
                let protection = extract_envelope_protection(fetch);

                Some(EmailEnvelope {
                    uid,
                    folder: folder.to_string(),
                    from,
                    subject,
                    date,
                    is_read,
                    is_starred,
                    is_answered,
                    replied_kind: None,
                    account_id: String::new(),
                    message_id: thread_msg_id,
                    in_reply_to: thread_in_reply_to,
                    references_ids: thread_refs,
                    // #334: cache populates these on upsert; off-the-wire
                    // envelopes don't know their thread identity yet.
                    thread_id: None,
                    thread_total_count: None,
                    protection,
                })
            })
            .collect();
        envelopes.sort_unstable_by_key(|e| std::cmp::Reverse(e.date));

        info!(
            "Fetched {} older envelopes in '{folder}' before UID {before_uid}",
            envelopes.len()
        );
        Ok(EnvelopeBatch {
            uidvalidity,
            envelopes,
        })
    }

    /// Log out from the IMAP server and close the connection cleanly.
    ///
    /// Always call this when you're done — it sends the LOGOUT command
    /// so the server knows we're leaving properly.
    pub async fn logout(mut self) -> Result<(), UnkaiError> {
        if let Some(mut session) = self.session.take() {
            session
                .logout()
                .await
                .map_err(|e| UnkaiError::Protocol(format!("IMAP logout failed: {e}")))?;
            info!("Logged out from IMAP server");
        }
        Ok(())
    }
}

// ── Helpers ────────────────────────────────────────────────────

/// Decode a possibly RFC 2047-encoded header value (e.g. `=?UTF-8?B?...?=`).
fn decode_header(bytes: &[u8]) -> String {
    // We reuse mail-parser's header decoding by wrapping the value in a
    // fake "Subject:" header and parsing. This handles encoded-word decoding
    // for us. If parsing fails, fall back to lossy UTF-8.
    let raw = format!("Subject: {}\r\n\r\n", String::from_utf8_lossy(bytes));
    MessageParser::default()
        .parse(raw.as_bytes())
        .and_then(|m| m.subject().map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

/// Format an IMAP envelope address as "Name <user@host>" (or just the address).
///
/// When the envelope's display-name slot equals the bare email
/// (case-insensitive), we drop the name and emit just the address.
/// Some senders / mail servers populate the personal-name component
/// with the email itself, which would produce malformed RFC 5322
/// like `alex@example.com <alex@example.com>` — the unquoted `@` in
/// the phrase makes the result unparseable, and a plain reply would
/// fail with "Invalid 'to' address".  Collapsing the redundant name
/// also cleans up the mail-list display.
fn format_address(addr: &async_imap::imap_proto::types::Address<'_>) -> String {
    let name = addr
        .name
        .as_ref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("");
    let mailbox = addr
        .mailbox
        .as_ref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("");
    let host = addr
        .host
        .as_ref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("");

    let email = if mailbox.is_empty() || host.is_empty() {
        String::new()
    } else {
        format!("{mailbox}@{host}")
    };

    let decoded_name = if name.is_empty() {
        String::new()
    } else {
        decode_header(name.as_bytes())
    };

    // Treat a name that's identical to the email as no name at all.
    let name_is_redundant = !email.is_empty() && decoded_name.trim().eq_ignore_ascii_case(&email);

    match (
        decoded_name.is_empty() || name_is_redundant,
        email.is_empty(),
    ) {
        (true, _) => email,
        (false, true) => decoded_name,
        (false, false) => format!("{decoded_name} <{email}>"),
    }
}

/// Pull RFC 5322 threading headers off a `Fetch` response (#277).
///
/// Returns `(message_id, in_reply_to, references)` where each
/// Message-ID has its angle brackets stripped — `<abc@h>` →
/// `abc@h` — so cache lookups don't have to be bracket-aware.
///
/// `Message-ID` and `In-Reply-To` come from the IMAP `ENVELOPE`
/// response (cheap, already in the FETCH).  `References` is *not*
/// part of ENVELOPE and has to be pulled from a
/// `BODY.PEEK[HEADER.FIELDS (REFERENCES)]` section, which we
/// added to the FETCH at the call site.
fn extract_threading_headers(
    fetch: &async_imap::types::Fetch,
) -> (Option<String>, Option<String>, Vec<String>) {
    let envelope = fetch.envelope();
    let message_id = envelope
        .and_then(|e| e.message_id.as_ref())
        .and_then(|raw| std::str::from_utf8(raw).ok())
        .and_then(strip_msgid_brackets);
    let in_reply_to = envelope
        .and_then(|e| e.in_reply_to.as_ref())
        .and_then(|raw| std::str::from_utf8(raw).ok())
        .and_then(strip_msgid_brackets);
    let references = fetch
        .header()
        .and_then(|raw| std::str::from_utf8(raw).ok())
        .map(parse_references_header)
        .unwrap_or_default();
    (message_id, in_reply_to, references)
}

/// #341 background-decrypt: peek at the BODY.PEEK[HEADER.FIELDS ...]
/// payload to classify a message's PGP/MIME envelope (encrypted /
/// signed / plaintext) without having to fetch the body.
///
/// Returns `Some("encrypted")` for `multipart/encrypted; protocol="application/pgp-encrypted"`,
/// `Some("signed")` for `multipart/signed; protocol="application/pgp-signature"`,
/// and `None` for anything else — including plain mail and messages
/// whose top-level Content-Type isn't a PGP/MIME wrapper.
///
/// The mail-list lock chip is keyed off this value via the cache's
/// `messages.protection` column, so the moment a new encrypted
/// message lands the UI can render the chip without first opening
/// the message.  The body-side `message_bodies.protection` stays
/// authoritative once we've decrypted (it can carry the post-decrypt
/// `"signed-and-encrypted"` label this header-only check can't), and
/// envelope reads `COALESCE(b.protection, m.protection)` to prefer
/// it.
///
/// Relies on the FETCH call site requesting `CONTENT-TYPE` in the
/// header-fields list — callers that don't pass it will only see
/// `None` here regardless of the message shape.  The PGP/MIME
/// detection rules mirror [`detect_pgp_mime_envelope`] so a message
/// classified as encrypted at envelope-fetch time stays classified
/// the same way once its full body lands and the body parser runs.
fn extract_envelope_protection(fetch: &async_imap::types::Fetch) -> Option<String> {
    let raw = fetch.header()?;
    // mail-parser handles RFC 5322 continuation lines + RFC 2045
    // Content-Type parameter parsing for us — including quoted /
    // unquoted protocol values, mixed-case header names, and
    // RFC 2231-encoded attributes — without re-implementing the
    // unfolding rules here.
    let parsed = MessageParser::default().parse(raw)?;
    let ct = parsed.content_type()?;
    let ctype = ct.ctype();
    let subtype = ct.subtype()?;

    // S/MIME encrypted form is a bare `application/pkcs7-mime` single
    // part (no `multipart` wrapper), so check it before the multipart
    // gate that the PGP shapes sit behind.  `smime-type=enveloped-data`
    // is the encrypted form; the opaque signed form and certs-only are
    // left to the body parser (we only stamp what the chip can mean).
    if is_pkcs7_mime(ctype, subtype) {
        let smime_type = ct.attribute("smime-type").unwrap_or("");
        return if smime_type.eq_ignore_ascii_case("enveloped-data") {
            Some("encrypted".to_string())
        } else {
            None
        };
    }

    if !ctype.eq_ignore_ascii_case("multipart") {
        return None;
    }
    let protocol = ct.attribute("protocol").unwrap_or("");
    if subtype.eq_ignore_ascii_case("encrypted")
        && protocol.eq_ignore_ascii_case("application/pgp-encrypted")
    {
        Some("encrypted".to_string())
    } else if subtype.eq_ignore_ascii_case("signed")
        && protocol.eq_ignore_ascii_case("application/pgp-signature")
    {
        Some("signed".to_string())
    } else if subtype.eq_ignore_ascii_case("signed")
        && (protocol.eq_ignore_ascii_case("application/pkcs7-signature")
            || protocol.eq_ignore_ascii_case("application/x-pkcs7-signature"))
    {
        // S/MIME detached (clear-signed) form — same "signed" chip the
        // PGP `multipart/signed` shape stamps.
        Some("signed".to_string())
    } else {
        None
    }
}

/// `<abc@host>` → `Some("abc@host")`.  Tolerates surrounding
/// whitespace and a value that's already bare (no brackets).
/// Empty / whitespace-only inputs return `None` so the caller can
/// store a clean `NULL` instead of an empty string.
fn strip_msgid_brackets(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = trimmed
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
        .unwrap_or(trimmed);
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Parse the body of a `BODY[HEADER.FIELDS (REFERENCES)]` response
/// into the ordered Message-ID list from the `References:` header.
/// The body is one or more lines starting with `References:`, with
/// continuation lines (RFC 5322 §2.2.3) folded by leading
/// whitespace.  We unfold by joining all non-empty lines that
/// follow `References:` until a blank line.  Each resulting token
/// matching `<...>` becomes one entry, brackets stripped.
fn parse_references_header(raw: &str) -> Vec<String> {
    let mut joined = String::new();
    let mut in_refs = false;
    for line in raw.lines() {
        if let Some(rest) = line
            .strip_prefix("References:")
            .or_else(|| line.strip_prefix("references:"))
            .or_else(|| line.strip_prefix("REFERENCES:"))
        {
            in_refs = true;
            joined.push_str(rest);
        } else if in_refs && (line.starts_with(' ') || line.starts_with('\t')) {
            // Continuation of the folded header.
            joined.push(' ');
            joined.push_str(line);
        } else if in_refs {
            // Reached the next header or a blank line — done.
            break;
        }
    }
    let mut out = Vec::new();
    let bytes = joined.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(end) = bytes[i + 1..].iter().position(|&b| b == b'>') {
                if let Ok(id) = std::str::from_utf8(&bytes[i + 1..i + 1 + end]) {
                    let trimmed = id.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_string());
                    }
                }
                i += 1 + end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Parse an RFC 2822 date string (as found in Date: headers) to chrono UTC.
fn parse_rfc2822(s: &str) -> Option<DateTime<Utc>> {
    match DateTime::parse_from_rfc2822(s) {
        Ok(dt) => Some(dt.with_timezone(&Utc)),
        Err(e) => {
            warn!("Failed to parse date '{s}': {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{quoted_mailbox_arg, to_wire};

    #[test]
    fn to_wire_is_bare_mutf7_no_quoting() {
        // `to_wire` must NOT add IMAP quoting — async-imap's
        // `validate_str` quotes for SELECT / EXAMINE / CREATE
        // / etc. internally, so quoting here would result in
        // double-quoting on the wire.  Pure-ASCII names pass
        // through verbatim; non-ASCII names go through mUTF-7
        // and stay ASCII without atom-specials.
        assert_eq!(to_wire("INBOX"), "INBOX");
        assert_eq!(to_wire("Sent"), "Sent");
        assert_eq!(to_wire("Audi TT"), "Audi TT"); // no quoting at this layer
        assert_eq!(to_wire("INBOX/Projects"), "INBOX/Projects");
        let unicode = to_wire("Bücher");
        assert!(unicode.is_ascii());
    }

    #[test]
    fn quoted_mailbox_arg_passes_simple_atoms_unquoted() {
        // The uid_copy helper only quotes when the name contains
        // atom-specials.  Bare ASCII names without specials stay
        // unquoted so they round-trip identically to the
        // pre-quoting behaviour for the common case.
        assert_eq!(quoted_mailbox_arg("INBOX"), "INBOX");
        assert_eq!(quoted_mailbox_arg("Sent"), "Sent");
        assert_eq!(quoted_mailbox_arg("INBOX/Projects"), "INBOX/Projects");
        assert_eq!(quoted_mailbox_arg("Foo.Bar"), "Foo.Bar");
    }

    #[test]
    fn quoted_mailbox_arg_quotes_names_with_spaces() {
        // Regression: names like "Audi TT" used to be sent as a
        // bare atom for UID COPY, and the IMAP server
        // interpreted everything before the first space as the
        // mailbox.  Only `uid_copy` / `uid_move` need this
        // helper; everywhere else async-imap's own quoter runs.
        assert_eq!(quoted_mailbox_arg("Audi TT"), "\"Audi TT\"");
        assert_eq!(quoted_mailbox_arg("Sent Items"), "\"Sent Items\"");
    }

    #[test]
    fn quoted_mailbox_arg_quotes_other_atom_specials() {
        assert_eq!(quoted_mailbox_arg("(work)"), "\"(work)\"");
        assert_eq!(quoted_mailbox_arg("a*b"), "\"a*b\"");
        assert_eq!(quoted_mailbox_arg("100%"), "\"100%\"");
    }

    #[test]
    fn quoted_mailbox_arg_escapes_quote_and_backslash() {
        assert_eq!(
            quoted_mailbox_arg("She said \"hi\""),
            "\"She said \\\"hi\\\"\""
        );
        assert_eq!(quoted_mailbox_arg("path\\to"), "\"path\\\\to\"");
    }

    // ── PGP/MIME receive interceptor (#57) ─────────────────────

    use super::{parse_eml_bytes, parse_eml_bytes_with_crypto};
    use unkai_core::UnkaiError;
    use unkai_core::crypto::{CryptoBridge, DecryptedPayload, EncryptedOutput, VerifyOutcome};

    /// A test-only bridge that hands back a pre-baked plaintext when
    /// asked to decrypt, and records the ciphertext it saw.  Used by
    /// the encryption-detection tests below to confirm the receive
    /// path extracted the right bytes from the PGP/MIME envelope
    /// without spinning up a real `rpgp` key.
    struct StubBridge {
        plaintext: Vec<u8>,
        signature_status: Option<String>,
        signer_fingerprint: Option<String>,
    }

    impl CryptoBridge for StubBridge {
        fn decrypt(&self, _ciphertext_armor: &[u8]) -> Result<DecryptedPayload, UnkaiError> {
            Ok(DecryptedPayload {
                plaintext: self.plaintext.clone(),
                signature_status: self.signature_status.clone(),
                signer_fingerprint: self.signer_fingerprint.clone(),
            })
        }
        fn decrypt_smime(&self, _cms_der: &[u8]) -> Result<DecryptedPayload, UnkaiError> {
            // Same pre-baked plaintext as the PGP path — the receive-path
            // tests only care that the right bytes were lifted out of the
            // S/MIME envelope and handed to the bridge, not how the CMS
            // is actually decrypted.
            Ok(DecryptedPayload {
                plaintext: self.plaintext.clone(),
                signature_status: self.signature_status.clone(),
                signer_fingerprint: self.signer_fingerprint.clone(),
            })
        }
        fn verify(
            &self,
            _signed_payload: &[u8],
            _signature_armor: &[u8],
        ) -> Result<VerifyOutcome, UnkaiError> {
            unreachable!("encryption tests never hit the verify path")
        }
        fn encrypt(
            &self,
            _inner_mime: &[u8],
            _recipient_emails: &[String],
            _sign: bool,
        ) -> Result<EncryptedOutput, UnkaiError> {
            unreachable!("receive-path tests never hit the encrypt path")
        }
        fn sign(&self, _signed_payload: &[u8]) -> Result<Vec<u8>, UnkaiError> {
            unreachable!("receive-path tests never hit the sign path")
        }
    }

    /// Build a PGP/MIME `multipart/encrypted` message with a placeholder
    /// ciphertext in the second part.  The bridge stub doesn't actually
    /// decrypt — it just hands back a pre-decided plaintext — so the
    /// ciphertext bytes only need to be parseable, not valid OpenPGP.
    fn pgp_mime_encrypted(inner_plaintext: &str) -> Vec<u8> {
        let body = format!(
            "From: alice@example.com\r\n\
             To: bob@example.com\r\n\
             Subject: secret note\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/encrypted; \
                 protocol=\"application/pgp-encrypted\"; \
                 boundary=\"unkai-test-boundary\"\r\n\
             \r\n\
             --unkai-test-boundary\r\n\
             Content-Type: application/pgp-encrypted\r\n\
             \r\n\
             Version: 1\r\n\
             \r\n\
             --unkai-test-boundary\r\n\
             Content-Type: application/octet-stream; name=\"encrypted.asc\"\r\n\
             \r\n\
             {inner_plaintext}\r\n\
             --unkai-test-boundary--\r\n",
        );
        body.into_bytes()
    }

    /// The plaintext recovered by the bridge stub.  Itself a complete
    /// inner MIME message — RFC 3156 §4 says the decrypted payload is
    /// the original mail body, headers included.
    fn pgp_mime_inner_plaintext() -> Vec<u8> {
        b"From: alice@example.com\r\n\
          To: bob@example.com\r\n\
          Subject: secret note\r\n\
          MIME-Version: 1.0\r\n\
          Content-Type: text/plain; charset=\"utf-8\"\r\n\
          \r\n\
          the eagle has landed\r\n"
            .to_vec()
    }

    #[test]
    fn pgp_mime_encrypted_is_unwrapped_when_bridge_present() {
        let raw = pgp_mime_encrypted("CIPHERTEXT-PLACEHOLDER");
        let bridge = StubBridge {
            plaintext: pgp_mime_inner_plaintext(),
            signature_status: None,
            signer_fingerprint: None,
        };

        let email =
            parse_eml_bytes_with_crypto(&raw, "INBOX:1", "acc", "INBOX", Some(&bridge)).unwrap();

        assert_eq!(email.subject, "secret note");
        assert_eq!(
            email.body_text.as_deref(),
            Some("the eagle has landed\n"),
            "decrypted plaintext body must reach the Email struct"
        );
        assert_eq!(email.protection.as_deref(), Some("encrypted"));
        assert_eq!(email.signature_status, None);
        assert_eq!(email.signer_fingerprint, None);
    }

    #[test]
    fn pgp_mime_signed_inside_encrypted_marks_protection_signed_and_encrypted() {
        let raw = pgp_mime_encrypted("CIPHERTEXT-PLACEHOLDER");
        let bridge = StubBridge {
            plaintext: pgp_mime_inner_plaintext(),
            signature_status: Some("valid".into()),
            signer_fingerprint: Some("ABCD1234".into()),
        };

        let email =
            parse_eml_bytes_with_crypto(&raw, "INBOX:1", "acc", "INBOX", Some(&bridge)).unwrap();

        assert_eq!(email.protection.as_deref(), Some("signed-and-encrypted"));
        assert_eq!(email.signature_status.as_deref(), Some("valid"));
        assert_eq!(email.signer_fingerprint.as_deref(), Some("ABCD1234"));
    }

    #[test]
    fn pgp_mime_encrypted_without_bridge_falls_back_to_plaintext() {
        // No bridge → the message is still parseable as plain MIME,
        // just with empty body and `protection = None`.  This is the
        // "user hasn't imported a PGP key yet" path: we don't break
        // the UI, we just can't show the contents.
        let raw = pgp_mime_encrypted("CIPHERTEXT-PLACEHOLDER");

        let email = parse_eml_bytes_with_crypto(&raw, "INBOX:1", "acc", "INBOX", None).unwrap();

        assert_eq!(email.subject, "secret note");
        assert_eq!(email.protection, None);
        // The `parse_eml_bytes` wrapper must behave identically.
        let plain = parse_eml_bytes(&raw, "INBOX:1", "acc", "INBOX").unwrap();
        assert_eq!(plain.subject, email.subject);
        assert_eq!(plain.protection, email.protection);
    }

    #[test]
    fn plain_mail_passes_through_unchanged_regardless_of_bridge() {
        let raw = b"From: alice@example.com\r\n\
                    To: bob@example.com\r\n\
                    Subject: plain mail\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: text/plain; charset=\"utf-8\"\r\n\
                    \r\n\
                    hello world\r\n";
        let bridge = StubBridge {
            plaintext: b"WOULD-NEVER-BE-CALLED".to_vec(),
            signature_status: None,
            signer_fingerprint: None,
        };

        let with_bridge =
            parse_eml_bytes_with_crypto(raw, "INBOX:1", "acc", "INBOX", Some(&bridge)).unwrap();
        let without_bridge = parse_eml_bytes(raw, "INBOX:1", "acc", "INBOX").unwrap();

        assert_eq!(with_bridge.subject, "plain mail");
        assert_eq!(with_bridge.body_text.as_deref(), Some("hello world\n"));
        assert_eq!(with_bridge.protection, None);
        // Plain mail must round-trip identically in both code paths —
        // the bridge is only consulted when a PGP envelope is detected.
        assert_eq!(with_bridge.subject, without_bridge.subject);
        assert_eq!(with_bridge.body_text, without_bridge.body_text);
        assert_eq!(with_bridge.protection, without_bridge.protection);
    }

    // ── S/MIME receive interceptor (#338) ──────────────────────

    /// Build an S/MIME `application/pkcs7-mime; smime-type=enveloped-data`
    /// message.  As with the PGP fixture the bridge stub doesn't really
    /// decrypt, so the body only needs to be parseable — base64 of an
    /// arbitrary byte string stands in for the CMS `EnvelopedData` DER.
    fn smime_enveloped(cms_body_b64: &str) -> Vec<u8> {
        format!(
            "From: alice@example.com\r\n\
             To: bob@example.com\r\n\
             Subject: secret memo\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: application/pkcs7-mime; smime-type=enveloped-data; \
                 name=\"smime.p7m\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             Content-Disposition: attachment; filename=\"smime.p7m\"\r\n\
             \r\n\
             {cms_body_b64}\r\n"
        )
        .into_bytes()
    }

    /// The plaintext the bridge stub "recovers" — a full inner MIME
    /// message, headers included, exactly like the OpenPGP fixture.
    fn smime_inner_plaintext() -> Vec<u8> {
        b"From: alice@example.com\r\n\
          To: bob@example.com\r\n\
          Subject: secret memo\r\n\
          MIME-Version: 1.0\r\n\
          Content-Type: text/plain; charset=\"utf-8\"\r\n\
          \r\n\
          the package is in transit\r\n"
            .to_vec()
    }

    #[test]
    fn smime_enveloped_is_unwrapped_when_bridge_present() {
        // "SGVsbG8=" is just valid base64 ("Hello") — the stub bridge
        // ignores the bytes and returns the pre-baked plaintext.
        let raw = smime_enveloped("SGVsbG8gU01JTUUgY2lwaGVydGV4dA==");
        let bridge = StubBridge {
            plaintext: smime_inner_plaintext(),
            signature_status: None,
            signer_fingerprint: None,
        };

        let email =
            parse_eml_bytes_with_crypto(&raw, "INBOX:1", "acc", "INBOX", Some(&bridge)).unwrap();

        assert_eq!(email.subject, "secret memo");
        assert_eq!(
            email.body_text.as_deref(),
            Some("the package is in transit\n"),
            "decrypted plaintext body must reach the Email struct"
        );
        assert_eq!(email.protection.as_deref(), Some("encrypted"));
        assert_eq!(email.signature_status, None);
        assert_eq!(email.signer_fingerprint, None);
    }

    #[test]
    fn smime_enveloped_with_inner_signature_marks_signed_and_encrypted() {
        // Forward-looking: once the nested sign-then-encrypt form is wired
        // through `decrypt_smime`, a non-None signature_status must bump
        // the label to "signed-and-encrypted" — same as the PGP path.
        let raw = smime_enveloped("SGVsbG8gU01JTUU=");
        let bridge = StubBridge {
            plaintext: smime_inner_plaintext(),
            signature_status: Some("valid".into()),
            signer_fingerprint: Some("AB:CD:EF".into()),
        };

        let email =
            parse_eml_bytes_with_crypto(&raw, "INBOX:1", "acc", "INBOX", Some(&bridge)).unwrap();

        assert_eq!(email.protection.as_deref(), Some("signed-and-encrypted"));
        assert_eq!(email.signature_status.as_deref(), Some("valid"));
        assert_eq!(email.signer_fingerprint.as_deref(), Some("AB:CD:EF"));
    }

    #[test]
    fn smime_enveloped_without_bridge_falls_back_to_plaintext() {
        // No bridge → still parseable as plain MIME (the p7m surfaces as
        // an attachment), `protection = None`.  Matches the PGP "no key
        // imported yet" behaviour: don't break the UI, just can't show
        // the contents.
        let raw = smime_enveloped("SGVsbG8gU01JTUU=");

        let email = parse_eml_bytes_with_crypto(&raw, "INBOX:1", "acc", "INBOX", None).unwrap();

        assert_eq!(email.subject, "secret memo");
        assert_eq!(email.protection, None);
        let plain = parse_eml_bytes(&raw, "INBOX:1", "acc", "INBOX").unwrap();
        assert_eq!(plain.subject, email.subject);
        assert_eq!(plain.protection, email.protection);
    }

    #[test]
    fn smime_multipart_signed_is_detected_but_not_verified() {
        // Clear-signed: the body part is readable, the detached signature
        // sits in a second `application/pkcs7-signature` part.  We stamp
        // "signed" without verifying (same deferral as the PGP path) and
        // the cleartext body must still come through.
        let raw = b"From: alice@example.com\r\n\
                    To: bob@example.com\r\n\
                    Subject: signed memo\r\n\
                    MIME-Version: 1.0\r\n\
                    Content-Type: multipart/signed; \
                        protocol=\"application/pkcs7-signature\"; \
                        micalg=\"sha-256\"; boundary=\"smime-boundary\"\r\n\
                    \r\n\
                    --smime-boundary\r\n\
                    Content-Type: text/plain; charset=\"utf-8\"\r\n\
                    \r\n\
                    the package is in transit\r\n\
                    --smime-boundary\r\n\
                    Content-Type: application/pkcs7-signature; name=\"smime.p7s\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    \r\n\
                    SGVsbG8gc2lnbmF0dXJl\r\n\
                    --smime-boundary--\r\n";
        let bridge = StubBridge {
            plaintext: b"WOULD-NEVER-BE-CALLED".to_vec(),
            signature_status: None,
            signer_fingerprint: None,
        };

        let email =
            parse_eml_bytes_with_crypto(raw, "INBOX:1", "acc", "INBOX", Some(&bridge)).unwrap();

        assert_eq!(email.subject, "signed memo");
        // No trailing newline: the CRLF before the `--smime-boundary`
        // delimiter is part of the MIME boundary, not the body content.
        assert_eq!(
            email.body_text.as_deref(),
            Some("the package is in transit"),
            "clear-signed body must be readable"
        );
        assert_eq!(email.protection.as_deref(), Some("signed"));
        assert_eq!(email.signature_status, None);
    }

    #[test]
    fn smime_opaque_signed_data_is_not_claimed() {
        // `smime-type=signed-data` is the opaque form — the content is
        // wrapped inside the CMS SignedData and we can't read it without
        // unwrapping (a follow-up).  The detector must NOT claim it as
        // something it can decrypt; it falls through to plaintext parsing
        // with `protection = None`.
        let raw = format!(
            "From: alice@example.com\r\n\
             To: bob@example.com\r\n\
             Subject: opaque signed\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: application/pkcs7-mime; smime-type=signed-data; \
                 name=\"smime.p7m\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             {}\r\n",
            "SGVsbG8gb3BhcXVl"
        )
        .into_bytes();
        let bridge = StubBridge {
            plaintext: b"WOULD-NEVER-BE-CALLED".to_vec(),
            signature_status: None,
            signer_fingerprint: None,
        };

        let email =
            parse_eml_bytes_with_crypto(&raw, "INBOX:1", "acc", "INBOX", Some(&bridge)).unwrap();

        assert_eq!(email.subject, "opaque signed");
        assert_eq!(email.protection, None);
    }
}
