//! SMTP client — connects to a mail server and sends emails.

use lettre::address::Envelope;
use lettre::message::{
    Attachment as LettreAttachment, Mailbox, MessageBuilder, MultiPart, SinglePart,
    header::{ContentDisposition, ContentId, ContentTransferEncoding, ContentType},
};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use rustls_pki_types::ServerName;
use tokio_rustls::TlsConnector;
use tracing::{debug, info};
use unkai_core::crypto::CryptoBridge;
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
    ///
    /// Thin wrapper around [`Self::send_with_crypto`] with no bridge —
    /// equivalent to the historical plaintext path.  Existing call
    /// sites keep their behaviour unchanged.
    pub async fn send(&self, email: &OutgoingEmail) -> Result<(), UnkaiError> {
        self.send_with_crypto(email, None).await
    }

    /// Send an email with optional end-to-end encryption (#57).
    ///
    /// When `email.encryption_mode == Some("pgp")` *and* a `bridge` is
    /// supplied, the built MIME message is wrapped in an RFC-3156
    /// `multipart/encrypted` envelope before being handed to the SMTP
    /// transport via [`AsyncTransport::send_raw`].  When either is
    /// missing, this falls back to the plaintext path so the historical
    /// behaviour is preserved by default.
    ///
    /// **BCC limitation**: PGP encryption with BCC recipients is not
    /// supported in this slice — sending one ciphertext encrypted to
    /// both visible and BCC keys would leak the BCC list via the
    /// recipient ESK packets.  Doing this safely requires sending one
    /// envelope per BCC recipient (a separate refactor); for now we
    /// surface a clear `UnkaiError::Protocol` instead of silently
    /// leaking.  Tracked as a follow-up under #57.
    pub async fn send_with_crypto(
        &self,
        email: &OutgoingEmail,
        bridge: Option<&dyn CryptoBridge>,
    ) -> Result<(), UnkaiError> {
        info!(
            from = %email.from,
            to = ?email.to,
            subject = %email.subject,
            encryption_mode = ?email.encryption_mode,
            signing_enabled = email.signing_enabled,
            "Sending email"
        );

        let wants_encryption = email.encryption_mode.as_deref() == Some("pgp");
        let wants_sign_only = email.signing_enabled && !wants_encryption;

        if wants_sign_only {
            // PGP/MIME `multipart/signed` requires canonicalising the
            // signed body bytes (RFC 3156 §5).  We haven't wired that
            // in yet (TODO inside the IMAP receive path mentions the
            // same gap on the verify side).  Refusing loudly is much
            // better than silently sending plaintext under a "Signed"
            // label the user would trust.
            return Err(UnkaiError::Protocol(
                "Sign-only PGP/MIME (`multipart/signed`) not yet supported; \
                 enable encryption alongside signing for #57"
                    .into(),
            ));
        }

        if !wants_encryption {
            // Plaintext path — historical behaviour, no MIME wrapping.
            let message = build_outgoing_message(email)?;
            self.transport
                .send(message)
                .await
                .map_err(|e| UnkaiError::Protocol(format!("Failed to send email: {e}")))?;
            info!("Email sent successfully to {:?}", email.to);
            return Ok(());
        }

        let bridge = bridge.ok_or_else(|| {
            UnkaiError::Crypto(
                "encryption_mode='pgp' requested but no CryptoBridge supplied — \
                 the Tauri command layer must compose one"
                    .into(),
            )
        })?;

        if !email.bcc.is_empty() {
            return Err(UnkaiError::Protocol(
                "PGP encryption with BCC recipients is not yet supported — \
                 BCC keys would leak via the OpenPGP ESK packets. \
                 Send to BCC recipients separately."
                    .into(),
            ));
        }

        let outer_bytes = wrap_as_pgp_mime(email, bridge)?;
        let envelope = envelope_from_email(email)?;

        self.transport
            .send_raw(&envelope, &outer_bytes)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to send encrypted email: {e}")))?;

        info!("Encrypted email sent successfully to {:?}", email.to);
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

/// Build the lettre `Envelope` (SMTP routing only — MAIL FROM + RCPT TO)
/// for an outgoing email.  Used together with [`AsyncTransport::send_raw`]
/// on the PGP/MIME path so we can hand the wire-format bytes we
/// constructed ourselves to lettre and still let it negotiate the SMTP
/// transaction.
///
/// `from` and every entry in `to + cc` is parsed through `Mailbox::parse`
/// to inherit the same display-name handling the plaintext path uses;
/// we then take the `.email` portion since `Envelope` doesn't carry the
/// display name (it's just the SMTP-level address list).  BCC is
/// intentionally **not** added to the envelope here — the only PGP/MIME
/// path that calls this currently rejects non-empty BCC before reaching
/// us, and adding them later requires per-recipient encryption.
fn envelope_from_email(email: &OutgoingEmail) -> Result<Envelope, UnkaiError> {
    let from_mailbox: Mailbox = sanitise_recipient(&email.from)
        .parse()
        .map_err(|e| UnkaiError::Protocol(format!("Invalid 'from' address: {e}")))?;
    let from_addr: Address = from_mailbox.email;

    let mut rcpts: Vec<Address> = Vec::new();
    for r in email.to.iter().chain(email.cc.iter()) {
        let mb: Mailbox = sanitise_recipient(r)
            .parse()
            .map_err(|e| UnkaiError::Protocol(format!("Invalid recipient '{r}': {e}")))?;
        rcpts.push(mb.email);
    }
    Envelope::new(Some(from_addr), rcpts)
        .map_err(|e| UnkaiError::Protocol(format!("build SMTP envelope: {e}")))
}

/// Wrap a plaintext `OutgoingEmail` as an RFC-3156 `multipart/encrypted`
/// PGP/MIME message and return the raw RFC-822 byte form ready for
/// `transport.send_raw`.
///
/// Flow:
///   1. Build the plaintext MIME message the way we always have
///      ([`build_outgoing_message`]).
///   2. Serialise it to bytes via lettre's `.formatted()`.
///   3. Hand those bytes to the bridge along with the visible
///      recipient list.  The bridge returns armored OpenPGP
///      ciphertext.
///   4. Emit a hand-built outer RFC-822 message with the routing
///      headers from the original `OutgoingEmail` (so the recipient
///      sees a normal `From`/`To`/`Subject` in their inbox) plus
///      a two-part `multipart/encrypted` body carrying the
///      ciphertext per RFC 3156 §4.
///
/// The inner serialisation already includes `From:`/`To:`/`Subject:`
/// headers — duplicated in the outer wrapper — which is the
/// header-protection-by-duplication pattern most existing PGP/MIME
/// clients produce.  RFC 9533 ("Header Protection") suggests a
/// stricter form; we'll evaluate adopting it once interop with the
/// common clients is proven.
fn wrap_as_pgp_mime(
    email: &OutgoingEmail,
    bridge: &dyn CryptoBridge,
) -> Result<Vec<u8>, UnkaiError> {
    let inner_message = build_outgoing_message(email)?;
    let inner_bytes = inner_message.formatted();

    let recipients: Vec<String> = email
        .to
        .iter()
        .chain(email.cc.iter())
        .map(|a| {
            // Extract just the address portion when the entry is
            // `Name <addr@host>` — recipient lookup in the bridge's
            // public-key cache is keyed on bare email.
            sanitise_recipient(a)
                .parse::<Mailbox>()
                .map(|mb| mb.email.to_string())
                .unwrap_or_else(|_| a.clone())
        })
        .collect();

    let encrypted = bridge.encrypt(&inner_bytes, &recipients, email.signing_enabled)?;
    Ok(build_outer_pgp_mime_bytes(
        email,
        &encrypted.ciphertext_armor,
    ))
}

/// Pure-function MIME envelope builder for the PGP/MIME outer.  Lives
/// outside [`wrap_as_pgp_mime`] so the structure can be unit-tested
/// against a fixed ciphertext without spinning up a real bridge or
/// transport.  All header strings are emitted with CRLF endings as
/// RFC 5322 requires (lettre normally handles this for us; we have
/// to do it ourselves on the hand-built outer).
fn build_outer_pgp_mime_bytes(email: &OutgoingEmail, ciphertext_armor: &[u8]) -> Vec<u8> {
    // Boundary string is just a random ASCII tag that can't appear in
    // either body part.  We use a UUID prefix so the chance of
    // collision with the ciphertext armor or the inner MIME is
    // effectively zero.
    let boundary = format!("unkai-pgp-mime-{}", uuid::Uuid::new_v4().simple());
    let message_id = format!("<{}@unkai-mail.local>", uuid::Uuid::new_v4().simple());
    let date = chrono::Utc::now().to_rfc2822();

    let mut headers = String::new();
    headers.push_str(&format!("From: {}\r\n", email.from));
    if !email.to.is_empty() {
        headers.push_str(&format!("To: {}\r\n", email.to.join(", ")));
    }
    if !email.cc.is_empty() {
        headers.push_str(&format!("Cc: {}\r\n", email.cc.join(", ")));
    }
    if let Some(reply_to) = &email.reply_to {
        headers.push_str(&format!("Reply-To: {reply_to}\r\n"));
    }
    headers.push_str(&format!("Subject: {}\r\n", email.subject));
    headers.push_str(&format!("Date: {date}\r\n"));
    headers.push_str(&format!("Message-ID: {message_id}\r\n"));
    if let Some(parent) = &email.in_reply_to {
        headers.push_str(&format!("In-Reply-To: <{parent}>\r\n"));
    }
    if !email.references.is_empty() {
        let refs = email
            .references
            .iter()
            .map(|r| format!("<{r}>"))
            .collect::<Vec<_>>()
            .join(" ");
        headers.push_str(&format!("References: {refs}\r\n"));
    }
    headers.push_str("MIME-Version: 1.0\r\n");
    headers.push_str(&format!(
        "Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"{boundary}\"\r\n"
    ));

    let mut body = String::new();
    body.push_str("\r\n");
    body.push_str(&format!("--{boundary}\r\n"));
    body.push_str("Content-Type: application/pgp-encrypted\r\n");
    body.push_str("Content-Description: PGP/MIME version identification\r\n");
    body.push_str("\r\n");
    body.push_str("Version: 1\r\n");
    body.push_str("\r\n");
    body.push_str(&format!("--{boundary}\r\n"));
    body.push_str("Content-Type: application/octet-stream; name=\"encrypted.asc\"\r\n");
    body.push_str("Content-Description: OpenPGP encrypted message\r\n");
    body.push_str("Content-Disposition: inline; filename=\"encrypted.asc\"\r\n");
    body.push_str("\r\n");

    let mut out = headers.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(ciphertext_armor);
    if !ciphertext_armor.ends_with(b"\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
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

    // ── PGP/MIME outer envelope construction (#57) ─────────────

    use super::{build_outer_pgp_mime_bytes, envelope_from_email};
    use mail_parser::{MessageParser, MimeHeaders};
    use unkai_core::models::OutgoingEmail;

    /// Minimal `OutgoingEmail` for the wrapper tests.  Skips
    /// attachments / calendar parts because the outer wrapper
    /// only carries routing headers + the ciphertext body —
    /// the inner body's complexity is the bridge's problem,
    /// not the wrapper's.
    fn outgoing(subject: &str, to: &[&str]) -> OutgoingEmail {
        OutgoingEmail {
            from: "alice@example.com".into(),
            to: to.iter().map(|s| s.to_string()).collect(),
            cc: vec![],
            bcc: vec![],
            reply_to: None,
            subject: subject.into(),
            body_text: Some("ignored — wrapper test".into()),
            body_html: None,
            attachments: vec![],
            calendar_part: None,
            skip_sent_copy: false,
            in_reply_to: None,
            references: vec![],
            encryption_mode: Some("pgp".into()),
            signing_enabled: false,
        }
    }

    #[test]
    fn outer_wrapper_advertises_multipart_encrypted_pgp_protocol() {
        let email = outgoing("secret memo", &["bob@example.com"]);
        let ciphertext =
            b"-----BEGIN PGP MESSAGE-----\nVERSION-PLACEHOLDER\n-----END PGP MESSAGE-----\n";
        let wire = build_outer_pgp_mime_bytes(&email, ciphertext);

        let parsed = MessageParser::default()
            .parse(&wire)
            .expect("outer must round-trip through mail-parser");
        let ct = parsed.content_type().expect("must carry a Content-Type");
        assert!(
            ct.ctype().eq_ignore_ascii_case("multipart"),
            "ctype = {}",
            ct.ctype()
        );
        assert_eq!(ct.subtype().unwrap_or(""), "encrypted");
        assert_eq!(
            ct.attribute("protocol").unwrap_or(""),
            "application/pgp-encrypted"
        );
        assert_eq!(parsed.subject().unwrap_or(""), "secret memo");
    }

    #[test]
    fn outer_wrapper_carries_ciphertext_in_octet_stream_part() {
        let email = outgoing("inbox-routable subject", &["bob@example.com"]);
        let ciphertext =
            b"-----BEGIN PGP MESSAGE-----\nWOULD-BE-OPAQUE\n-----END PGP MESSAGE-----\n";
        let wire = build_outer_pgp_mime_bytes(&email, ciphertext);

        let parsed = MessageParser::default().parse(&wire).expect("parse outer");

        // Find the application/octet-stream part — that's where the
        // armored ciphertext lives per RFC 3156 §4.  We walk parts
        // because mail-parser's flat index isn't a constant `(1, 2)`
        // — same reason the IMAP receive interceptor scans rather
        // than indexing.
        let octet = (0..)
            .map_while(|i| parsed.part(i))
            .find(|p| {
                p.content_type().is_some_and(|c| {
                    c.ctype().eq_ignore_ascii_case("application")
                        && c.subtype()
                            .is_some_and(|s| s.eq_ignore_ascii_case("octet-stream"))
                })
            })
            .expect("ciphertext part must exist");
        let body = std::str::from_utf8(octet.contents()).expect("ciphertext bytes are utf-8");
        assert!(
            body.contains("-----BEGIN PGP MESSAGE-----"),
            "octet-stream body must carry the armor; got: {body}"
        );
        assert!(body.contains("WOULD-BE-OPAQUE"));
    }

    #[test]
    fn outer_wrapper_carries_version_part_first() {
        let email = outgoing("hi", &["bob@example.com"]);
        let wire = build_outer_pgp_mime_bytes(
            &email,
            b"-----BEGIN PGP MESSAGE-----\nx\n-----END PGP MESSAGE-----\n",
        );
        let parsed = MessageParser::default().parse(&wire).unwrap();

        // First non-root part must be `application/pgp-encrypted`
        // with the literal `Version: 1`.  RFC 3156 §4 fixes this
        // order — older PGP-aware clients reject the message if
        // the version part isn't first.
        let version_part = (0..)
            .map_while(|i| parsed.part(i))
            .find(|p| {
                p.content_type().is_some_and(|c| {
                    c.ctype().eq_ignore_ascii_case("application")
                        && c.subtype()
                            .is_some_and(|s| s.eq_ignore_ascii_case("pgp-encrypted"))
                })
            })
            .expect("version part must exist");
        let body = std::str::from_utf8(version_part.contents()).expect("utf-8");
        assert!(body.contains("Version: 1"));
    }

    #[test]
    fn envelope_from_email_includes_cc_but_not_bcc() {
        let mut email = outgoing("e2e", &["primary@example.com"]);
        email.cc = vec!["copied@example.com".into()];
        email.bcc = vec!["hidden@example.com".into()];

        let env = envelope_from_email(&email).expect("envelope must build");
        let rcpts: Vec<String> = env.to().iter().map(|a| a.to_string()).collect();

        assert!(rcpts.contains(&"primary@example.com".into()));
        assert!(rcpts.contains(&"copied@example.com".into()));
        assert!(
            !rcpts.contains(&"hidden@example.com".into()),
            "BCC must not appear at the envelope layer — would leak via Received headers"
        );
    }
}
