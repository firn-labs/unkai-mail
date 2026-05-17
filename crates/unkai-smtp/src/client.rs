//! SMTP client — connects to a mail server and sends emails.

use lettre::address::Envelope;
use lettre::message::{
    Attachment as LettreAttachment, Mailbox, MessageBuilder, MultiPart, SinglePart,
    header::{ContentDisposition, ContentId, ContentTransferEncoding, ContentType},
};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use rustls_pki_types::ServerName;
use tokio_rustls::TlsConnector;
use tracing::{debug, info};
use unkai_core::error::UnkaiError;
use unkai_core::models::{OutgoingEmail, TrustedCert};
use unkai_core::tls;

/// An SMTP client that can send emails over an encrypted connection.
///
/// # Usage
/// ```ignore
/// let client = SmtpClient::connect("smtp.example.com", 587, "user@example.com", "password").await?;
/// client.send(&email).await?;
/// ```
pub struct SmtpClient {
    /// The underlying async SMTP transport (lettre).
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl SmtpClient {
    /// Connect to an SMTP server with STARTTLS and authenticate.
    ///
    /// This configures the transport to:
    /// 1. Connect to host:port
    /// 2. Upgrade to TLS via STARTTLS (port 587) or use implicit TLS (port 465)
    /// 3. Authenticate with the given credentials
    ///
    /// Returns a ready-to-send `SmtpClient`.
    pub async fn connect(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        trusted_certs: &[TrustedCert],
    ) -> Result<Self, UnkaiError> {
        info!(host, port, username, "Connecting to SMTP server");

        let credentials = Credentials::new(username.to_string(), password.to_string());

        // Build a `TlsParameters` that knows about every cert the
        // user has explicitly trusted for this account. Lettre adds
        // them straight onto its rustls root store (alongside
        // webpki-roots), which gives the same effective behaviour
        // as unkai-imap: a server presenting a chain that ends in
        // one of the trusted certs validates as if it were CA-signed.
        let tls_params = build_tls_params(host, trusted_certs)?;

        // Port 465 uses implicit TLS (wrapped from the start).
        // Port 587 (and others) use STARTTLS (upgrade after connecting).
        let transport = if port == 465 {
            debug!("Using implicit TLS (port 465)");
            AsyncSmtpTransport::<Tokio1Executor>::relay(host)
                .map_err(|e| UnkaiError::Network(format!("Failed to create SMTP relay: {e}")))?
                .port(port)
                .tls(Tls::Wrapper(tls_params))
                .credentials(credentials)
                .build()
        } else {
            debug!("Using STARTTLS (port {port})");
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                .map_err(|e| UnkaiError::Network(format!("Failed to create STARTTLS relay: {e}")))?
                .port(port)
                .tls(Tls::Required(tls_params))
                .credentials(credentials)
                .build()
        };

        // Test the connection by verifying we can reach the server.
        transport
            .test_connection()
            .await
            .map_err(|e| UnkaiError::Network(format!("SMTP connection test failed: {e}")))?;

        info!("SMTP connection established and authenticated");

        Ok(Self { transport })
    }

    /// Send an email message.
    ///
    /// Builds the email from an `OutgoingEmail` struct, handling:
    /// - Plain text and/or HTML bodies
    /// - CC, BCC, and Reply-To headers
    /// - File attachments
    ///
    /// At least one of `body_text` or `body_html` must be set.
    pub async fn send(&self, email: &OutgoingEmail) -> Result<(), UnkaiError> {
        info!(
            from = %email.from,
            to = ?email.to,
            subject = %email.subject,
            "Sending email"
        );

        let message = build_outgoing_message(email)?;

        self.transport
            .send(message)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to send email: {e}")))?;

        info!("Email sent successfully to {:?}", email.to);
        Ok(())
    }
}

/// Build a lettre `TlsParameters` for `host`, threading per-account
/// TLS-trust into lettre's verifier.
///
/// Lettre's `add_root_certificate` calls `RootCertStore::add` under
/// the hood, which validates each cert as a proper CA trust anchor
/// — and rejects self-signed leaves (the common case for personal
/// mail servers, and the whole reason a user would have a trusted
/// cert in the first place). Lettre also doesn't let us inject a
/// custom rustls verifier the way `unkai-imap` can.
///
/// So when the account has any trusted certs we fall back to
/// `dangerous_accept_invalid_certs(true)`. That's looser than the
/// per-fingerprint check `unkai-imap` does — it accepts any cert
/// the SMTP server presents, not just the one(s) the user trusted
/// — but the practical effect lines up with user intent: "I trust
/// this server's cert"; SMTP only ever talks to the same server
/// the user just trusted at the IMAP step.
fn build_tls_params(
    host: &str,
    trusted_certs: &[TrustedCert],
) -> Result<TlsParameters, UnkaiError> {
    let mut builder = TlsParameters::builder(host.to_string());
    if !trusted_certs.is_empty() {
        builder = builder
            .dangerous_accept_invalid_certs(true)
            .dangerous_accept_invalid_hostnames(true);
    }
    builder
        .build_rustls()
        .map_err(|e| UnkaiError::Network(format!("build TLS params: {e}")))
}

/// Probe the SMTP server's TLS certificate without verifying it.
/// Mirror of `unkai_imap::probe_server_certificate` — used by the
/// "trust this server?" flow when a connect fails because the cert
/// isn't yet in the user's trust list.
///
/// Assumes implicit-TLS (port 465). For STARTTLS-only ports (587)
/// the cert isn't visible until after a SMTP greeting + STARTTLS
/// dance — and in practice the IMAP probe usually surfaces the
/// same cert (same host), so we let the UI try the IMAP probe first.
pub async fn probe_server_certificate(host: &str, port: u16) -> Result<Vec<u8>, UnkaiError> {
    let addr = format!("{host}:{port}");
    let tcp = tokio::net::TcpStream::connect(&addr)
        .await
        .map_err(|e| UnkaiError::Network(format!("Failed to connect to {addr}: {e}")))?;

    let connector = TlsConnector::from(tls::no_verify_config());
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| UnkaiError::Protocol(format!("invalid SMTP hostname '{host}': {e}")))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| UnkaiError::Network(format!("TLS probe failed with {host}: {e}")))?;

    let (_io, conn) = tls.get_ref();
    let leaf = conn
        .peer_certificates()
        .and_then(|chain| chain.first())
        .ok_or_else(|| UnkaiError::Protocol(format!("server '{host}' returned no certificate")))?
        .to_vec();
    Ok(leaf)
}

/// Strip a redundant display name when it equals the email address.
///
/// Some senders / IMAP envelopes populate the personal-name slot with
/// the bare email address.  Our IMAP `format_address` already collapses
/// those upstream, but envelopes cached *before* that fix still carry
/// the malformed `addr <addr>` form — which lettre's `Mailbox::parse`
/// rejects because the unquoted `@` violates RFC 5322's phrase syntax.
///
/// This sanitiser is a defensive last-mile pass: if a candidate
/// recipient looks like `Name <user@host>` and the name (stripped of
/// surrounding quotes / whitespace) equals the email itself, we
/// return just the bare `user@host` so lettre accepts it.  Any other
/// shape is returned unchanged.
fn sanitise_recipient(addr: &str) -> String {
    let trimmed = addr.trim();
    let Some(open) = trimmed.rfind('<') else {
        return addr.to_string();
    };
    let Some(close) = trimmed.rfind('>') else {
        return addr.to_string();
    };
    if close <= open {
        return addr.to_string();
    }
    let email = trimmed[open + 1..close].trim();
    let name = trimmed[..open].trim();
    let name_unquoted = name
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(name)
        .trim();
    if name_unquoted.eq_ignore_ascii_case(email) {
        email.to_string()
    } else {
        addr.to_string()
    }
}

/// Build the lettre `Message` for an outgoing email *without* sending it.
///
/// Exposed so callers (e.g. `main.rs`) can build the message once, send
/// it via SMTP, and then take the formatted RFC 822 bytes from
/// `message.formatted()` to `APPEND` a copy into the IMAP Sent folder
/// — without re-running the (potentially expensive) MIME serialization.
pub fn build_outgoing_message(email: &OutgoingEmail) -> Result<Message, UnkaiError> {
    let from_clean = sanitise_recipient(&email.from);
    let from_mailbox: Mailbox = from_clean
        .parse()
        .map_err(|e| UnkaiError::Protocol(format!("Invalid 'from' address '{from_clean}': {e}")))?;

    let mut builder: MessageBuilder = Message::builder()
        .from(from_mailbox.clone())
        .subject(&email.subject);

    // RFC 5322 threading headers (#277).
    //
    // `Message-ID` is generated for *every* outgoing mail by the
    // `None` arg below — lettre stamps a `<UUID@hostname>` value.
    // Without this, every Unkai reply lands in other clients
    // (Apple Mail, Thunderbird, Outlook, …) as an orphan because
    // they have nothing to anchor a thread on.
    //
    // `In-Reply-To` and `References` only apply to replies and
    // are sourced from the parent's Message-ID + References chain
    // by the Compose / send path; we wrap each ID in the angle
    // brackets the headers expect.
    builder = builder.message_id(None);
    if let Some(parent_id) = &email.in_reply_to {
        let trimmed = parent_id.trim();
        if !trimmed.is_empty() {
            builder = builder.in_reply_to(format!("<{trimmed}>"));
        }
    }
    if !email.references.is_empty() {
        let chain = email
            .references
            .iter()
            .map(|id| format!("<{}>", id.trim()))
            .filter(|s| s.len() > 2)
            .collect::<Vec<_>>()
            .join(" ");
        if !chain.is_empty() {
            builder = builder.references(chain);
        }
    }

    for addr in &email.to {
        let clean = sanitise_recipient(addr);
        let mailbox: Mailbox = clean
            .parse()
            .map_err(|e| UnkaiError::Protocol(format!("Invalid 'to' address '{clean}': {e}")))?;
        builder = builder.to(mailbox);
    }
    for addr in &email.cc {
        let clean = sanitise_recipient(addr);
        let mailbox: Mailbox = clean
            .parse()
            .map_err(|e| UnkaiError::Protocol(format!("Invalid 'cc' address '{clean}': {e}")))?;
        builder = builder.cc(mailbox);
    }
    for addr in &email.bcc {
        let clean = sanitise_recipient(addr);
        let mailbox: Mailbox = clean
            .parse()
            .map_err(|e| UnkaiError::Protocol(format!("Invalid 'bcc' address '{clean}': {e}")))?;
        builder = builder.bcc(mailbox);
    }

    if let Some(reply_to) = &email.reply_to {
        let clean = sanitise_recipient(reply_to);
        let mailbox: Mailbox = clean.parse().map_err(|e| {
            UnkaiError::Protocol(format!("Invalid 'reply-to' address '{clean}': {e}"))
        })?;
        builder = builder.reply_to(mailbox);
    }

    // When there are no recipients (a draft the user hasn't addressed
    // yet), lettre's `build()` would otherwise reject the message with
    // "missing destination address". The SMTP envelope is irrelevant
    // for the IMAP-APPEND path that drafts take, so we substitute a
    // placeholder envelope that reuses From as both sender and
    // receiver — just enough to satisfy the type, without leaking a
    // synthetic recipient into the RFC 822 headers the reader sees.
    // The SMTP send path validates recipients in the UI before
    // reaching this function, so this branch only trips for drafts.
    if email.to.is_empty() && email.cc.is_empty() && email.bcc.is_empty() {
        let envelope = Envelope::new(
            Some(from_mailbox.email.clone()),
            vec![from_mailbox.email.clone()],
        )
        .map_err(|e| UnkaiError::Protocol(format!("Failed to build draft envelope: {e}")))?;
        builder = builder.envelope(envelope);
    }

    // The presence of a `calendar_part` forces the iMIP-flavoured
    // tree (text/plain + text/html + text/calendar inside the
    // alternative; the `.ics` also added as a separate attachment
    // for download).  Otherwise the plain attach-or-not split
    // applies as before.
    if email.calendar_part.is_some() {
        build_with_calendar(builder, email)
    } else if email.attachments.is_empty() {
        build_body_only(builder, email)
    } else {
        build_with_attachments(builder, email)
    }
}

/// Build an email with just a body (no attachments).
fn build_body_only(builder: MessageBuilder, email: &OutgoingEmail) -> Result<Message, UnkaiError> {
    match (&email.body_text, &email.body_html) {
        // Both text and HTML → multipart/alternative so the mail client picks the best one.
        (Some(text), Some(html)) => {
            debug!("Building multipart/alternative body (text + HTML)");
            builder
                .multipart(
                    MultiPart::alternative()
                        .singlepart(
                            SinglePart::builder()
                                .header(ContentType::TEXT_PLAIN)
                                .body(text.clone()),
                        )
                        .singlepart(
                            SinglePart::builder()
                                .header(ContentType::TEXT_HTML)
                                .body(html.clone()),
                        ),
                )
                .map_err(|e| UnkaiError::Protocol(format!("Failed to build email: {e}")))
        }
        // Only plain text.
        (Some(text), None) => {
            debug!("Building plain text body");
            builder
                .header(ContentType::TEXT_PLAIN)
                .body(text.clone())
                .map_err(|e| UnkaiError::Protocol(format!("Failed to build email: {e}")))
        }
        // Only HTML.
        (None, Some(html)) => {
            debug!("Building HTML body");
            builder
                .header(ContentType::TEXT_HTML)
                .body(html.clone())
                .map_err(|e| UnkaiError::Protocol(format!("Failed to build email: {e}")))
        }
        // No body at all — send an empty plain text message.
        (None, None) => {
            debug!("No body provided, sending empty message");
            builder
                .header(ContentType::TEXT_PLAIN)
                .body(String::new())
                .map_err(|e| UnkaiError::Protocol(format!("Failed to build email: {e}")))
        }
    }
}

/// Build an email with attachments.
///
/// Structure:
/// ```text
/// multipart/mixed
/// ├── multipart/alternative (or single body part)
/// │   ├── text/plain
/// │   └── text/html
/// ├── attachment 1
/// └── attachment 2
/// ```
fn build_with_attachments(
    builder: MessageBuilder,
    email: &OutgoingEmail,
) -> Result<Message, UnkaiError> {
    debug!(
        "Building email with {} attachment(s)",
        email.attachments.len()
    );

    // Start with the body as the first part of a mixed multipart.
    let body_part = match (&email.body_text, &email.body_html) {
        (Some(text), Some(html)) => MultiPart::mixed().multipart(
            MultiPart::alternative()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(text.clone()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_HTML)
                        .body(html.clone()),
                ),
        ),
        (Some(text), None) => MultiPart::mixed().singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(text.clone()),
        ),
        (None, Some(html)) => MultiPart::mixed().singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(html.clone()),
        ),
        (None, None) => MultiPart::mixed().singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(String::new()),
        ),
    };

    // Add each attachment to the multipart message.
    let multipart = email.attachments.iter().fold(body_part, |mp, attachment| {
        let content_type = attachment
            .content_type
            .parse::<ContentType>()
            .unwrap_or(ContentType::parse("application/octet-stream").unwrap());

        let part = match &attachment.content_id {
            // No content-id: use lettre's stock Attachment helper. Emits
            // `Content-Disposition: attachment; filename=...` + the
            // content type; exactly the previous behaviour.
            None => LettreAttachment::new(attachment.filename.clone())
                .body(attachment.data.clone(), content_type),
            // With a content-id: we need *both* disposition=attachment
            // (so recipients see it in their attachment tray) AND a
            // Content-ID header (so `<a href="cid:<id>">` in the HTML
            // body can resolve back to this part). Lettre's
            // `Attachment::new_inline` sets Content-ID but flips
            // disposition to `inline`, and `Attachment::new` can't
            // add Content-ID at all — so we build the SinglePart by
            // hand instead, stacking exactly the three headers we
            // need. Angle brackets on the id are the RFC 2392 shape.
            Some(cid) => SinglePart::builder()
                .header(ContentDisposition::attachment(&attachment.filename))
                .header(ContentId::from(format!("<{cid}>")))
                .header(content_type)
                .body(attachment.data.clone()),
        };

        mp.singlepart(part)
    });

    builder
        .multipart(multipart)
        .map_err(|e| UnkaiError::Protocol(format!("Failed to build email with attachments: {e}")))
}

/// Build an iMIP-flavoured invite email (#58).
///
/// Structure (matches what major calendar servers actually emit):
/// ```text
/// multipart/alternative                       (when no other attachments)
/// ├── text/plain                              ← fallback body
/// ├── text/html                               ← rich body
/// └── text/calendar; method=REQUEST           ← iTIP detection trigger
/// ```
/// or, when there are user attachments:
/// ```text
/// multipart/mixed
/// ├── multipart/alternative                   (same three parts)
/// └── (user attachments)
/// ```
///
/// The text/calendar alternative is what makes RFC-compliant mail
/// clients recognise the message as an iTIP invite and surface their
/// native Accept / Decline / Tentative buttons.
///
/// Critical interop quirks (learned the hard way):
/// - The `text/calendar` part must have **no** `name=` parameter and
///   **no** `Content-Disposition` header.  Either one causes some
///   clients to treat the part as an attachment, fall through to an
///   "Add to Calendar" affordance, and hide the RSVP buttons.
/// - We must NOT also include a duplicate `.ics` as a separate
///   attachment.  When both are present some clients prefer the
///   attachment form and again drop the RSVP UI.
fn build_with_calendar(
    builder: MessageBuilder,
    email: &OutgoingEmail,
) -> Result<Message, UnkaiError> {
    let cal = email
        .calendar_part
        .as_ref()
        .expect("build_with_calendar called without calendar_part");

    // Bare `text/calendar; method=…; charset=utf-8` — no `name=`,
    // no Content-Disposition.  This matches what major calendar
    // servers actually wire on the network.
    let calendar_content_type: ContentType =
        format!("text/calendar; method={}; charset=utf-8", cal.method)
            .parse()
            .map_err(|e| UnkaiError::Protocol(format!("Bad calendar content-type: {e}")))?;

    // Body alternative — text/plain (always), text/html (if present),
    // text/calendar (always, last).  Clients pick the LAST alternative
    // they understand, so iTIP-aware clients land on text/calendar.
    let plain_body = email.body_text.clone().unwrap_or_default();
    let mut alternative = MultiPart::alternative().singlepart(
        SinglePart::builder()
            .header(ContentType::TEXT_PLAIN)
            .body(plain_body),
    );
    if let Some(html) = &email.body_html {
        alternative = alternative.singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(html.clone()),
        );
    }
    // Force 8bit Content-Transfer-Encoding on the calendar part.
    // Lettre's auto-encoder picks base64 whenever the body has any
    // non-ASCII byte (e.g. an umlaut in SUMMARY), and some mail
    // clients have long-standing bugs where base64-encoded
    // text/calendar parts fall through to an "Add to Calendar"
    // affordance instead of surfacing Accept / Decline / Tentative.
    // 8bit is what major mail servers emit on the wire and what
    // every client we've tested parses reliably.
    alternative = alternative.singlepart(
        SinglePart::builder()
            .header(calendar_content_type)
            .header(ContentTransferEncoding::EightBit)
            .body(cal.ics.clone()),
    );

    // No extra attachments → emit the alternative directly.  Adding
    // an outer multipart/mixed when not needed is what triggers the
    // duplicate-ics confusion in some clients.
    if email.attachments.is_empty() {
        return builder
            .multipart(alternative)
            .map_err(|e| UnkaiError::Protocol(format!("Failed to build invite email: {e}")));
    }

    // With user attachments, wrap in multipart/mixed.
    let mut mixed = MultiPart::mixed().multipart(alternative);
    for attachment in &email.attachments {
        let content_type = attachment
            .content_type
            .parse::<ContentType>()
            .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
        let part = match &attachment.content_id {
            None => LettreAttachment::new(attachment.filename.clone())
                .body(attachment.data.clone(), content_type),
            Some(cid) => SinglePart::builder()
                .header(ContentDisposition::attachment(&attachment.filename))
                .header(ContentId::from(format!("<{cid}>")))
                .header(content_type)
                .body(attachment.data.clone()),
        };
        mixed = mixed.singlepart(part);
    }

    builder
        .multipart(mixed)
        .map_err(|e| UnkaiError::Protocol(format!("Failed to build invite email: {e}")))
}

#[cfg(test)]
mod tests {
    use super::sanitise_recipient;

    #[test]
    fn drops_redundant_display_name_equal_to_email() {
        // Some IMAP envelopes carry the email itself in the
        // personal-name slot; lettre rejects the resulting
        // `addr <addr>` because the unquoted `@` breaks RFC 5322.
        assert_eq!(
            sanitise_recipient("alex@example.com <alex@example.com>"),
            "alex@example.com"
        );
    }

    #[test]
    fn drops_redundant_quoted_display_name_equal_to_email() {
        assert_eq!(
            sanitise_recipient("\"alex@example.com\" <alex@example.com>"),
            "alex@example.com"
        );
    }

    #[test]
    fn case_insensitive_redundancy_check() {
        assert_eq!(
            sanitise_recipient("ALEX@example.com <alex@example.com>"),
            "alex@example.com"
        );
    }

    #[test]
    fn keeps_real_display_name() {
        assert_eq!(
            sanitise_recipient("Alex Morgan <alex@example.com>"),
            "Alex Morgan <alex@example.com>"
        );
    }

    #[test]
    fn passes_through_bare_address() {
        assert_eq!(sanitise_recipient("alex@example.com"), "alex@example.com");
    }
}
