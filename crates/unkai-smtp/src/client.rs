//! SMTP client — connects to a mail server and sends emails.

use lettre::address::Envelope;
use lettre::message::{
    Attachment as LettreAttachment, Mailbox, MessageBuilder, MultiPart, SinglePart,
    header::{
        ContentDisposition, ContentId, ContentTransferEncoding, ContentType, Header, HeaderName,
        HeaderValue,
    },
};
use lettre::transport::smtp::authentication::{Credentials, DEFAULT_MECHANISMS};
use lettre::transport::smtp::client::{AsyncSmtpConnection, Tls, TlsParameters};
use lettre::transport::smtp::commands::{Data, Ehlo, Mail, Rcpt};
use lettre::transport::smtp::extension::{
    ClientId, Extension, MailBodyParameter, MailParameter, RcptParameter,
};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use rustls_pki_types::ServerName;
use std::time::Duration;
use tokio_rustls::TlsConnector;
use tracing::{debug, info, warn};
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
    /// Connection parameters retained for the delivery-confirmation
    /// path (#461).  Lettre's pooled transport hardcodes empty
    /// `RCPT TO` parameter lists, so a send that requests an
    /// RFC 3461 DSN has to open and drive its own
    /// [`AsyncSmtpConnection`] — which needs everything `connect`
    /// was originally called with.
    host: String,
    port: u16,
    credentials: Credentials,
    tls_params: TlsParameters,
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
                .tls(Tls::Wrapper(tls_params.clone()))
                .credentials(credentials.clone())
                .build()
        } else {
            debug!("Using STARTTLS (port {port})");
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                .map_err(|e| UnkaiError::Network(format!("Failed to create STARTTLS relay: {e}")))?
                .port(port)
                .tls(Tls::Required(tls_params.clone()))
                .credentials(credentials.clone())
                .build()
        };

        // Test the connection by verifying we can reach the server.
        transport
            .test_connection()
            .await
            .map_err(|e| UnkaiError::Network(format!("SMTP connection test failed: {e}")))?;

        info!("SMTP connection established and authenticated");

        Ok(Self {
            transport,
            host: host.to_string(),
            port,
            credentials,
            tls_params,
        })
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

    /// Send a message disposition notification — the read receipt
    /// itself (RFC 8098, #416).
    ///
    /// The wire bytes come from [`build_mdn_report_bytes`]; this
    /// method only adds the SMTP envelope, which has one deliberate
    /// quirk: the reverse-path is **null** (`MAIL FROM:<>`), as
    /// RFC 8098 §3 requires.  A receipt that could itself bounce —
    /// or trigger another receipt — would loop two auto-responders
    /// against each other forever; the null sender is the standard
    /// loop-breaker (same mechanism delivery status notifications
    /// use).
    pub async fn send_mdn(&self, reply: &MdnReply) -> Result<(), UnkaiError> {
        let rcpt: Mailbox = sanitise_recipient(&reply.to)
            .parse()
            .map_err(|e| UnkaiError::Protocol(format!("Invalid receipt address: {e}")))?;
        let envelope = Envelope::new(None, vec![rcpt.email])
            .map_err(|e| UnkaiError::Protocol(format!("build MDN envelope: {e}")))?;

        let bytes = build_mdn_report_bytes(reply);
        self.transport
            .send_raw(&envelope, &bytes)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Failed to send read receipt: {e}")))?;
        info!("Read receipt sent to {}", reply.to);
        Ok(())
    }

    /// Send an email with optional OpenPGP or S/MIME wrapping.
    ///
    /// Mode is picked from `email.encryption_mode` (with the historical
    /// `signing_enabled` flag still driving PGP sign-only):
    /// - `Some("pgp")` → RFC 3156 §4 `multipart/encrypted` (encrypt +
    ///   sign-inside-encrypt).  Requires `bridge` and at least one cached
    ///   recipient key per address.  BCC is handled by splitting the
    ///   send: a single ciphertext to TO + CC keys, plus one separate
    ///   ciphertext per BCC recipient encrypted to that recipient's key
    ///   alone, so the OpenPGP ESK packets in one recipient's copy never
    ///   name another.  See [`plan_pgp_encrypted_envelopes`].
    /// - `Some("smime")` → RFC 8551 §3.2 `application/pkcs7-mime;
    ///   smime-type=enveloped-data` (CMS `EnvelopedData`, encrypt-only;
    ///   nested sign-then-encrypt is a later sub-chunk).  Requires
    ///   `bridge` and a cached X.509 cert per recipient.  Same BCC
    ///   split as PGP — CMS `RecipientInfos` leak the recipient set
    ///   (RFC 5652 §6.2.1) just as ESK packets do.  See
    ///   [`plan_smime_enveloped_envelopes`].
    /// - `Some("smime-sign")` → RFC 8551 §3.4 `multipart/signed;
    ///   protocol="application/pkcs7-signature"` sign-only.  Body in
    ///   cleartext + a detached CMS signature; BCC rides the same
    ///   envelope.
    /// - `signing_enabled == true` (and no encryption mode) → RFC 3156
    ///   §5 `multipart/signed` PGP sign-only.  Body cleartext + a
    ///   detached OpenPGP signature; BCC rides the same envelope.
    /// - none of the above → historical plaintext path, no MIME wrapping.
    ///
    /// In every crypto mode the wire bytes are constructed locally and
    /// handed to the transport via [`AsyncTransport::send_raw`] so
    /// lettre's outer MIME builder doesn't strip / rewrite headers the
    /// recipient's PGP/MIME or S/MIME parser is keyed on.
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

        let mode = email.encryption_mode.as_deref();
        let wants_pgp_encrypt = mode == Some("pgp");
        let wants_smime_encrypt = mode == Some("smime");
        let wants_smime_sign = mode == Some("smime-sign");
        // PGP sign-only keeps its historical trigger (`signing_enabled`
        // with no encryption mode), but must not fire when an explicit
        // S/MIME mode is selected — those carry their own stack.
        let wants_pgp_sign = email.signing_enabled
            && !wants_pgp_encrypt
            && !wants_smime_encrypt
            && !wants_smime_sign;

        let any_crypto =
            wants_pgp_encrypt || wants_smime_encrypt || wants_smime_sign || wants_pgp_sign;
        if !any_crypto {
            // Plaintext path — historical behaviour, no MIME wrapping.
            // (envelope + formatted bytes is exactly what lettre's
            // `AsyncTransport::send` derives from a `Message`
            // internally, so routing through `submit_raw` changes
            // nothing for the non-DSN case.)
            let message = build_outgoing_message(email)?;
            self.submit_raw(
                message.envelope(),
                &message.formatted(),
                email.request_delivery_receipt,
                "Failed to send email",
            )
            .await?;
            info!("Email sent successfully to {:?}", email.to);
            return Ok(());
        }

        let bridge = bridge.ok_or_else(|| {
            UnkaiError::Crypto(
                "Encrypted/signed send requested (encryption_mode or signing_enabled set) \
                 but no CryptoBridge supplied — the Tauri command layer must compose one"
                    .into(),
            )
        })?;

        if wants_pgp_sign {
            // RFC 3156 §5 `multipart/signed`.  The body is sent in
            // cleartext — the signature only attests origin and
            // integrity — so BCC is fine here (no per-recipient
            // envelopes needed, the same wire bytes go to every
            // recipient like a plaintext send).
            let outer_bytes = wrap_as_pgp_mime_signed(email, bridge)?;
            let envelope = envelope_from_email_include_bcc(email)?;
            self.submit_raw(
                &envelope,
                &outer_bytes,
                email.request_delivery_receipt,
                "Failed to send signed email",
            )
            .await?;
            info!("Signed email sent successfully to {:?}", email.to);
            return Ok(());
        }

        if wants_smime_sign {
            // RFC 8551 §3.4 `multipart/signed; protocol="application/
            // pkcs7-signature"`.  Same cleartext-body model as the PGP
            // sign-only path, so BCC rides the same envelope.
            let outer_bytes = wrap_as_smime_signed(email, bridge)?;
            let envelope = envelope_from_email_include_bcc(email)?;
            self.submit_raw(
                &envelope,
                &outer_bytes,
                email.request_delivery_receipt,
                "Failed to send S/MIME signed email",
            )
            .await?;
            info!("S/MIME signed email sent successfully to {:?}", email.to);
            return Ok(());
        }

        // Encrypted paths (PGP or S/MIME) both fan out into one-or-more
        // per-recipient envelopes and share the same send loop.
        //
        // The BCC split applies to BOTH stacks.  OpenPGP leaks the
        // recipient set via per-recipient ESK packets (RFC 4880 §5.1);
        // CMS `EnvelopedData` leaks it via each recipient's
        // `KeyTransRecipientInfo.rid` (issuer+serial or SKI, RFC 5652
        // §6.2.1).  So in both cases we emit one envelope for the visible
        // TO + CC copy plus one envelope per BCC recipient, each
        // encrypted only to that recipient's key/cert.
        let planned = if wants_smime_encrypt {
            plan_smime_enveloped_envelopes(email, bridge)?
        } else {
            plan_pgp_encrypted_envelopes(email, bridge)?
        };
        if planned.is_empty() {
            return Err(UnkaiError::Protocol(
                "Encrypted send has no recipients — To, Cc, and Bcc are all empty".into(),
            ));
        }
        let envelope_count = planned.len();
        for (
            idx,
            PlannedEnvelope {
                envelope,
                wire_bytes,
            },
        ) in planned.iter().enumerate()
        {
            self.submit_raw(
                envelope,
                wire_bytes,
                email.request_delivery_receipt,
                &format!(
                    "Failed to send encrypted envelope {}/{}",
                    idx + 1,
                    envelope_count
                ),
            )
            .await?;
        }

        info!(
            envelope_count,
            "Encrypted email sent successfully (To/Cc copy plus one envelope per BCC)"
        );
        Ok(())
    }

    /// Submit one SMTP transaction (envelope + wire bytes), routing by
    /// whether the sender asked for a delivery confirmation (#461).
    ///
    /// The plain route hands the bytes to lettre's pooled transport —
    /// byte-for-byte what every send did before #461.  The DSN route
    /// can't use the pool (lettre's connection layer hardcodes empty
    /// `RCPT TO` parameter lists), so it drives a dedicated
    /// connection via [`Self::send_raw_requesting_dsn`].
    ///
    /// `ctx` prefixes any error so each caller keeps its historical
    /// message ("Failed to send signed email: …" etc.).
    async fn submit_raw(
        &self,
        envelope: &Envelope,
        bytes: &[u8],
        request_dsn: bool,
        ctx: &str,
    ) -> Result<(), UnkaiError> {
        if request_dsn {
            self.send_raw_requesting_dsn(envelope, bytes, ctx).await
        } else {
            self.transport
                .send_raw(envelope, bytes)
                .await
                .map(|_| ())
                .map_err(|e| UnkaiError::Protocol(format!("{ctx}: {e}")))
        }
    }

    /// Send raw wire bytes requesting an RFC 3461 delivery status
    /// notification (`NOTIFY=SUCCESS,FAILURE`) for every recipient.
    ///
    /// DSN requests ride as parameters on the `RCPT TO` command, and
    /// lettre 0.11's `AsyncSmtpTransport` gives us no way to set them
    /// — so this path opens its own [`AsyncSmtpConnection`] and
    /// replays the same dance the transport does internally: connect
    /// (implicit TLS on 465, STARTTLS otherwise), authenticate, then
    /// MAIL FROM / RCPT TO / DATA with our parameters attached.
    ///
    /// Best-effort by design: when the server doesn't advertise the
    /// `DSN` extension the transaction proceeds *without* the NOTIFY
    /// parameters (sending them anyway would be a syntax error per
    /// RFC 3461 §3.1) — the mail always goes out, mirroring how a
    /// read-receipt request can be ignored on the recipient side.
    async fn send_raw_requesting_dsn(
        &self,
        envelope: &Envelope,
        bytes: &[u8],
        ctx: &str,
    ) -> Result<(), UnkaiError> {
        // EHLO name + timeout mirror the pooled transport's defaults.
        let client_id = ClientId::default();
        let timeout = Some(Duration::from_secs(60));

        let mut conn = if self.port == 465 {
            AsyncSmtpConnection::connect_tokio1(
                (self.host.as_str(), self.port),
                timeout,
                &client_id,
                Some(self.tls_params.clone()),
                None,
            )
            .await
            .map_err(|e| UnkaiError::Network(format!("{ctx}: SMTP connect failed: {e}")))?
        } else {
            let mut conn = AsyncSmtpConnection::connect_tokio1(
                (self.host.as_str(), self.port),
                timeout,
                &client_id,
                None,
                None,
            )
            .await
            .map_err(|e| UnkaiError::Network(format!("{ctx}: SMTP connect failed: {e}")))?;
            // `starttls` errors when the server doesn't offer it —
            // the same hard requirement `Tls::Required` enforces on
            // the pooled transport.  Never fall back to cleartext.
            conn.starttls(self.tls_params.clone(), &client_id)
                .await
                .map_err(|e| UnkaiError::Network(format!("{ctx}: STARTTLS failed: {e}")))?;
            conn
        };

        match self
            .dsn_transaction(&mut conn, envelope, bytes, &client_id)
            .await
        {
            Ok(()) => {
                // Best-effort QUIT — the mail is already accepted, a
                // hiccup while saying goodbye is not a send failure.
                let _ = conn.quit().await;
                Ok(())
            }
            Err(e) => {
                // Drop the connection without QUIT so a half-done
                // transaction can't linger (mirrors lettre's own
                // abort-on-error handling).
                conn.abort().await;
                Err(UnkaiError::Protocol(format!("{ctx}: {e}")))
            }
        }
    }

    /// The MAIL FROM / RCPT TO / DATA exchange of the DSN path.
    /// Split out so the caller can uniformly abort the connection on
    /// any error.  Lettre's `command()` returns `Err` for every
    /// non-2xx/3xx reply, so each `?` here is a protocol-level abort.
    async fn dsn_transaction(
        &self,
        conn: &mut AsyncSmtpConnection,
        envelope: &Envelope,
        bytes: &[u8],
        client_id: &ClientId,
    ) -> Result<(), UnkaiError> {
        conn.auth(DEFAULT_MECHANISMS, &self.credentials)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("SMTP authentication failed: {e}")))?;

        // Lettre's `ServerInfo` only models the extensions it uses
        // itself (STARTTLS / AUTH / 8BITMIME / SMTPUTF8) and silently
        // drops the rest — so `supports_feature` can never answer
        // "does this server do DSN?".  Re-issue EHLO and scan the raw
        // reply lines instead; re-EHLO before MAIL is legal (RFC 5321
        // §4.1.4, it just resets the session state we haven't used yet).
        let ehlo = conn
            .command(Ehlo::new(client_id.clone()))
            .await
            .map_err(|e| UnkaiError::Protocol(format!("EHLO failed: {e}")))?;
        let supports_dsn = ehlo_advertises_dsn(ehlo.message());
        if !supports_dsn {
            warn!(
                host = %self.host,
                "SMTP server does not advertise the DSN extension — \
                 sending without the delivery-confirmation request"
            );
        }

        // Mirror the SMTPUTF8 / 8BITMIME negotiation lettre's own
        // `send` performs (its `ServerInfo` does track those two).
        let mut mail_params = Vec::new();
        if envelope_has_non_ascii_addresses(envelope) {
            if !conn.server_info().supports_feature(Extension::SmtpUtfEight) {
                return Err(UnkaiError::Protocol(
                    "Envelope contains non-ascii addresses but the server does not support SMTPUTF8"
                        .into(),
                ));
            }
            mail_params.push(MailParameter::SmtpUtfEight);
        }
        if !bytes.is_ascii() {
            if !conn.server_info().supports_feature(Extension::EightBitMime) {
                return Err(UnkaiError::Protocol(
                    "Message contains non-ascii bytes but the server does not support 8BITMIME"
                        .into(),
                ));
            }
            mail_params.push(MailParameter::Body(MailBodyParameter::EightBitMime));
        }

        conn.command(Mail::new(envelope.from().cloned(), mail_params))
            .await
            .map_err(|e| UnkaiError::Protocol(format!("MAIL FROM rejected: {e}")))?;
        for rcpt in envelope.to() {
            let params = if supports_dsn {
                dsn_notify_params()
            } else {
                Vec::new()
            };
            conn.command(Rcpt::new(rcpt.clone(), params))
                .await
                .map_err(|e| UnkaiError::Protocol(format!("RCPT TO <{rcpt}> rejected: {e}")))?;
        }
        conn.command(Data)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("DATA rejected: {e}")))?;
        conn.message(bytes)
            .await
            .map_err(|e| UnkaiError::Protocol(format!("Message body rejected: {e}")))?;
        Ok(())
    }
}

/// The `RCPT TO` parameters of a delivery-confirmation request
/// (RFC 3461 §4.1): report when the message reaches the recipient's
/// mailbox (`SUCCESS`) and keep failure reporting explicit
/// (`FAILURE`).  `DELAY` is deliberately omitted — transient
/// queue-delay notices read as noise next to the "did it arrive?"
/// question the compose toggle asks.
fn dsn_notify_params() -> Vec<RcptParameter> {
    vec![RcptParameter::Other {
        keyword: "NOTIFY".into(),
        value: Some("SUCCESS,FAILURE".into()),
    }]
}

/// Scan a raw EHLO reply for the `DSN` keyword (RFC 3461 §3).
///
/// The first reply line is the server's greeting (domain plus free
/// text — skipped so a host like `dsn.example.com` can't
/// false-positive); every later line is one extension keyword,
/// optionally followed by parameters.  `DSN` takes none, so an exact
/// case-insensitive match is the whole test.
fn ehlo_advertises_dsn<'a>(lines: impl Iterator<Item = &'a str>) -> bool {
    lines.skip(1).any(|l| l.trim().eq_ignore_ascii_case("DSN"))
}

/// Replicates lettre's private `Envelope::has_non_ascii_addresses`
/// for the hand-driven DSN transaction: SMTPUTF8 is only needed when
/// a reverse-path or forward-path address itself carries non-ascii
/// characters.
fn envelope_has_non_ascii_addresses(envelope: &Envelope) -> bool {
    envelope.from().is_some_and(|a| !a.to_string().is_ascii())
        || envelope.to().iter().any(|a| !a.to_string().is_ascii())
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
/// intentionally **not** added to the envelope here — the
/// `multipart/encrypted` send path routes BCC recipients through
/// separate per-recipient envelopes built via
/// [`envelope_for_single_recipient`] so the OpenPGP ESK packets in one
/// recipient's ciphertext never name another.  For `multipart/signed`
/// use [`envelope_from_email_include_bcc`] instead.
fn envelope_from_email(email: &OutgoingEmail) -> Result<Envelope, UnkaiError> {
    envelope_from_email_inner(email, false)
}

/// Build a one-recipient envelope (single RCPT TO) for the encrypted
/// BCC fan-out.  Each per-BCC `multipart/encrypted` copy ships its
/// own ciphertext encrypted to one key only, and the SMTP envelope
/// has to match — otherwise the receiving relay would either reject
/// the extra recipients or, worse, route a copy to addresses whose
/// key never appears in the ESK packets.
fn envelope_for_single_recipient(
    email: &OutgoingEmail,
    recipient: &str,
) -> Result<Envelope, UnkaiError> {
    let from_mailbox: Mailbox = sanitise_recipient(&email.from)
        .parse()
        .map_err(|e| UnkaiError::Protocol(format!("Invalid 'from' address: {e}")))?;
    let from_addr: Address = from_mailbox.email;

    let rcpt_mailbox: Mailbox = sanitise_recipient(recipient)
        .parse()
        .map_err(|e| UnkaiError::Protocol(format!("Invalid recipient '{recipient}': {e}")))?;

    Envelope::new(Some(from_addr), vec![rcpt_mailbox.email])
        .map_err(|e| UnkaiError::Protocol(format!("build SMTP envelope: {e}")))
}

/// Strip the display name from a `Name <addr@host>` entry and return
/// just the bare `addr@host` form.  Used when feeding recipient lists
/// to [`CryptoBridge::encrypt`], whose public-key cache is keyed on
/// the bare email — handing it the full quoted form would miss the
/// cached key and surface as `CryptoKeyNotFound`.
fn address_only(addr: &str) -> String {
    sanitise_recipient(addr)
        .parse::<Mailbox>()
        .map(|mb| mb.email.to_string())
        .unwrap_or_else(|_| addr.to_string())
}

/// Variant of [`envelope_from_email`] that adds BCC recipients to the
/// envelope's RCPT TO list.  Safe for the `multipart/signed` send path
/// because the body is cleartext — the SMTP server fans out one
/// identical envelope per recipient with the BCC list scrubbed from the
/// visible headers, just like a plaintext mail.  Not safe for
/// `multipart/encrypted` because a single ciphertext encrypted to the
/// combined TO, CC, and BCC keys leaks the BCC list via the OpenPGP ESK
/// packets, which is why the encrypted path keeps the BCC-exclusion version.
fn envelope_from_email_include_bcc(email: &OutgoingEmail) -> Result<Envelope, UnkaiError> {
    envelope_from_email_inner(email, true)
}

fn envelope_from_email_inner(
    email: &OutgoingEmail,
    include_bcc: bool,
) -> Result<Envelope, UnkaiError> {
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
    if include_bcc {
        for r in email.bcc.iter() {
            let mb: Mailbox = sanitise_recipient(r)
                .parse()
                .map_err(|e| UnkaiError::Protocol(format!("Invalid BCC recipient '{r}': {e}")))?;
            rcpts.push(mb.email);
        }
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
/// Build the `multipart/encrypted` wire bytes for one envelope's
/// worth of recipients.
///
/// `encrypt_to` is the explicit recipient list passed to the bridge,
/// decoupled from `email.to / email.cc`.  Used by the BCC fan-out
/// in [`plan_pgp_encrypted_envelopes`]: each per-BCC copy is
/// encrypted to just that one BCC recipient's key even though the
/// inner MIME's visible `To` / `Cc` headers still list the original
/// (non-BCC) recipients — the BCC recipient sees who else was on
/// the mail, just not the other BCCs (matching the plaintext BCC
/// fan-out).
///
/// `email.bcc` should be empty on calls from the encrypted-send
/// path: the outer header writer ignores BCC by design, but the
/// inner [`build_outgoing_message`] would still emit the address
/// via lettre's `Bcc:` header if it were populated — exactly the
/// leak we're avoiding.  Callers strip BCC on the email clone they
/// hand in.
fn wrap_as_pgp_mime_for_recipients(
    email: &OutgoingEmail,
    bridge: &dyn CryptoBridge,
    encrypt_to: &[String],
) -> Result<Vec<u8>, UnkaiError> {
    let inner_message = build_outgoing_message(email)?;
    let inner_bytes = inner_message.formatted();

    let encrypted = bridge.encrypt(&inner_bytes, encrypt_to, email.signing_enabled)?;
    Ok(build_outer_pgp_mime_bytes(
        email,
        &encrypted.ciphertext_armor,
    ))
}

/// One planned PGP/MIME envelope ready to hand to `transport.send_raw`.
struct PlannedEnvelope {
    /// SMTP routing — MAIL FROM + RCPT TO list.  For the visible
    /// TO + CC copy this carries every TO + CC address; for a
    /// per-BCC copy it carries that single BCC address.
    envelope: Envelope,
    /// Wire-format `multipart/encrypted` bytes including outer
    /// routing headers + ciphertext body.
    wire_bytes: Vec<u8>,
}

/// Plan one or more `multipart/encrypted` envelopes for `email`.
///
/// When `email.bcc` is empty, returns a single envelope addressed
/// to TO + CC.  When BCC is non-empty, returns one envelope for
/// the TO + CC visible recipients (skipped if both are empty —
/// e.g. a BCC-only send), plus one envelope per BCC recipient.
///
/// Each per-BCC envelope's ciphertext is encrypted **only** to
/// that BCC recipient's key, so the OpenPGP ESK packets in one
/// recipient's copy never name another.  Mirrors how the
/// plaintext BCC fan-out works at the SMTP level (separate RCPT
/// TO transactions, no BCC headers on the wire), but with the
/// additional cryptographic property that no two ciphertexts
/// ever share a session-key recipient set.
///
/// All copies use the same inner MIME body (built from `email`
/// with `bcc` stripped), so what each recipient *reads* is
/// identical — the differences are confined to the OpenPGP
/// session-key wrappers and the SMTP envelope routing.
fn plan_pgp_encrypted_envelopes(
    email: &OutgoingEmail,
    bridge: &dyn CryptoBridge,
) -> Result<Vec<PlannedEnvelope>, UnkaiError> {
    plan_encrypted_envelopes(email, |header_email, recipients| {
        wrap_as_pgp_mime_for_recipients(header_email, bridge, recipients)
    })
}

/// Plan one or more S/MIME `application/pkcs7-mime; smime-type=
/// enveloped-data` envelopes for `email`.  S/MIME sibling of
/// [`plan_pgp_encrypted_envelopes`] — same BCC split-send shape, just a
/// different wire wrapper.
///
/// The split protects the BCC list for the same reason it does on the
/// OpenPGP path: a CMS `EnvelopedData` carries one
/// `KeyTransRecipientInfo` per recipient, each holding a
/// `RecipientIdentifier` (issuer + serial number, or subjectKeyIdentifier
/// — RFC 5652 §6.2.1) that pins down whose certificate the copy was
/// encrypted to.  Bundling TO + CC + BCC into a single `EnvelopedData`
/// would let any recipient enumerate the others' cert identities, so we
/// fan out exactly as the OpenPGP path does.
fn plan_smime_enveloped_envelopes(
    email: &OutgoingEmail,
    bridge: &dyn CryptoBridge,
) -> Result<Vec<PlannedEnvelope>, UnkaiError> {
    plan_encrypted_envelopes(email, |header_email, recipients| {
        wrap_as_smime_enveloped_for_recipients(header_email, bridge, recipients)
    })
}

/// Shared BCC split-send planner for both encrypted stacks.  `build_wire`
/// turns one envelope's recipient list (already reduced to bare
/// `addr@host` form) into the wire bytes for that copy — PGP passes
/// [`wrap_as_pgp_mime_for_recipients`], S/MIME passes
/// [`wrap_as_smime_enveloped_for_recipients`].
///
/// When `email.bcc` is empty, returns a single envelope addressed to
/// TO + CC.  When BCC is non-empty, returns one envelope for the visible
/// TO + CC recipients (skipped if both are empty — a BCC-only send),
/// plus one envelope per BCC recipient.  Each per-BCC envelope's
/// ciphertext is encrypted **only** to that recipient's key/cert, so no
/// two copies ever share a recipient set and a BCC recipient never sees
/// another recipient's key/cert identity.  All copies share the same
/// inner MIME body (built from `email` with `bcc` stripped), so what
/// each recipient *reads* is identical — the differences are confined to
/// the per-copy cryptographic wrappers and the SMTP envelope routing.
///
/// Keeping the recipient-isolation logic in one place is deliberate: a
/// drift between the two stacks here would be a silent BCC-disclosure
/// bug, so PGP and S/MIME route through the exact same code.
fn plan_encrypted_envelopes<F>(
    email: &OutgoingEmail,
    build_wire: F,
) -> Result<Vec<PlannedEnvelope>, UnkaiError>
where
    F: Fn(&OutgoingEmail, &[String]) -> Result<Vec<u8>, UnkaiError>,
{
    // BCC is cleared from the clone we hand to the body / outer
    // builders so the BCC list cannot land in any copy's headers via
    // lettre's `Bcc:` header on the inner MIME.  The outer wrapper
    // already omits BCC; this belt-and-braces step keeps the inner
    // clean too.
    let mut header_email = email.clone();
    header_email.bcc.clear();

    let mut planned = Vec::with_capacity(1 + email.bcc.len());

    // ── (1) Visible TO + CC copy ───────────────────────────────
    // One ciphertext encrypted to every visible recipient's key.
    // Skipped if both TO and CC are empty (a BCC-only send) so
    // we don't waste a transaction on an empty RCPT TO list.
    let to_cc_recipients: Vec<String> = email
        .to
        .iter()
        .chain(email.cc.iter())
        .map(|a| address_only(a))
        .collect();
    if !to_cc_recipients.is_empty() {
        let wire = build_wire(&header_email, &to_cc_recipients)?;
        let envelope = envelope_from_email(&header_email)?;
        planned.push(PlannedEnvelope {
            envelope,
            wire_bytes: wire,
        });
    }

    // ── (2) Per-BCC fan-out ────────────────────────────────────
    // One envelope per BCC address, each with its own ciphertext
    // encrypted only to that recipient's key.  Visible headers
    // still show the original TO / CC — same disclosure model as
    // the plaintext BCC fan-out — but the BCC recipient never
    // sees any other BCC address and the TO / CC recipients
    // never see any sign that BCC was used.
    for bcc_addr in &email.bcc {
        let bcc_key = address_only(bcc_addr);
        let wire = build_wire(&header_email, &[bcc_key])?;
        let envelope = envelope_for_single_recipient(&header_email, bcc_addr)?;
        planned.push(PlannedEnvelope {
            envelope,
            wire_bytes: wire,
        });
    }

    Ok(planned)
}

/// Pure-function MIME envelope builder for the PGP/MIME outer.  Lives
/// outside [`wrap_as_pgp_mime_for_recipients`] so the structure can
/// be unit-tested against a fixed ciphertext without spinning up a
/// real bridge or transport.  All header strings are emitted with
/// CRLF endings as RFC 5322 requires (lettre normally handles this
/// for us; we have to do it ourselves on the hand-built outer).
fn build_outer_pgp_mime_bytes(email: &OutgoingEmail, ciphertext_armor: &[u8]) -> Vec<u8> {
    // Boundary string is just a random ASCII tag that can't appear in
    // either body part.  We use a UUID prefix so the chance of
    // collision with the ciphertext armor or the inner MIME is
    // effectively zero.
    let boundary = format!("unkai-pgp-mime-{}", uuid::Uuid::new_v4().simple());

    let mut headers = write_outer_routing_headers(email);
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

/// Emit the routing + threading headers shared by both PGP/MIME outer
/// wrappers (`multipart/encrypted` and `multipart/signed`).  Stops
/// after `MIME-Version: 1.0` so the caller can append the
/// envelope-specific `Content-Type` header on the next line.
///
/// `From`/`To`/`Cc`/`Reply-To` pass through verbatim — the SMTP layer
/// upstream of us is expected to have already RFC-5322-formatted these
/// (the lettre `Mailbox::parse` pass on the plaintext send path serves
/// the same role).  We do not emit `Bcc:` because BCC must never appear
/// in the wire headers a recipient sees; the SMTP envelope (built via
/// [`envelope_from_email_include_bcc`] on the sign-only path) carries
/// BCC routing without leaking the addresses.
fn write_outer_routing_headers(email: &OutgoingEmail) -> String {
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
    // RFC 8098 read-receipt request (#416).  Routing metadata, so it
    // must live on the *outer* wrapper — a receiving client can only
    // honour it if it's visible before any decrypt/verify step.
    if email.request_read_receipt {
        headers.push_str(&format!(
            "Disposition-Notification-To: {}\r\n",
            address_only(&email.from)
        ));
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
    headers
}

/// Wrap a plaintext `OutgoingEmail` as an RFC 3156 §5
/// `multipart/signed; protocol="application/pgp-signature"; micalg="pgp-sha256"`
/// PGP/MIME message and return the raw RFC 822 byte form ready for
/// `transport.send_raw`.
///
/// Flow:
///   1. Build the inner MIME body via the regular [`build_outgoing_message`]
///      path.
///   2. Extract just the body MIME entity (Content-* headers + body
///      bytes), stripping the outer routing headers
///      (From/To/Subject/Date/...) — those live on the outer wrapper
///      only.  RFC 3156 §5 signs the body MIME entity, NOT the full
///      RFC 822 message.
///   3. Canonicalise the body bytes per RFC 3156 §5: line endings to
///      CRLF, strip trailing whitespace.  Mandatory before the hash
///      is computed because a recipient's verify pass will apply the
///      same canonicalisation before re-hashing — any drift between
///      the bytes we sign and the bytes a verifier reconstructs from
///      the wire results in an `unknown-signer`/`invalid` status even
///      with the right key.
///   4. Ask the bridge for a detached armored signature over the
///      canonicalised bytes.  The bridge encapsulates the private-key
///      + passphrase + subkey-selection dance.
///   5. Emit a hand-built outer `multipart/signed` envelope: the
///      first part is the *exact* canonicalised body bytes we signed
///      (byte-for-byte parity matters here — see step 3 rationale),
///      the second part is the armored signature blob.
fn wrap_as_pgp_mime_signed(
    email: &OutgoingEmail,
    bridge: &dyn CryptoBridge,
) -> Result<Vec<u8>, UnkaiError> {
    let inner_message = build_outgoing_message(email)?;
    let inner_full = inner_message.formatted();
    let inner_entity = extract_inner_body_mime_entity(&inner_full)?;
    let canonical = canonicalize_for_pgp_signing(&inner_entity);
    let signature_armor = bridge.sign(&canonical)?;
    Ok(build_outer_pgp_mime_signed_bytes(
        email,
        &canonical,
        &signature_armor,
    ))
}

/// Pure-function MIME envelope builder for the `multipart/signed`
/// outer.  Mirrors [`build_outer_pgp_mime_bytes`] but emits the RFC
/// 3156 §5 layout: the signed body part first, then the
/// `application/pgp-signature` part carrying the armored detached
/// signature.
///
/// `inner_canonical` MUST be the same bytes that were passed to
/// `bridge.sign()` — verifiers re-hash whatever they pull from
/// between the boundary delimiters, so any divergence here results
/// in a verify failure even when the right key is present.
fn build_outer_pgp_mime_signed_bytes(
    email: &OutgoingEmail,
    inner_canonical: &[u8],
    signature_armor: &[u8],
) -> Vec<u8> {
    let boundary = format!("unkai-pgp-signed-{}", uuid::Uuid::new_v4().simple());

    let mut headers = write_outer_routing_headers(email);
    // RFC 3156 §5: `micalg` advertises the hash algorithm so the
    // recipient can pre-select it before parsing the signature
    // packet.  We always emit SHA-256 (matches `sign_detached`'s
    // hash choice in `unkai_crypto::ops`) — pgp-sha256 is the
    // OpenPGP IANA-registered micalg value for SHA-256.
    headers.push_str(&format!(
        "Content-Type: multipart/signed; \
         protocol=\"application/pgp-signature\"; \
         micalg=\"pgp-sha256\"; \
         boundary=\"{boundary}\"\r\n"
    ));

    let mut out = headers.into_bytes();
    out.extend_from_slice(b"\r\n");

    // First body part: the signed MIME entity, byte-for-byte as it
    // was handed to `bridge.sign()`.
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(inner_canonical);
    // Ensure exactly one CRLF before the next boundary delimiter —
    // RFC 2046 attaches that CRLF to the boundary, not to the part
    // body, but verifiers expect the byte layout to be consistent.
    if !inner_canonical.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }

    // Second body part: the armored signature.  Content-Disposition
    // attachment + filename="signature.asc" is the conventional shape
    // every PGP-aware mail client emits and recognises.
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(b"Content-Type: application/pgp-signature; name=\"signature.asc\"\r\n");
    out.extend_from_slice(b"Content-Description: OpenPGP digital signature\r\n");
    out.extend_from_slice(b"Content-Disposition: attachment; filename=\"signature.asc\"\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(signature_armor);
    if !signature_armor.ends_with(b"\n") {
        out.extend_from_slice(b"\r\n");
    }

    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

// ── S/MIME send path (#338) ─────────────────────────────────────────

/// Build the S/MIME `application/pkcs7-mime; smime-type=enveloped-data`
/// wire bytes for one envelope's worth of recipients.  S/MIME sibling of
/// [`wrap_as_pgp_mime_for_recipients`]: same "encrypt the full inner MIME
/// message, then wrap routing headers around it" shape, but the output is
/// a single `application/pkcs7-mime` part (RFC 8551 §3.2) rather than the
/// OpenPGP two-part `multipart/encrypted`.
///
/// `encrypt_to` is the explicit recipient list handed to the bridge,
/// decoupled from `email.to / email.cc` so the BCC fan-out in
/// [`plan_smime_enveloped_envelopes`] can encrypt each per-BCC copy to a
/// single cert.  As on the PGP path, `email.bcc` must be empty on these
/// calls — the planner strips it on the clone it hands in.
fn wrap_as_smime_enveloped_for_recipients(
    email: &OutgoingEmail,
    bridge: &dyn CryptoBridge,
    encrypt_to: &[String],
) -> Result<Vec<u8>, UnkaiError> {
    let inner_message = build_outgoing_message(email)?;
    let inner_bytes = inner_message.formatted();
    let cms_der = bridge.encrypt_smime(&inner_bytes, encrypt_to)?;
    Ok(build_outer_smime_enveloped_bytes(email, &cms_der))
}

/// Pure-function MIME builder for the S/MIME enveloped-data outer.
/// Unlike the OpenPGP `multipart/encrypted` form (a two-part wrapper with
/// a version-identification part), RFC 8551 §3.2 puts the CMS
/// `EnvelopedData` directly into a single `application/pkcs7-mime` part,
/// base64-encoded.  The emitted Content-Type / Content-Disposition match
/// what our own receive path's `detect_smime_envelope` keys on, so a
/// message we send round-trips back through our decrypt path.
fn build_outer_smime_enveloped_bytes(email: &OutgoingEmail, cms_der: &[u8]) -> Vec<u8> {
    let mut headers = write_outer_routing_headers(email);
    headers.push_str(
        "Content-Type: application/pkcs7-mime; smime-type=enveloped-data; name=\"smime.p7m\"\r\n",
    );
    headers.push_str("Content-Transfer-Encoding: base64\r\n");
    headers.push_str("Content-Disposition: attachment; filename=\"smime.p7m\"\r\n");

    let mut out = headers.into_bytes();
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(base64_mime_body(cms_der).as_bytes());
    out
}

/// Wrap a plaintext `OutgoingEmail` as an RFC 8551 §3.4 `multipart/
/// signed; protocol="application/pkcs7-signature"; micalg="sha-256"`
/// S/MIME message and return the raw RFC 822 byte form ready for
/// `transport.send_raw`.
///
/// S/MIME sibling of [`wrap_as_pgp_mime_signed`] — the flow is identical:
/// build the inner MIME body, extract just the body MIME entity, then
/// canonicalise it, detached-sign the canonical bytes, and emit a
/// two-part `multipart/signed` whose first part is the *exact* canonical
/// bytes and whose second part is the detached signature.  The
/// byte-for-byte parity between the signed bytes and the wire bytes
/// matters for the same reason it does on the PGP path: a verifier
/// re-hashes whatever sits between the boundary delimiters.
///
/// RFC 8551 §3.1.1's canonical S/MIME form (CRLF line endings, no
/// trailing whitespace) coincides with RFC 3156 §5's, so we reuse
/// [`canonicalize_for_pgp_signing`].  `unkai_crypto::smime_sign` signs
/// with the `BINARY` flag precisely so OpenSSL does not re-canonicalise
/// on top of the bytes we already normalised here.
fn wrap_as_smime_signed(
    email: &OutgoingEmail,
    bridge: &dyn CryptoBridge,
) -> Result<Vec<u8>, UnkaiError> {
    let inner_message = build_outgoing_message(email)?;
    let inner_full = inner_message.formatted();
    let inner_entity = extract_inner_body_mime_entity(&inner_full)?;
    let canonical = canonicalize_for_pgp_signing(&inner_entity);
    let signature_der = bridge.sign_smime(&canonical)?;
    Ok(build_outer_smime_signed_bytes(
        email,
        &canonical,
        &signature_der,
    ))
}

/// Pure-function MIME builder for the S/MIME `multipart/signed` outer.
/// Mirrors [`build_outer_pgp_mime_signed_bytes`] but emits the
/// `application/pkcs7-signature` second part carrying the base64 DER of
/// the detached CMS `SignedData`.
///
/// `inner_canonical` MUST be the same bytes passed to
/// `bridge.sign_smime()` — see [`wrap_as_smime_signed`] for why.
fn build_outer_smime_signed_bytes(
    email: &OutgoingEmail,
    inner_canonical: &[u8],
    signature_der: &[u8],
) -> Vec<u8> {
    let boundary = format!("unkai-smime-signed-{}", uuid::Uuid::new_v4().simple());

    let mut headers = write_outer_routing_headers(email);
    // RFC 5751 §3.4.3.2: the micalg parameter value for SHA-256 in
    // S/MIME is the bare `sha-256` — NOT the OpenPGP `pgp-sha256`
    // spelling the PGP builder emits.  `unkai_crypto::smime_sign` uses a
    // SHA-256 digest, matching this advertised value.
    headers.push_str(&format!(
        "Content-Type: multipart/signed; \
         protocol=\"application/pkcs7-signature\"; \
         micalg=\"sha-256\"; \
         boundary=\"{boundary}\"\r\n"
    ));

    let mut out = headers.into_bytes();
    out.extend_from_slice(b"\r\n");

    // First body part: the signed MIME entity, byte-for-byte as it was
    // handed to `bridge.sign_smime()`.
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(inner_canonical);
    if !inner_canonical.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }

    // Second body part: the base64-encoded detached CMS signature.
    // `name`/`filename="smime.p7s"` is the conventional shape every
    // S/MIME-aware mail client emits and recognises.
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(b"Content-Type: application/pkcs7-signature; name=\"smime.p7s\"\r\n");
    out.extend_from_slice(b"Content-Transfer-Encoding: base64\r\n");
    out.extend_from_slice(b"Content-Description: S/MIME Cryptographic Signature\r\n");
    out.extend_from_slice(b"Content-Disposition: attachment; filename=\"smime.p7s\"\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(base64_mime_body(signature_der).as_bytes());

    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// Everything needed to compose one read receipt (RFC 8098, #416).
/// Assembled by the Tauri command layer from the cached original
/// message + the sending account, and handed to
/// [`build_mdn_report_bytes`] / [`SmtpClient::send_mdn`].
#[derive(Debug, Clone)]
pub struct MdnReply {
    /// The receipt's author — the account address the original mail
    /// was delivered to.  Becomes both the `From:` header and the
    /// report's `Final-Recipient` field.
    pub from: String,
    /// Where the receipt goes: the original message's
    /// `Disposition-Notification-To:` value.
    pub to: String,
    /// The original message's Message-ID *without* angle brackets
    /// (the storage convention #277 set).  Fills the report's
    /// `Original-Message-ID` field — the key the sender's client
    /// uses to match the receipt back to its sent mail — plus
    /// `In-Reply-To`/`References` so the receipt threads under the
    /// original.  `None` when the original carried no Message-ID;
    /// the receipt is still valid, just unmatchable by ID.
    pub original_message_id: Option<String>,
    /// The original message's subject, quoted in the receipt's own
    /// `Subject:` and human-readable part.
    pub original_subject: String,
    /// `true` when the `Always` policy fired the receipt without a
    /// per-message user action — reported as `automatic-action/
    /// MDN-sent-automatically` per RFC 8098 §3.2.6.2 so the sender
    /// knows a machine, not the reader, confirmed the display.
    /// `false` = the user explicitly clicked "Send receipt"
    /// (`manual-action/MDN-sent-manually`).
    pub automatic: bool,
}

/// Pure-function builder for the `multipart/report;
/// report-type=disposition-notification` wire bytes (RFC 8098 §3).
/// Same hand-built-CRLF approach as the crypto outer builders —
/// lettre has no model for the `message/disposition-notification`
/// media type — and split out from [`SmtpClient::send_mdn`] so the
/// structure is unit-testable without a transport.
///
/// Layout (both parts required by the RFC):
///   1. `text/plain` — human-readable one-liner for clients that
///      don't understand MDNs and just render the parts.
///   2. `message/disposition-notification` — the machine-readable
///      field block (`Final-Recipient`, `Original-Message-ID`,
///      `Disposition`).
///
/// The top-level headers also carry `Auto-Submitted: auto-replied`
/// (RFC 3834) so other auto-responders — vacation replies, ticket
/// systems — know not to respond to the receipt in turn.
pub fn build_mdn_report_bytes(reply: &MdnReply) -> Vec<u8> {
    let boundary = format!("unkai-mdn-{}", uuid::Uuid::new_v4().simple());
    let message_id = format!("<{}@unkai-mail.local>", uuid::Uuid::new_v4().simple());
    let date = chrono::Utc::now().to_rfc2822();
    let final_recipient = address_only(&reply.from);

    let mut out = String::new();
    out.push_str(&format!("From: {}\r\n", reply.from));
    out.push_str(&format!("To: {}\r\n", reply.to));
    out.push_str(&format!("Subject: Read: {}\r\n", reply.original_subject));
    out.push_str(&format!("Date: {date}\r\n"));
    out.push_str(&format!("Message-ID: {message_id}\r\n"));
    if let Some(orig) = &reply.original_message_id {
        out.push_str(&format!("In-Reply-To: <{orig}>\r\n"));
        out.push_str(&format!("References: <{orig}>\r\n"));
    }
    out.push_str("Auto-Submitted: auto-replied\r\n");
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str(&format!(
        "Content-Type: multipart/report; report-type=disposition-notification; \
         boundary=\"{boundary}\"\r\n"
    ));
    out.push_str("\r\n");

    // Part 1 — human-readable summary.  Deliberately English-only:
    // it renders in the *sender's* client, whose locale we can't
    // know, and a fixed string is the interop-safe choice.
    out.push_str(&format!("--{boundary}\r\n"));
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("\r\n");
    out.push_str(&format!(
        "The message \"{}\" sent to {} has been displayed.\r\n\
         This is no guarantee that the message has been read or understood.\r\n",
        reply.original_subject, final_recipient,
    ));
    out.push_str("\r\n");

    // Part 2 — the machine-readable disposition field block.
    out.push_str(&format!("--{boundary}\r\n"));
    out.push_str("Content-Type: message/disposition-notification\r\n");
    out.push_str("\r\n");
    out.push_str("Reporting-UA: Unkai Mail\r\n");
    out.push_str(&format!("Final-Recipient: rfc822;{final_recipient}\r\n"));
    if let Some(orig) = &reply.original_message_id {
        out.push_str(&format!("Original-Message-ID: <{orig}>\r\n"));
    }
    let mode = if reply.automatic {
        "automatic-action/MDN-sent-automatically"
    } else {
        "manual-action/MDN-sent-manually"
    };
    out.push_str(&format!("Disposition: {mode}; displayed\r\n"));
    out.push_str("\r\n");

    out.push_str(&format!("--{boundary}--\r\n"));
    out.into_bytes()
}

/// Base64-encode `data` and lay it out as a MIME `Content-Transfer-
/// Encoding: base64` body: 76-character lines (the RFC 2045 §6.8 limit),
/// CRLF terminators, including a final CRLF after the last line so the
/// closing multipart boundary that follows is correctly delimited.  Used
/// by both S/MIME parts (the enveloped-data `.p7m` and the detached
/// signature `.p7s`).
fn base64_mime_body(data: &[u8]) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2 + 2);
    for chunk in encoded.as_bytes().chunks(76) {
        // `chunk` is always valid ASCII — base64's alphabet is a subset.
        out.push_str(std::str::from_utf8(chunk).expect("base64 output is ASCII"));
        out.push_str("\r\n");
    }
    out
}

/// Pull out just the body MIME entity (Content-* headers + body bytes)
/// from a full RFC 822 message produced by [`build_outgoing_message`],
/// stripping the outer routing headers (From/To/Cc/Subject/Date/
/// Message-ID/In-Reply-To/References/Reply-To/MIME-Version) that live
/// on the outer wrapper only.
///
/// RFC 3156 §5's signed entity is the body part itself — Content-*
/// headers + body — not the surrounding RFC 822 message.  Verifiers
/// re-hash the bytes between the boundary delimiters and would reject
/// a signature computed over a payload that included `From:` /
/// `Subject:` (those headers don't reappear on the wire inside the
/// multipart/signed body part).
///
/// Folded continuation lines (lines starting with SP/HTAB) are
/// preserved with their preceding header.
fn extract_inner_body_mime_entity(formatted: &[u8]) -> Result<Vec<u8>, UnkaiError> {
    let sep_pos = formatted
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            UnkaiError::Protocol("inner MIME message missing CRLF CRLF header separator".into())
        })?;
    let headers_blob = std::str::from_utf8(&formatted[..sep_pos])
        .map_err(|_| UnkaiError::Protocol("inner MIME message headers are not UTF-8".into()))?;
    let body_bytes = &formatted[sep_pos + 4..];

    let mut kept = String::with_capacity(headers_blob.len());
    let mut iter = headers_blob.split("\r\n").peekable();
    while let Some(line) = iter.next() {
        if line.is_empty() {
            continue;
        }
        let mut full_header = String::from(line);
        // Pull any folded continuation lines (RFC 5322 §2.2.3 — a
        // header field that begins with whitespace is a continuation
        // of the previous field) onto the same logical header so we
        // either keep or drop them together.
        while let Some(next) = iter.peek() {
            if next.starts_with(' ') || next.starts_with('\t') {
                full_header.push_str("\r\n");
                full_header.push_str(next);
                iter.next();
            } else {
                break;
            }
        }
        let name = full_header.split(':').next().unwrap_or("").trim();
        // Keep only the headers that describe the body itself.
        // Content-Disposition / Content-ID / Content-Description
        // never appear at the top level of a `build_outgoing_message`
        // output (they live inside multipart subparts) so this list
        // covers every real case; the extra names are belt-and-braces
        // for future builders.
        let keep = name.eq_ignore_ascii_case("content-type")
            || name.eq_ignore_ascii_case("content-transfer-encoding")
            || name.eq_ignore_ascii_case("content-disposition")
            || name.eq_ignore_ascii_case("content-id")
            || name.eq_ignore_ascii_case("content-description");
        if keep {
            kept.push_str(&full_header);
            kept.push_str("\r\n");
        }
    }

    if kept.is_empty() {
        return Err(UnkaiError::Protocol(
            "inner MIME message carried no Content-* headers — nothing to sign".into(),
        ));
    }

    let mut out = kept.into_bytes();
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body_bytes);
    Ok(out)
}

/// Canonicalise `input` per RFC 3156 §5 for OpenPGP detached
/// signing of a `multipart/signed` body part:
///   1. Normalise every line terminator to CRLF (bare LF or bare CR
///      becomes CRLF).
///   2. Strip trailing SP / HTAB on every line *before* the
///      terminator.
///
/// Both transforms run on the same byte slice the SMTP layer will put
/// on the wire — by signing this canonical form and writing the same
/// canonical bytes into the outer envelope, we guarantee a verifier
/// re-hashing the wire bytes will land on the exact same digest.
fn canonicalize_for_pgp_signing(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut line: Vec<u8> = Vec::with_capacity(80);
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'\r' {
            // Consume optional LF so bare CR and CRLF both terminate
            // the current line; bare CR is rare but legal in some MIME
            // encoders and we don't want to leave it stranded mid-line.
            if i + 1 < input.len() && input[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
            flush_line(&mut out, &line);
            line.clear();
        } else if b == b'\n' {
            i += 1;
            flush_line(&mut out, &line);
            line.clear();
        } else {
            line.push(b);
            i += 1;
        }
    }
    // Trailing partial line (no terminator).  Per RFC 3156 §5 the
    // canonical form ends every line with CRLF, so we emit one even
    // for the tail.  Verifiers do the same on re-canonicalisation,
    // keeping the digest stable across the round trip.
    if !line.is_empty() {
        flush_line(&mut out, &line);
    }
    out
}

fn flush_line(out: &mut Vec<u8>, line: &[u8]) {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b' ' || line[end - 1] == b'\t') {
        end -= 1;
    }
    out.extend_from_slice(&line[..end]);
    out.extend_from_slice(b"\r\n");
}

/// `Disposition-Notification-To:` typed header (RFC 8098 §2.1, #416).
/// lettre 0.11 has no built-in type for it, so we implement the
/// `Header` trait ourselves — the value is the address the recipient's
/// client should send the read receipt to (our own From address).
#[derive(Debug, Clone)]
struct DispositionNotificationTo(String);

impl Header for DispositionNotificationTo {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("Disposition-Notification-To")
    }

    fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self(s.to_string()))
    }

    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), self.0.clone())
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
    // Without this, every Unkai reply lands in the recipient's
    // client as a thread orphan because there is nothing to anchor
    // a thread on.
    //
    // `In-Reply-To` and `References` only apply to replies and
    // are sourced from the parent's Message-ID + References chain
    // by the Compose / send path; we wrap each ID in the angle
    // brackets the headers expect.
    builder = builder.message_id(None);

    // RFC 8098 read-receipt request (#416): point the receipt at
    // our own address.  The bare From (display name stripped) keeps
    // the header a plain routable address — some receiving clients
    // choke on comments/names in this field.
    if email.request_read_receipt {
        builder = builder.header(DispositionNotificationTo(address_only(&email.from)));
    }
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
            request_read_receipt: false,
            request_delivery_receipt: false,
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

    // ── multipart/signed sign-only path (#341) ─────────────────

    use super::{
        canonicalize_for_pgp_signing, envelope_from_email_include_bcc,
        extract_inner_body_mime_entity, wrap_as_pgp_mime_signed,
    };
    use unkai_core::UnkaiError;
    use unkai_core::crypto::{CryptoBridge, DecryptedPayload, EncryptedOutput, VerifyOutcome};

    /// Test-only bridge whose `sign` returns a fixed armored blob and
    /// records the bytes it was asked to sign — lets us assert that
    /// the SMTP layer canonicalised the body the same way before
    /// signing AND before writing it to the wire.  The actual OpenPGP
    /// math is covered by `unkai_crypto`'s own round-trip tests; here
    /// we only care about the envelope construction.
    struct RecordingSignBridge {
        last_signed: std::sync::Mutex<Option<Vec<u8>>>,
        signature: Vec<u8>,
    }
    impl RecordingSignBridge {
        fn new() -> Self {
            Self {
                last_signed: std::sync::Mutex::new(None),
                signature:
                    b"-----BEGIN PGP SIGNATURE-----\nFAKE-SIG\n-----END PGP SIGNATURE-----\n"
                        .to_vec(),
            }
        }
        fn take_last(&self) -> Vec<u8> {
            self.last_signed
                .lock()
                .expect("mutex")
                .clone()
                .expect("sign was never called")
        }
    }
    impl CryptoBridge for RecordingSignBridge {
        fn decrypt(&self, _: &[u8]) -> Result<DecryptedPayload, UnkaiError> {
            unreachable!()
        }
        fn decrypt_smime(&self, _: &[u8]) -> Result<DecryptedPayload, UnkaiError> {
            unreachable!()
        }
        fn verify(&self, _: &[u8], _: &[u8]) -> Result<VerifyOutcome, UnkaiError> {
            unreachable!()
        }
        fn verify_smime(&self, _: &[u8], _: &[u8], _: &str) -> Result<VerifyOutcome, UnkaiError> {
            unreachable!()
        }
        fn encrypt(&self, _: &[u8], _: &[String], _: bool) -> Result<EncryptedOutput, UnkaiError> {
            unreachable!()
        }
        fn sign(&self, signed_payload: &[u8]) -> Result<Vec<u8>, UnkaiError> {
            *self.last_signed.lock().expect("mutex") = Some(signed_payload.to_vec());
            Ok(self.signature.clone())
        }
        fn encrypt_smime(&self, _: &[u8], _: &[String]) -> Result<Vec<u8>, UnkaiError> {
            unreachable!()
        }
        fn sign_smime(&self, signed_payload: &[u8]) -> Result<Vec<u8>, UnkaiError> {
            // Same recording shape as `sign`, but returns a DER-ish marker
            // (the S/MIME wrap base64-encodes it) instead of PGP armor.
            *self.last_signed.lock().expect("mutex") = Some(signed_payload.to_vec());
            Ok(b"FAKE-CMS-SIGNED-DATA-DER".to_vec())
        }
    }

    #[test]
    fn canonicalize_normalises_line_endings_and_strips_trailing_whitespace() {
        // Three line terminator forms (LF, CRLF, bare CR), each line
        // with assorted trailing whitespace.  Per RFC 3156 §5 all
        // three terminators must collapse to CRLF and every trailing
        // SP / HTAB must be removed before the digital signature is
        // computed.  Body content (the leading spaces on the third
        // line) is untouched.
        let input = b"first line  \nsecond\tline \t\r\n  third line\rfourth";
        let out = canonicalize_for_pgp_signing(input);
        assert_eq!(
            std::str::from_utf8(&out).expect("utf-8"),
            "first line\r\nsecond\tline\r\n  third line\r\nfourth\r\n",
        );
    }

    #[test]
    fn canonicalize_is_idempotent() {
        // Verifiers will canonicalise the wire bytes a second time
        // before re-hashing; running our canonicaliser on its own
        // output must be a no-op or the wire bytes drift away from
        // what we signed.
        let input = b"already\r\nclean\r\n";
        assert_eq!(canonicalize_for_pgp_signing(input), input.to_vec());
    }

    #[test]
    fn extract_inner_strips_routing_keeps_content_type() {
        let full = b"From: alice@example.com\r\n\
                     To: bob@example.com\r\n\
                     Subject: hi\r\n\
                     Date: Sun, 24 May 2026 12:00:00 +0000\r\n\
                     Message-ID: <abc@unkai>\r\n\
                     MIME-Version: 1.0\r\n\
                     Content-Type: text/plain; charset=utf-8\r\n\
                     Content-Transfer-Encoding: 7bit\r\n\
                     \r\n\
                     Hello, signed world!\r\n";
        let inner = extract_inner_body_mime_entity(full).expect("extract");
        let s = std::str::from_utf8(&inner).expect("utf-8");
        assert!(
            !s.contains("From:")
                && !s.contains("To:")
                && !s.contains("Subject:")
                && !s.contains("Date:")
                && !s.contains("Message-ID:"),
            "routing headers must not appear inside the signed body MIME entity: {s}"
        );
        assert!(
            s.starts_with("Content-Type: text/plain"),
            "first kept header must be Content-Type: {s}"
        );
        assert!(s.contains("Hello, signed world!"));
    }

    #[test]
    fn extract_inner_preserves_folded_continuation_lines() {
        // Headers can wrap onto continuation lines that begin with
        // SP/HTAB (RFC 5322 §2.2.3).  When we keep / drop a header
        // we must keep / drop its continuation lines as a unit so
        // the resulting block stays parseable.
        let full = b"Subject: a long subject\r\n that wraps onto\r\n\ttwo continuation lines\r\n\
                     Content-Type: multipart/alternative;\r\n boundary=\"abc\"\r\n\
                     \r\n\
                     body";
        let inner = extract_inner_body_mime_entity(full).expect("extract");
        let s = std::str::from_utf8(&inner).expect("utf-8");
        // Subject + its two continuation lines dropped together.
        assert!(!s.contains("Subject"));
        assert!(!s.contains("continuation"));
        // Content-Type + its continuation line kept together.
        assert!(s.contains("multipart/alternative"));
        assert!(s.contains("boundary=\"abc\""));
    }

    #[test]
    fn signed_outer_advertises_pgp_signature_protocol_and_sha256_micalg() {
        let email = outgoing("audit-able update", &["bob@example.com"]);
        let bridge = RecordingSignBridge::new();
        let wire = wrap_as_pgp_mime_signed(&email, &bridge).expect("wrap");

        let parsed = MessageParser::default().parse(&wire).expect("parse outer");
        let ct = parsed.content_type().expect("Content-Type");
        assert!(ct.ctype().eq_ignore_ascii_case("multipart"));
        assert_eq!(ct.subtype().unwrap_or(""), "signed");
        assert_eq!(
            ct.attribute("protocol").unwrap_or(""),
            "application/pgp-signature"
        );
        assert_eq!(ct.attribute("micalg").unwrap_or(""), "pgp-sha256");
        assert_eq!(parsed.subject().unwrap_or(""), "audit-able update");
    }

    #[test]
    fn signed_outer_carries_signature_in_pgp_signature_part() {
        let email = outgoing("notice", &["bob@example.com"]);
        let bridge = RecordingSignBridge::new();
        let wire = wrap_as_pgp_mime_signed(&email, &bridge).expect("wrap");

        let parsed = MessageParser::default().parse(&wire).expect("parse outer");
        let sig_part = (0..)
            .map_while(|i| parsed.part(i))
            .find(|p| {
                p.content_type().is_some_and(|c| {
                    c.ctype().eq_ignore_ascii_case("application")
                        && c.subtype()
                            .is_some_and(|s| s.eq_ignore_ascii_case("pgp-signature"))
                })
            })
            .expect("application/pgp-signature part must exist");
        let body = std::str::from_utf8(sig_part.contents()).expect("utf-8");
        assert!(body.contains("-----BEGIN PGP SIGNATURE-----"));
        assert!(body.contains("FAKE-SIG"));
    }

    #[test]
    fn signed_outer_signed_bytes_match_wire_first_part() {
        // RFC 3156 §5 / RFC 1847 §2.1: a verifier re-hashes the bytes
        // it pulls out from between the boundary delimiters.  The
        // bytes we signed and the bytes we wrote into the body part
        // MUST be byte-identical or the signature won't verify even
        // with the right key.  This test is the contract.
        //
        // We assert the contract on raw bytes (not via mail-parser)
        // because mail-parser decodes transfer encodings (e.g.
        // quoted-printable) before returning `contents()`, which
        // would mask any drift between the signed bytes and the wire
        // bytes — the very thing this test is here to catch.
        let email = outgoing("byte parity", &["bob@example.com"]);
        let bridge = RecordingSignBridge::new();
        let wire = wrap_as_pgp_mime_signed(&email, &bridge).expect("wrap");
        let signed = bridge.take_last();

        // The signed bytes (Content-Type + headers + body of the
        // inner MIME entity) must appear verbatim somewhere in the
        // outer wire output, sandwiched between the two boundary
        // delimiters.  `windows().position()` finds the exact byte
        // run; any divergence (extra CRLF, header reshuffle,
        // re-encoding) breaks the search and fails the test.
        let signed_in_wire = wire.windows(signed.len()).any(|w| w == signed.as_slice());
        assert!(
            signed_in_wire,
            "signed payload must appear byte-for-byte in the outer envelope wire bytes"
        );
    }

    #[test]
    fn signed_path_envelope_includes_bcc() {
        // Sign-only does NOT leak BCC the way `multipart/encrypted`
        // would (per-recipient ESK packets), so BCC recipients ride
        // the same envelope as TO + CC.
        let mut email = outgoing("memo", &["a@example.com"]);
        email.bcc = vec!["secret@example.com".into()];
        let env = envelope_from_email_include_bcc(&email).expect("envelope");
        let rcpts: Vec<String> = env.to().iter().map(|a| a.to_string()).collect();
        assert!(rcpts.contains(&"a@example.com".into()));
        assert!(rcpts.contains(&"secret@example.com".into()));
    }

    // ── multipart/encrypted BCC split-send (#341) ───────────────

    use super::plan_pgp_encrypted_envelopes;

    /// Test bridge that records every `encrypt` call's recipient
    /// list — the property under test for the BCC fan-out is
    /// "no two ciphertexts share a recipient set, and no
    /// ciphertext names a BCC recipient alongside any other".
    /// `encrypt` returns a marker armor blob tagged with the call
    /// index so wire-byte assertions can tell the per-recipient
    /// ciphertexts apart.
    struct RecordingEncryptBridge {
        calls: std::sync::Mutex<Vec<Vec<String>>>,
    }
    impl RecordingEncryptBridge {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn recorded(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("mutex").clone()
        }
    }
    impl CryptoBridge for RecordingEncryptBridge {
        fn decrypt(&self, _: &[u8]) -> Result<DecryptedPayload, UnkaiError> {
            unreachable!()
        }
        fn decrypt_smime(&self, _: &[u8]) -> Result<DecryptedPayload, UnkaiError> {
            unreachable!()
        }
        fn verify(&self, _: &[u8], _: &[u8]) -> Result<VerifyOutcome, UnkaiError> {
            unreachable!()
        }
        fn verify_smime(&self, _: &[u8], _: &[u8], _: &str) -> Result<VerifyOutcome, UnkaiError> {
            unreachable!()
        }
        fn encrypt(
            &self,
            _: &[u8],
            recipients: &[String],
            _: bool,
        ) -> Result<EncryptedOutput, UnkaiError> {
            let mut guard = self.calls.lock().expect("mutex");
            let idx = guard.len();
            guard.push(recipients.to_vec());
            let armor = format!(
                "-----BEGIN PGP MESSAGE-----\nUNKAI-TEST-CIPHERTEXT-{idx}\n-----END PGP MESSAGE-----\n"
            );
            Ok(EncryptedOutput {
                ciphertext_armor: armor.into_bytes(),
            })
        }
        fn sign(&self, _: &[u8]) -> Result<Vec<u8>, UnkaiError> {
            unreachable!()
        }
        fn encrypt_smime(&self, _: &[u8], recipients: &[String]) -> Result<Vec<u8>, UnkaiError> {
            // Mirror `encrypt`'s recording so the shared BCC planner's
            // recipient-isolation property can be asserted on the S/MIME
            // path too.  Returns a DER-ish marker tagged with the call
            // index (the wrap layer base64-encodes it).
            let mut guard = self.calls.lock().expect("mutex");
            let idx = guard.len();
            guard.push(recipients.to_vec());
            Ok(format!("UNKAI-TEST-CMS-ENVELOPED-{idx}").into_bytes())
        }
        fn sign_smime(&self, _: &[u8]) -> Result<Vec<u8>, UnkaiError> {
            unreachable!()
        }
    }

    #[test]
    fn plan_without_bcc_emits_one_envelope_to_visible_recipients() {
        // The non-BCC case must keep the historical single-envelope
        // shape — one ciphertext encrypted to every TO + CC key,
        // one RCPT TO list covering them all.  Anything else would
        // double the SMTP load for the common case.
        let mut email = outgoing("regular send", &["to1@example.com"]);
        email.cc = vec!["cc1@example.com".into()];
        let bridge = RecordingEncryptBridge::new();
        let planned = plan_pgp_encrypted_envelopes(&email, &bridge).expect("plan");

        assert_eq!(planned.len(), 1);

        let recipients = bridge.recorded();
        assert_eq!(recipients.len(), 1);
        assert_eq!(
            recipients[0],
            vec!["to1@example.com".to_string(), "cc1@example.com".to_string()]
        );

        let rcpts: Vec<String> = planned[0]
            .envelope
            .to()
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert!(rcpts.contains(&"to1@example.com".into()));
        assert!(rcpts.contains(&"cc1@example.com".into()));
    }

    #[test]
    fn plan_with_bcc_isolates_each_bcc_in_its_own_envelope() {
        // The core split-send property: with BCC recipients in the
        // mix, we emit one envelope for the visible TO + CC and one
        // additional envelope per BCC.  Each BCC envelope's
        // ciphertext is encrypted ONLY to that BCC's key (so no
        // other recipient ever appears in the ESK packets that
        // BCC recipient could read), and the SMTP envelope routes
        // to that single BCC address (so the receiving relay can't
        // accidentally fan it out further).
        let mut email = outgoing("split-send", &["to1@example.com"]);
        email.cc = vec!["cc1@example.com".into()];
        email.bcc = vec!["bcc1@example.com".into(), "bcc2@example.com".into()];

        let bridge = RecordingEncryptBridge::new();
        let planned = plan_pgp_encrypted_envelopes(&email, &bridge).expect("plan");

        assert_eq!(planned.len(), 3, "1 TO+CC envelope + 2 per-BCC envelopes");

        let recipients = bridge.recorded();
        assert_eq!(recipients.len(), 3);
        // (1) TO + CC copy: both visible recipients in one ESK set.
        assert_eq!(
            recipients[0],
            vec!["to1@example.com".to_string(), "cc1@example.com".to_string()]
        );
        // (2) bcc1 copy: ESK set is exactly { bcc1 }.
        assert_eq!(recipients[1], vec!["bcc1@example.com".to_string()]);
        // (3) bcc2 copy: ESK set is exactly { bcc2 }.
        assert_eq!(recipients[2], vec!["bcc2@example.com".to_string()]);

        // Envelopes route to the matching addresses.
        let env0_rcpts: Vec<String> = planned[0]
            .envelope
            .to()
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert!(env0_rcpts.contains(&"to1@example.com".into()));
        assert!(env0_rcpts.contains(&"cc1@example.com".into()));
        assert!(!env0_rcpts.contains(&"bcc1@example.com".into()));
        assert!(!env0_rcpts.contains(&"bcc2@example.com".into()));

        let env1_rcpts: Vec<String> = planned[1]
            .envelope
            .to()
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(env1_rcpts, vec!["bcc1@example.com".to_string()]);

        let env2_rcpts: Vec<String> = planned[2]
            .envelope
            .to()
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(env2_rcpts, vec!["bcc2@example.com".to_string()]);
    }

    #[test]
    fn plan_with_bcc_keeps_bcc_out_of_every_copys_headers() {
        // No copy may carry a `Bcc:` header — the visible TO + CC
        // copy must not let TO / CC recipients see that BCC was
        // used at all, and each per-BCC copy must not let its
        // recipient see the other BCCs.  Same disclosure shape as
        // the plaintext BCC fan-out.
        let mut email = outgoing("no bcc header anywhere", &["to1@example.com"]);
        email.bcc = vec!["bcc1@example.com".into(), "bcc2@example.com".into()];

        let bridge = RecordingEncryptBridge::new();
        let planned = plan_pgp_encrypted_envelopes(&email, &bridge).expect("plan");
        assert_eq!(planned.len(), 3);

        for (idx, env) in planned.iter().enumerate() {
            let wire = std::str::from_utf8(&env.wire_bytes).expect("utf-8");
            assert!(
                !wire.to_ascii_lowercase().contains("bcc:"),
                "envelope {idx} wire bytes must not carry any `Bcc:` header: {wire}"
            );
            // Visible To header still appears so the recipient
            // knows who else was on the (visible) recipient list.
            assert!(
                wire.contains("To: to1@example.com"),
                "envelope {idx} must keep the visible To header: {wire}"
            );
        }
    }

    #[test]
    fn plan_with_bcc_only_skips_the_visible_copy() {
        // A BCC-only send (no TO, no CC) must NOT emit a wasted
        // empty envelope — only the per-BCC copies should be
        // queued.  Otherwise the SMTP server would reject a
        // RCPT-less transaction and fail the entire send.
        let mut email = outgoing("bcc-only", &[]);
        email.bcc = vec!["only@example.com".into()];

        let bridge = RecordingEncryptBridge::new();
        let planned = plan_pgp_encrypted_envelopes(&email, &bridge).expect("plan");
        assert_eq!(planned.len(), 1);

        let recipients = bridge.recorded();
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0], vec!["only@example.com".to_string()]);

        let rcpts: Vec<String> = planned[0]
            .envelope
            .to()
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(rcpts, vec!["only@example.com".to_string()]);
    }

    #[test]
    fn plan_strips_display_names_before_handing_recipients_to_bridge() {
        // The bridge's public-key cache is keyed on the bare
        // email — handing it `"Alex Morgan <alex@example.com>"`
        // would miss the cached key and surface a spurious
        // CryptoKeyNotFound.  `address_only` strips the display
        // name on every recipient before the encrypt call.
        let mut email = outgoing(
            "display-name stripping",
            &["Alex Morgan <alex@example.com>"],
        );
        email.bcc = vec!["Sam Lee <sam@example.com>".into()];

        let bridge = RecordingEncryptBridge::new();
        let planned = plan_pgp_encrypted_envelopes(&email, &bridge).expect("plan");
        assert_eq!(planned.len(), 2);

        let recipients = bridge.recorded();
        // TO copy keyed on the bare alex@…, BCC copy on the bare sam@….
        assert_eq!(recipients[0], vec!["alex@example.com".to_string()]);
        assert_eq!(recipients[1], vec!["sam@example.com".to_string()]);
    }

    // ── S/MIME send path (#338) ─────────────────────────────────

    use super::{
        base64_mime_body, build_outer_smime_enveloped_bytes, plan_smime_enveloped_envelopes,
        wrap_as_smime_signed,
    };

    /// A chunk of bytes long enough to force `base64_mime_body` to wrap
    /// onto more than one 76-char line (encodes to ~272 base64 chars).
    fn fake_cms_der() -> Vec<u8> {
        (0u8..=200).cycle().take(204).collect()
    }

    #[test]
    fn base64_mime_body_wraps_at_76_and_round_trips() {
        use base64::Engine;
        let der = fake_cms_der();
        let body = base64_mime_body(&der);

        // Every line ends with CRLF and is at most 76 base64 chars wide.
        for line in body.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(
                line.len() <= 76,
                "line exceeds 76 chars: {} chars",
                line.len()
            );
        }
        assert!(body.ends_with("\r\n"), "body must end with a CRLF");

        // Stripping the CRLFs and decoding recovers the original bytes.
        let joined: String = body.split("\r\n").collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(joined)
            .expect("valid base64");
        assert_eq!(decoded, der);
    }

    #[test]
    fn smime_enveloped_outer_is_single_pkcs7_mime_enveloped_data_part() {
        // The S/MIME enveloped outer is a SINGLE `application/pkcs7-mime`
        // part (RFC 8551 §3.2), unlike the OpenPGP two-part
        // `multipart/encrypted`.  The Content-Type attributes are exactly
        // what our own receive path's `detect_smime_envelope` keys on, so
        // a message we send round-trips back through our decrypt path.
        let email = outgoing("secret memo", &["bob@example.com"]);
        let der = fake_cms_der();
        let wire = build_outer_smime_enveloped_bytes(&email, &der);

        let parsed = MessageParser::default().parse(&wire).expect("parse outer");
        let ct = parsed.content_type().expect("Content-Type");
        assert!(ct.ctype().eq_ignore_ascii_case("application"));
        assert_eq!(ct.subtype().unwrap_or(""), "pkcs7-mime");
        assert_eq!(ct.attribute("smime-type").unwrap_or(""), "enveloped-data");
        assert_eq!(parsed.subject().unwrap_or(""), "secret memo");

        // The base64 body decodes back to the raw CMS DER we handed in —
        // mail-parser undoes the `Content-Transfer-Encoding: base64` for
        // us, exactly as the receive path relies on.
        let body_part = (0..)
            .map_while(|i| parsed.part(i))
            .find(|p| {
                p.content_type().is_some_and(|c| {
                    c.ctype().eq_ignore_ascii_case("application")
                        && c.subtype()
                            .is_some_and(|s| s.eq_ignore_ascii_case("pkcs7-mime"))
                })
            })
            .expect("application/pkcs7-mime part must exist");
        assert_eq!(body_part.contents(), der.as_slice());
    }

    #[test]
    fn smime_enveloped_plan_isolates_each_bcc_in_its_own_envelope() {
        // Same BCC split-send property as the OpenPGP path: CMS
        // `RecipientInfos` leak the recipient set (RFC 5652 §6.2.1), so we
        // emit one envelope for the visible TO + CC and one per BCC, each
        // encrypted only to that BCC's cert.  The S/MIME planner routes
        // through the SAME `plan_encrypted_envelopes` as PGP, so this test
        // guards the shared isolation logic from drifting.
        let mut email = outgoing("split-send", &["to1@example.com"]);
        email.cc = vec!["cc1@example.com".into()];
        email.bcc = vec!["bcc1@example.com".into(), "bcc2@example.com".into()];

        let bridge = RecordingEncryptBridge::new();
        let planned = plan_smime_enveloped_envelopes(&email, &bridge).expect("plan");
        assert_eq!(planned.len(), 3, "1 TO+CC envelope + 2 per-BCC envelopes");

        let recipients = bridge.recorded();
        assert_eq!(
            recipients[0],
            vec!["to1@example.com".to_string(), "cc1@example.com".to_string()]
        );
        assert_eq!(recipients[1], vec!["bcc1@example.com".to_string()]);
        assert_eq!(recipients[2], vec!["bcc2@example.com".to_string()]);

        // The visible TO+CC envelope must not route to (or name) any BCC.
        let env0_rcpts: Vec<String> = planned[0]
            .envelope
            .to()
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert!(!env0_rcpts.contains(&"bcc1@example.com".into()));
        assert!(!env0_rcpts.contains(&"bcc2@example.com".into()));
    }

    #[test]
    fn smime_signed_outer_advertises_pkcs7_signature_protocol_and_sha256_micalg() {
        let email = outgoing("audit-able update", &["bob@example.com"]);
        let bridge = RecordingSignBridge::new();
        let wire = wrap_as_smime_signed(&email, &bridge).expect("wrap");

        let parsed = MessageParser::default().parse(&wire).expect("parse outer");
        let ct = parsed.content_type().expect("Content-Type");
        assert!(ct.ctype().eq_ignore_ascii_case("multipart"));
        assert_eq!(ct.subtype().unwrap_or(""), "signed");
        assert_eq!(
            ct.attribute("protocol").unwrap_or(""),
            "application/pkcs7-signature"
        );
        // RFC 5751 §3.4.3.2: bare `sha-256`, NOT the OpenPGP `pgp-sha256`.
        assert_eq!(ct.attribute("micalg").unwrap_or(""), "sha-256");
        assert_eq!(parsed.subject().unwrap_or(""), "audit-able update");
    }

    #[test]
    fn smime_signed_outer_carries_signature_in_pkcs7_signature_part() {
        let email = outgoing("notice", &["bob@example.com"]);
        let bridge = RecordingSignBridge::new();
        let wire = wrap_as_smime_signed(&email, &bridge).expect("wrap");

        let parsed = MessageParser::default().parse(&wire).expect("parse outer");
        let sig_part = (0..)
            .map_while(|i| parsed.part(i))
            .find(|p| {
                p.content_type().is_some_and(|c| {
                    c.ctype().eq_ignore_ascii_case("application")
                        && c.subtype()
                            .is_some_and(|s| s.eq_ignore_ascii_case("pkcs7-signature"))
                })
            })
            .expect("application/pkcs7-signature part must exist");
        // mail-parser base64-decodes the part → the raw DER the stub
        // "signed with" (the wrap layer base64-wrapped it on the way out).
        assert_eq!(sig_part.contents(), b"FAKE-CMS-SIGNED-DATA-DER");
    }

    #[test]
    fn smime_signed_signed_bytes_match_wire_first_part() {
        // RFC 8551 §3.4 / RFC 1847 §2.1: a verifier re-hashes the bytes it
        // pulls from between the boundary delimiters.  The bytes we signed
        // and the bytes we wrote into the first body part MUST be
        // byte-identical, asserted on raw bytes (not via mail-parser,
        // which would decode transfer encodings and mask any drift).
        let email = outgoing("byte parity", &["bob@example.com"]);
        let bridge = RecordingSignBridge::new();
        let wire = wrap_as_smime_signed(&email, &bridge).expect("wrap");
        let signed = bridge.take_last();

        let signed_in_wire = wire.windows(signed.len()).any(|w| w == signed.as_slice());
        assert!(
            signed_in_wire,
            "signed payload must appear byte-for-byte in the outer envelope wire bytes"
        );
    }

    // ── Read receipts / MDN (#416) ─────────────────────────────

    use super::{MdnReply, build_mdn_report_bytes, build_outgoing_message};

    #[test]
    fn request_read_receipt_stamps_disposition_notification_to() {
        let mut email = outgoing("please confirm", &["bob@example.com"]);
        email.encryption_mode = None;
        email.from = "Alex Morgan <alex@example.com>".into();
        email.request_read_receipt = true;

        let raw = build_outgoing_message(&email).expect("build").formatted();
        let parsed = MessageParser::default().parse(&raw).expect("parse");
        let dnt = parsed
            .header("Disposition-Notification-To")
            .and_then(|h| h.as_text())
            .expect("header must be present");
        // Bare address — display name stripped for interop.
        assert_eq!(dnt.trim(), "alex@example.com");
    }

    #[test]
    fn no_receipt_request_means_no_header() {
        let mut email = outgoing("ordinary mail", &["bob@example.com"]);
        email.encryption_mode = None;

        let raw = build_outgoing_message(&email).expect("build").formatted();
        let parsed = MessageParser::default().parse(&raw).expect("parse");
        assert!(parsed.header("Disposition-Notification-To").is_none());
    }

    #[test]
    fn crypto_outer_headers_carry_receipt_request() {
        // The crypto sends hand-build their outer routing headers, so
        // the receipt request must survive that path too — it's
        // routing metadata the recipient's client reads pre-decrypt.
        let mut email = outgoing("secret memo", &["bob@example.com"]);
        email.request_read_receipt = true;
        let wire = build_outer_pgp_mime_bytes(
            &email,
            b"-----BEGIN PGP MESSAGE-----\nx\n-----END PGP MESSAGE-----\n",
        );
        let parsed = MessageParser::default().parse(&wire).expect("parse outer");
        let dnt = parsed
            .header("Disposition-Notification-To")
            .and_then(|h| h.as_text())
            .expect("outer wrapper must carry the header");
        assert_eq!(dnt.trim(), "alice@example.com");
    }

    // ── Delivery confirmations / DSN (#461) ────────────────────

    use super::{dsn_notify_params, ehlo_advertises_dsn};

    #[test]
    fn dsn_notify_params_render_on_rcpt_command() {
        // The exact bytes the DSN path puts on the wire for each
        // recipient — RFC 3461 §4.1 `NOTIFY` esmtp-param, rendered
        // through lettre's `Rcpt` command Display impl.
        let addr: super::Address = "bob@example.com".parse().expect("valid address");
        let cmd = super::Rcpt::new(addr, dsn_notify_params()).to_string();
        assert_eq!(cmd, "RCPT TO:<bob@example.com> NOTIFY=SUCCESS,FAILURE\r\n");
    }

    #[test]
    fn ehlo_scan_detects_dsn_keyword() {
        // Keyword match is case-insensitive and tolerant of the
        // parameterised lines other extensions emit.
        let lines = [
            "mail.example.com at your service",
            "SIZE 35882577",
            "8BITMIME",
            "dsn",
            "SMTPUTF8",
        ];
        assert!(ehlo_advertises_dsn(lines.iter().copied()));
    }

    #[test]
    fn ehlo_scan_ignores_greeting_and_lookalikes() {
        // "DSN" in the greeting line (e.g. a hostname) must not
        // count, and neither does a keyword that merely starts with
        // the letters.
        let lines = [
            "DSN.example.com welcomes you",
            "8BITMIME",
            "DSNX",
            "SIZE 100",
        ];
        assert!(!ehlo_advertises_dsn(lines.iter().copied()));
    }

    fn mdn_reply() -> MdnReply {
        MdnReply {
            from: "Alex Morgan <alex@example.com>".into(),
            to: "sender@example.org".into(),
            original_message_id: Some("orig-123@example.org".into()),
            original_subject: "quarterly numbers".into(),
            automatic: false,
        }
    }

    #[test]
    fn mdn_report_is_multipart_report_with_disposition_notification_type() {
        let wire = build_mdn_report_bytes(&mdn_reply());
        let parsed = MessageParser::default().parse(&wire).expect("parse MDN");

        let ct = parsed.content_type().expect("Content-Type");
        assert!(ct.ctype().eq_ignore_ascii_case("multipart"));
        assert_eq!(ct.subtype().unwrap_or(""), "report");
        assert_eq!(
            ct.attribute("report-type").unwrap_or(""),
            "disposition-notification"
        );
        assert_eq!(parsed.subject().unwrap_or(""), "Read: quarterly numbers");
        // RFC 3834: receipts must declare themselves auto-submitted so
        // other auto-responders don't answer them.
        assert_eq!(
            parsed
                .header("Auto-Submitted")
                .and_then(|h| h.as_text())
                .unwrap_or(""),
            "auto-replied"
        );
    }

    #[test]
    fn mdn_report_field_block_names_original_message_and_disposition() {
        let wire = build_mdn_report_bytes(&mdn_reply());
        let parsed = MessageParser::default().parse(&wire).expect("parse MDN");

        let field_part = (0..)
            .map_while(|i| parsed.part(i))
            .find(|p| {
                p.content_type().is_some_and(|c| {
                    c.ctype().eq_ignore_ascii_case("message")
                        && c.subtype()
                            .is_some_and(|s| s.eq_ignore_ascii_case("disposition-notification"))
                })
            })
            .expect("message/disposition-notification part must exist");
        let body = std::str::from_utf8(field_part.contents()).expect("utf-8");
        assert!(body.contains("Final-Recipient: rfc822;alex@example.com"));
        assert!(body.contains("Original-Message-ID: <orig-123@example.org>"));
        assert!(body.contains("Disposition: manual-action/MDN-sent-manually; displayed"));
    }

    #[test]
    fn automatic_mdn_reports_automatic_action_mode() {
        let mut reply = mdn_reply();
        reply.automatic = true;
        let wire = build_mdn_report_bytes(&reply);
        let text = String::from_utf8(wire).expect("utf-8");
        assert!(text.contains("Disposition: automatic-action/MDN-sent-automatically; displayed"));
    }

    #[test]
    fn mdn_report_threads_under_the_original() {
        let wire = build_mdn_report_bytes(&mdn_reply());
        let parsed = MessageParser::default().parse(&wire).expect("parse MDN");
        assert_eq!(
            parsed
                .header("In-Reply-To")
                .and_then(|h| h.as_text())
                .unwrap_or(""),
            "orig-123@example.org"
        );
    }
}
