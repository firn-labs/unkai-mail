//! JMAP client — connects to a JMAP-capable mail server via HTTPS
//! and provides methods that mirror `ImapClient` so the Tauri layer
//! can switch transparently.

use chrono::{DateTime, Utc};
use nimbus_core::error::NimbusError;
use nimbus_core::models::{Email, EmailEnvelope, Folder, OutgoingEmail};
use nimbus_core::url::ensure_https;
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use tracing::info;
use url::Url;

use crate::types::*;

/// An authenticated JMAP client, ready to interact with mailboxes.
///
/// Unlike `ImapClient`, this is stateless — there's no persistent TCP
/// session. Each method call is an independent HTTP POST. The session
/// metadata (API URL, account ID) is cached from the initial discovery.
pub struct JmapClient {
    /// Pre-configured reqwest client.
    http: Client,
    /// Stored username for Basic Auth on each request.
    username: String,
    /// Stored password for Basic Auth on each request.
    password: String,
    /// The JMAP Session resource (API URLs, account info).
    session: Session,
    /// The primary mail account ID on this server.
    account_id: String,
    /// Base URL of the server (for resolving relative URLs).
    base_url: Url,
}

/// The JMAP capabilities URIs we declare in every request.
const CAPABILITIES: &[&str] = &[
    "urn:ietf:params:jmap:core",
    "urn:ietf:params:jmap:mail",
    "urn:ietf:params:jmap:submission",
];

impl JmapClient {
    /// Discover the JMAP session and authenticate.
    ///
    /// # How it works
    ///
    /// 1. Sends `GET /.well-known/jmap` to the server (RFC 8620 §2.2).
    /// 2. The server responds with a Session object containing:
    ///    - `apiUrl`: where to POST method calls
    ///    - `accounts`: which mail accounts are available
    ///    - `primaryAccounts`: which account to use by default
    /// 3. We pick the primary mail account and store everything for
    ///    subsequent method calls.
    ///
    /// Authentication is HTTP Basic Auth (username + password) which
    /// the server validates on every request.
    ///
    /// `base_url` should be the server root, e.g. `https://mail.example.com`.
    /// The `.well-known/jmap` path is appended automatically.
    pub async fn connect(
        base_url: &str,
        username: &str,
        password: &str,
    ) -> Result<Self, NimbusError> {
        info!("Discovering JMAP session at {base_url}");

        let base = Url::parse(base_url)
            .map_err(|e| NimbusError::Network(format!("Invalid JMAP URL '{base_url}': {e}")))?;

        let well_known = base
            .join("/.well-known/jmap")
            .map_err(|e| NimbusError::Network(format!("Failed to build .well-known URL: {e}")))?;

        let http = Client::builder()
            .build()
            .map_err(|e| NimbusError::Network(format!("Failed to build HTTP client: {e}")))?;

        let resp = http
            .get(well_known.as_str())
            .basic_auth(username, Some(password))
            .send()
            .await
            .map_err(|e| NimbusError::Network(format!("JMAP session discovery failed: {e}")))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(NimbusError::Auth(
                "JMAP authentication failed — check username and password".into(),
            ));
        }

        if !resp.status().is_success() {
            return Err(NimbusError::Network(format!(
                "JMAP session discovery returned HTTP {}",
                resp.status()
            )));
        }

        let session: Session = resp.json().await.map_err(|e| {
            NimbusError::Protocol(format!("Failed to parse JMAP session response: {e}"))
        })?;

        // Find the primary mail account. The key in `primaryAccounts` is
        // the capability URI; the value is the account ID.
        let account_id = session
            .primary_accounts
            .get("urn:ietf:params:jmap:mail")
            .cloned()
            .ok_or_else(|| {
                NimbusError::Protocol("Server has no primary mail account in JMAP session".into())
            })?;

        info!(
            "JMAP session established: apiUrl={}, accountId={}",
            session.api_url, account_id
        );

        Ok(Self {
            http,
            username: username.to_string(),
            password: password.to_string(),
            session,
            account_id,
            base_url: base,
        })
    }

    /// Resolve a possibly-relative URL from the session against the base URL.
    fn resolve_url(&self, url: &str) -> Result<String, NimbusError> {
        if url.starts_with("http://") || url.starts_with("https://") {
            Ok(url.to_string())
        } else {
            self.base_url
                .join(url)
                .map(|u| u.to_string())
                .map_err(|e| NimbusError::Network(format!("Failed to resolve URL '{url}': {e}")))
        }
    }

    /// Send a JMAP request and return the response.
    async fn call(&self, method_calls: Vec<MethodCall>) -> Result<JmapResponse, NimbusError> {
        let api_url = self.resolve_url(&self.session.api_url)?;
        ensure_https(&api_url)?;

        let request = JmapRequest {
            using: CAPABILITIES.iter().map(|s| s.to_string()).collect(),
            method_calls,
        };

        let resp = self
            .http
            .post(&api_url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&request)
            .send()
            .await
            .map_err(|e| NimbusError::Network(format!("JMAP API request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(NimbusError::Protocol(format!(
                "JMAP API returned HTTP {status}: {body}"
            )));
        }

        resp.json()
            .await
            .map_err(|e| NimbusError::Protocol(format!("Failed to parse JMAP API response: {e}")))
    }

    /// Find a method response by its call ID.
    fn find_response<'a>(
        responses: &'a [MethodResponse],
        call_id: &str,
    ) -> Result<&'a Value, NimbusError> {
        responses
            .iter()
            .find(|r| r.call_id == call_id)
            .map(|r| &r.args)
            .ok_or_else(|| NimbusError::Protocol(format!("No response for call '{call_id}'")))
    }

    // ── Public API (mirrors ImapClient) ────────────────────────

    /// List all mailboxes (folders) on the server.
    ///
    /// Uses `Mailbox/get` to fetch all mailboxes with their names, roles,
    /// and unread counts, then maps them to `nimbus_core::models::Folder`.
    pub async fn list_folders(&self) -> Result<Vec<Folder>, NimbusError> {
        let resp = self
            .call(vec![MethodCall {
                name: "Mailbox/get".into(),
                args: json!({
                    "accountId": self.account_id,
                }),
                call_id: "mbox0".into(),
            }])
            .await?;

        let args = Self::find_response(&resp.method_responses, "mbox0")?;
        let result: GetResult<JmapMailbox> = serde_json::from_value(args.clone())
            .map_err(|e| NimbusError::Protocol(format!("Failed to parse Mailbox/get: {e}")))?;

        // Build a map of id → mailbox for resolving parent names.
        let by_id: HashMap<&str, &JmapMailbox> =
            result.list.iter().map(|m| (m.id.as_str(), m)).collect();

        let mut folders: Vec<Folder> = result
            .list
            .iter()
            .map(|mbox| {
                // Build the full path name by walking up parent_id links.
                let full_name = build_full_name(mbox, &by_id);

                // Map the JMAP role to IMAP-style attributes so the existing
                // Sidebar icon logic works without changes.
                let attributes = match mbox.role.as_deref() {
                    Some("inbox") => vec!["Inbox".into()],
                    Some("sent") => vec!["Sent".into()],
                    Some("drafts") => vec!["Drafts".into()],
                    Some("trash") => vec!["Trash".into()],
                    Some("junk") => vec!["Junk".into()],
                    Some("archive") => vec!["Archive".into()],
                    Some(other) => vec![other.to_string()],
                    None => vec![],
                };

                Folder {
                    name: full_name,
                    delimiter: Some("/".into()),
                    attributes,
                    unread_count: Some(mbox.unread_emails),
                }
            })
            .collect();

        // Sort: Inbox first, then by name.
        folders.sort_by(|a, b| {
            let a_inbox = a.name.eq_ignore_ascii_case("inbox");
            let b_inbox = b.name.eq_ignore_ascii_case("inbox");
            match (a_inbox, b_inbox) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.cmp(&b.name),
            }
        });

        info!("JMAP: found {} mailboxes", folders.len());
        Ok(folders)
    }

    /// Fetch the newest `limit` email envelopes from a mailbox.
    ///
    /// Two-step process:
    /// 1. `Email/query` to get the IDs of the newest emails, sorted by
    ///    receivedAt descending.
    /// 2. `Email/get` to fetch the envelope properties for those IDs.
    ///
    /// `since_uid` is ignored for JMAP (it uses server-side sorting and
    /// pagination instead of UID ranges), but accepted for API compatibility.
    pub async fn fetch_envelopes(
        &self,
        folder: &str,
        limit: u32,
        _since_uid: Option<u32>,
    ) -> Result<Vec<EmailEnvelope>, NimbusError> {
        // First, find the mailbox ID for this folder name.
        let mailbox_id = self.find_mailbox_id(folder).await?;

        // Batch both calls in a single HTTP request — JMAP's killer feature.
        let resp = self
            .call(vec![
                MethodCall {
                    name: "Email/query".into(),
                    args: json!({
                        "accountId": self.account_id,
                        "filter": {
                            "inMailbox": mailbox_id,
                        },
                        "sort": [{
                            "property": "receivedAt",
                            "isAscending": false,
                        }],
                        "limit": limit,
                    }),
                    call_id: "q0".into(),
                },
                MethodCall {
                    name: "Email/get".into(),
                    args: json!({
                        "accountId": self.account_id,
                        // Back-reference: use the IDs from the query result.
                        "#ids": {
                            "resultOf": "q0",
                            "name": "Email/query",
                            "path": "/ids",
                        },
                        "properties": [
                            "id", "from", "subject", "receivedAt",
                            "keywords", "hasAttachment", "mailboxIds",
                        ],
                    }),
                    call_id: "g0".into(),
                },
            ])
            .await?;

        let args = Self::find_response(&resp.method_responses, "g0")?;
        let result: GetResult<JmapEmail> = serde_json::from_value(args.clone())
            .map_err(|e| NimbusError::Protocol(format!("Failed to parse Email/get: {e}")))?;

        let envelopes: Vec<EmailEnvelope> = result
            .list
            .iter()
            .enumerate()
            .map(|(i, email)| {
                let from = email
                    .from
                    .as_ref()
                    .and_then(|addrs| addrs.first())
                    .map(|a| a.display())
                    .unwrap_or_default();

                let date = email
                    .received_at
                    .as_deref()
                    .and_then(parse_jmap_date)
                    .unwrap_or_else(Utc::now);

                // JMAP uses keywords instead of flags:
                //   $seen → read, $flagged → starred,
                //   $answered → user has replied to this message (#255)
                let is_read = email.keywords.contains_key("$seen");
                let is_starred = email.keywords.contains_key("$flagged");
                let is_answered = email.keywords.contains_key("$answered");

                EmailEnvelope {
                    // JMAP IDs are strings, but our model uses u32 UIDs.
                    // We use a hash of the ID as a synthetic UID. This is
                    // stable across sessions for the same message.
                    uid: synthetic_uid(&email.id, i),
                    folder: folder.to_string(),
                    from,
                    subject: email.subject.clone().unwrap_or_default(),
                    date,
                    is_read,
                    is_starred,
                    is_answered,
                    replied_kind: None,
                    // Stamped into the cache by the caller; left empty
                    // here for the same reason as the IMAP path.
                    account_id: String::new(),
                    // Threading headers (#277) — JMAP carries
                    // `messageId` / `inReplyTo` / `references` in
                    // its Email object.  Plumbing those through
                    // the JMAP types is a follow-up; until then
                    // JMAP-fetched envelopes thread only via the
                    // server-side `thread_id` JMAP already
                    // provides.
                    message_id: None,
                    in_reply_to: None,
                    references_ids: Vec::new(),
                }
            })
            .collect();

        info!(
            "JMAP: fetched {} envelopes from '{folder}'",
            envelopes.len()
        );
        Ok(envelopes)
    }

    /// Fetch a full message (headers + body) by folder + UID.
    ///
    /// For JMAP, the "UID" is our synthetic hash — we need to map it back
    /// to a JMAP email ID. We do this by querying the mailbox and finding
    /// the matching email. If the caller provides the JMAP ID directly
    /// (future optimisation), we skip the lookup.
    pub async fn fetch_message(
        &self,
        folder: &str,
        uid: u32,
        account_id: &str,
    ) -> Result<Email, NimbusError> {
        // Find the JMAP email ID for this synthetic UID by querying
        // the mailbox and matching. This is O(n) in the mailbox size,
        // but we limit to a reasonable range.
        let jmap_id = self.resolve_jmap_id(folder, uid).await?;

        let resp = self
            .call(vec![MethodCall {
                name: "Email/get".into(),
                args: json!({
                    "accountId": self.account_id,
                    "ids": [jmap_id],
                    "properties": [
                        "id", "from", "to", "cc", "subject", "receivedAt",
                        "keywords", "hasAttachment", "mailboxIds",
                        "bodyValues", "textBody", "htmlBody", "attachments",
                    ],
                    "fetchAllBodyValues": true,
                }),
                call_id: "msg0".into(),
            }])
            .await?;

        let args = Self::find_response(&resp.method_responses, "msg0")?;
        let result: GetResult<JmapEmail> = serde_json::from_value(args.clone())
            .map_err(|e| NimbusError::Protocol(format!("Failed to parse Email/get: {e}")))?;

        let email = result
            .list
            .into_iter()
            .next()
            .ok_or_else(|| NimbusError::Protocol(format!("No email with ID '{jmap_id}'")))?;

        let from = email
            .from
            .as_ref()
            .and_then(|a| a.first())
            .map(|a| a.display())
            .unwrap_or_default();

        let to: Vec<String> = email
            .to
            .as_ref()
            .map(|addrs| addrs.iter().filter_map(|a| a.email.clone()).collect())
            .unwrap_or_default();

        let cc: Vec<String> = email
            .cc
            .as_ref()
            .map(|addrs| addrs.iter().filter_map(|a| a.email.clone()).collect())
            .unwrap_or_default();

        // Extract body text from bodyValues.
        let body_text = email
            .text_body
            .iter()
            .filter_map(|part| {
                part.part_id
                    .as_ref()
                    .and_then(|pid| email.body_values.get(pid))
                    .map(|bv| bv.value.clone())
            })
            .collect::<Vec<_>>();
        let body_text = if body_text.is_empty() {
            None
        } else {
            Some(body_text.join("\n"))
        };

        let body_html = email
            .html_body
            .iter()
            .filter_map(|part| {
                part.part_id
                    .as_ref()
                    .and_then(|pid| email.body_values.get(pid))
                    .map(|bv| bv.value.clone())
            })
            .collect::<Vec<_>>();
        let body_html = if body_html.is_empty() {
            None
        } else {
            Some(body_html.join("\n"))
        };

        let date = email
            .received_at
            .as_deref()
            .and_then(parse_jmap_date)
            .unwrap_or_else(Utc::now);

        let is_read = email.keywords.contains_key("$seen");
        let is_starred = email.keywords.contains_key("$flagged");
        let has_attachments = email.has_attachment;

        info!("JMAP: fetched message '{}'", email.id);

        Ok(Email {
            id: email.id,
            account_id: account_id.to_string(),
            folder: folder.to_string(),
            from,
            to,
            cc,
            subject: email.subject.unwrap_or_default(),
            body_text,
            body_html,
            date,
            is_read,
            is_starred,
            has_attachments,
            // JMAP's Email/get returns attachment metadata under
            // `bodyValues`/`attachments`, but we don't fetch that
            // payload today — mirror the empty-list behaviour the
            // cache uses for older rows. Wiring up JMAP attachments
            // is its own issue once the IMAP side is proven.
            attachments: Vec::new(),
        })
    }

    /// Clear the `$seen` keyword on a message — i.e. mark it unread.
    /// Mirror of `mark_as_read`. JMAP keyword removal uses `null` in
    /// the patch object.
    pub async fn mark_as_unread(&self, folder: &str, uid: u32) -> Result<(), NimbusError> {
        let jmap_id = self.resolve_jmap_id(folder, uid).await?;

        let resp = self
            .call(vec![MethodCall {
                name: "Email/set".into(),
                args: json!({
                    "accountId": self.account_id,
                    "update": {
                        jmap_id.clone(): {
                            "keywords/$seen": null,
                        },
                    },
                }),
                call_id: "unread0".into(),
            }])
            .await?;

        let args = Self::find_response(&resp.method_responses, "unread0")?;
        if let Some(errors) = args.get("notUpdated")
            && let Some(err) = errors.get(&jmap_id)
        {
            return Err(NimbusError::Protocol(format!(
                "Failed to mark as unread: {err}"
            )));
        }

        info!("JMAP: cleared $seen on '{jmap_id}'");
        Ok(())
    }

    /// Mark a message as read by setting the `$seen` keyword.
    ///
    /// Uses `Email/set` with a keyword update — equivalent to IMAP's
    /// `UID STORE +FLAGS (\Seen)`.
    pub async fn mark_as_read(&self, folder: &str, uid: u32) -> Result<(), NimbusError> {
        let jmap_id = self.resolve_jmap_id(folder, uid).await?;

        let resp = self
            .call(vec![MethodCall {
                name: "Email/set".into(),
                args: json!({
                    "accountId": self.account_id,
                    "update": {
                        jmap_id.clone(): {
                            "keywords/$seen": true,
                        },
                    },
                }),
                call_id: "read0".into(),
            }])
            .await?;

        // Check for errors in the response.
        let args = Self::find_response(&resp.method_responses, "read0")?;
        if let Some(errors) = args.get("notUpdated")
            && let Some(err) = errors.get(&jmap_id)
        {
            return Err(NimbusError::Protocol(format!(
                "Failed to mark as read: {err}"
            )));
        }

        info!("JMAP: marked '{jmap_id}' as $seen");
        Ok(())
    }

    /// Set the `$answered` keyword on a message (#255) — equivalent
    /// of IMAP's `UID STORE +FLAGS (\Answered)`.  Called from the
    /// send path after a successful reply / reply-all / meeting
    /// reply so the original message reflects the answered state
    /// across clients.
    pub async fn mark_as_answered(&self, folder: &str, uid: u32) -> Result<(), NimbusError> {
        let jmap_id = self.resolve_jmap_id(folder, uid).await?;

        let resp = self
            .call(vec![MethodCall {
                name: "Email/set".into(),
                args: json!({
                    "accountId": self.account_id,
                    "update": {
                        jmap_id.clone(): {
                            "keywords/$answered": true,
                        },
                    },
                }),
                call_id: "answered0".into(),
            }])
            .await?;

        let args = Self::find_response(&resp.method_responses, "answered0")?;
        if let Some(errors) = args.get("notUpdated")
            && let Some(err) = errors.get(&jmap_id)
        {
            return Err(NimbusError::Protocol(format!(
                "Failed to mark as answered: {err}"
            )));
        }

        info!("JMAP: marked '{jmap_id}' as $answered");
        Ok(())
    }

    /// Send an email via JMAP Submission.
    ///
    /// This is a two-step process batched into one request:
    /// 1. `Email/set` to create the email object on the server.
    /// 2. `EmailSubmission/set` to tell the server to actually send it.
    ///
    /// The server handles SMTP delivery internally — we don't need a
    /// separate SMTP connection.
    pub async fn send_email(&self, email: &OutgoingEmail) -> Result<(), NimbusError> {
        // Find the identity (sending address) to use.
        let identity_id = self.find_identity(&email.from).await?;

        // Find the Drafts mailbox to store the email in (standard JMAP
        // practice — the submission then moves it to Sent automatically).
        let drafts_id = self
            .find_mailbox_by_role("drafts")
            .await
            .unwrap_or_else(|_| self.account_id.clone());

        // Build the email object.
        let mut body_values = HashMap::new();
        let mut text_body = Vec::new();
        let mut html_body = Vec::new();

        if let Some(ref text) = email.body_text {
            body_values.insert(
                "text".to_string(),
                BodyValueCreate {
                    value: text.clone(),
                    content_type: Some("text/plain".into()),
                },
            );
            text_body.push(BodyPartCreate {
                part_id: "text".into(),
                content_type: "text/plain".into(),
            });
        }

        if let Some(ref html) = email.body_html {
            body_values.insert(
                "html".to_string(),
                BodyValueCreate {
                    value: html.clone(),
                    content_type: Some("text/html".into()),
                },
            );
            html_body.push(BodyPartCreate {
                part_id: "html".into(),
                content_type: "text/html".into(),
            });
        }

        let to: Vec<EmailAddress> = email
            .to
            .iter()
            .map(|e| EmailAddress {
                name: None,
                email: Some(e.clone()),
            })
            .collect();

        let cc: Vec<EmailAddress> = email
            .cc
            .iter()
            .map(|e| EmailAddress {
                name: None,
                email: Some(e.clone()),
            })
            .collect();

        let bcc: Vec<EmailAddress> = email
            .bcc
            .iter()
            .map(|e| EmailAddress {
                name: None,
                email: Some(e.clone()),
            })
            .collect();

        // All recipients for the SMTP envelope.
        let all_rcpt: Vec<String> = email
            .to
            .iter()
            .chain(email.cc.iter())
            .chain(email.bcc.iter())
            .cloned()
            .collect();

        // Build the batched request: create email + submit it.
        let resp = self
            .call(vec![
                MethodCall {
                    name: "Email/set".into(),
                    args: json!({
                        "accountId": self.account_id,
                        "create": {
                            "draft": {
                                "mailboxIds": { drafts_id: true },
                                "from": [{ "email": email.from }],
                                "to": to,
                                "cc": cc,
                                "bcc": bcc,
                                "subject": email.subject,
                                "bodyValues": body_values,
                                "textBody": text_body,
                                "htmlBody": html_body,
                                "keywords": { "$draft": true },
                            },
                        },
                    }),
                    call_id: "emailCreate".into(),
                },
                MethodCall {
                    name: "EmailSubmission/set".into(),
                    args: json!({
                        "accountId": self.account_id,
                        "create": {
                            "sub": {
                                "emailId": "#draft",
                                "identityId": identity_id,
                                "envelope": {
                                    "mailFrom": { "email": email.from },
                                    "rcptTo": all_rcpt.iter().map(|e| json!({ "email": e })).collect::<Vec<_>>(),
                                },
                            },
                        },
                        // Automatically move from Drafts to Sent after submission.
                        "onSuccessUpdateEmail": {
                            "#sub": {
                                "keywords/$draft": null,
                                "keywords/$seen": true,
                            },
                        },
                    }),
                    call_id: "submit".into(),
                },
            ])
            .await?;

        // Check for creation errors.
        let email_args = Self::find_response(&resp.method_responses, "emailCreate")?;
        if let Some(errors) = email_args.get("notCreated")
            && let Some(err) = errors.get("draft")
        {
            return Err(NimbusError::Protocol(format!(
                "Failed to create email: {err}"
            )));
        }

        let sub_args = Self::find_response(&resp.method_responses, "submit")?;
        if let Some(errors) = sub_args.get("notCreated")
            && let Some(err) = errors.get("sub")
        {
            return Err(NimbusError::Protocol(format!(
                "Failed to submit email: {err}"
            )));
        }

        info!("JMAP: email sent successfully via submission");
        Ok(())
    }

    /// Return the event source URL for push notifications.
    ///
    /// The frontend (or a background task) can connect to this URL with
    /// an EventSource/SSE client to receive real-time change notifications
    /// without polling.
    pub fn event_source_url(&self) -> Result<String, NimbusError> {
        // The session's eventSourceUrl is a template with `{types}`,
        // `{closeafter}`, and `{ping}` placeholders (RFC 8620 §7.3).
        let url = self
            .session
            .event_source_url
            .replace("{types}", "*")
            .replace("{closeafter}", "no")
            .replace("{ping}", "30");
        self.resolve_url(&url)
    }

    /// The JMAP account ID for this session.
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    // ── Internal helpers ───────────────────────────────────────

    /// Find the JMAP mailbox ID for a given folder name.
    async fn find_mailbox_id(&self, folder: &str) -> Result<String, NimbusError> {
        let resp = self
            .call(vec![MethodCall {
                name: "Mailbox/get".into(),
                args: json!({
                    "accountId": self.account_id,
                }),
                call_id: "mfind".into(),
            }])
            .await?;

        let args = Self::find_response(&resp.method_responses, "mfind")?;
        let result: GetResult<JmapMailbox> = serde_json::from_value(args.clone())
            .map_err(|e| NimbusError::Protocol(format!("Failed to parse Mailbox/get: {e}")))?;

        let by_id: HashMap<&str, &JmapMailbox> =
            result.list.iter().map(|m| (m.id.as_str(), m)).collect();

        // Match by full path name or by role.
        result
            .list
            .iter()
            .find(|m| {
                let full = build_full_name(m, &by_id);
                full.eq_ignore_ascii_case(folder)
                    || m.name.eq_ignore_ascii_case(folder)
                    || m.role
                        .as_deref()
                        .map(|r| r.eq_ignore_ascii_case(folder))
                        .unwrap_or(false)
            })
            .map(|m| m.id.clone())
            .ok_or_else(|| NimbusError::Protocol(format!("No JMAP mailbox matching '{folder}'")))
    }

    /// Find a mailbox ID by its standard role (inbox, sent, drafts, trash, etc.).
    async fn find_mailbox_by_role(&self, role: &str) -> Result<String, NimbusError> {
        let resp = self
            .call(vec![MethodCall {
                name: "Mailbox/get".into(),
                args: json!({
                    "accountId": self.account_id,
                }),
                call_id: "mrole".into(),
            }])
            .await?;

        let args = Self::find_response(&resp.method_responses, "mrole")?;
        let result: GetResult<JmapMailbox> = serde_json::from_value(args.clone())
            .map_err(|e| NimbusError::Protocol(format!("Failed to parse Mailbox/get: {e}")))?;

        result
            .list
            .iter()
            .find(|m| m.role.as_deref() == Some(role))
            .map(|m| m.id.clone())
            .ok_or_else(|| NimbusError::Protocol(format!("No JMAP mailbox with role '{role}'")))
    }

    /// Resolve a synthetic UID back to a JMAP email ID.
    ///
    /// We query the mailbox and scan for an email whose synthetic UID
    /// matches. This is a trade-off: JMAP uses string IDs while our
    /// model uses u32 UIDs. A production system would store a
    /// JMAP ID ↔ UID mapping in the cache.
    async fn resolve_jmap_id(&self, folder: &str, uid: u32) -> Result<String, NimbusError> {
        let mailbox_id = self.find_mailbox_id(folder).await?;

        // Query a reasonable window of recent emails.
        let resp = self
            .call(vec![
                MethodCall {
                    name: "Email/query".into(),
                    args: json!({
                        "accountId": self.account_id,
                        "filter": { "inMailbox": mailbox_id },
                        "sort": [{ "property": "receivedAt", "isAscending": false }],
                        "limit": 200,
                    }),
                    call_id: "rq".into(),
                },
                MethodCall {
                    name: "Email/get".into(),
                    args: json!({
                        "accountId": self.account_id,
                        "#ids": {
                            "resultOf": "rq",
                            "name": "Email/query",
                            "path": "/ids",
                        },
                        "properties": ["id"],
                    }),
                    call_id: "rg".into(),
                },
            ])
            .await?;

        let args = Self::find_response(&resp.method_responses, "rg")?;
        let result: GetResult<JmapEmail> = serde_json::from_value(args.clone())
            .map_err(|e| NimbusError::Protocol(format!("Failed to parse Email/get: {e}")))?;

        result
            .list
            .iter()
            .enumerate()
            .find(|(i, e)| synthetic_uid(&e.id, *i) == uid)
            .map(|(_, e)| e.id.clone())
            .ok_or_else(|| {
                NimbusError::Protocol(format!(
                    "No JMAP email found for synthetic UID {uid} in '{folder}'"
                ))
            })
    }

    /// Find the JMAP Identity ID for a given email address.
    ///
    /// Identities represent "from" addresses the user can send as.
    /// We match the requested from address against the server's identities.
    async fn find_identity(&self, from_email: &str) -> Result<String, NimbusError> {
        let resp = self
            .call(vec![MethodCall {
                name: "Identity/get".into(),
                args: json!({
                    "accountId": self.account_id,
                }),
                call_id: "id0".into(),
            }])
            .await?;

        let args = Self::find_response(&resp.method_responses, "id0")?;
        let result: GetResult<Identity> = serde_json::from_value(args.clone())
            .map_err(|e| NimbusError::Protocol(format!("Failed to parse Identity/get: {e}")))?;

        // Try exact match first, then case-insensitive.
        result
            .list
            .iter()
            .find(|id| id.email == from_email)
            .or_else(|| {
                result
                    .list
                    .iter()
                    .find(|id| id.email.eq_ignore_ascii_case(from_email))
            })
            .or_else(|| result.list.first())
            .map(|id| id.id.clone())
            .ok_or_else(|| NimbusError::Protocol("No JMAP identities available for sending".into()))
    }

    /// Test the JMAP connection — used during account setup.
    ///
    /// If `connect()` succeeds, the connection is valid. This is just
    /// a convenience wrapper that returns a human-readable message.
    pub async fn test(
        base_url: &str,
        username: &str,
        password: &str,
    ) -> Result<String, NimbusError> {
        let client = Self::connect(base_url, username, password).await?;
        let account_name = client
            .session
            .accounts
            .get(&client.account_id)
            .map(|a| a.name.as_str())
            .unwrap_or("unknown");
        Ok(format!(
            "JMAP login succeeded (account: {account_name}, id: {})",
            client.account_id
        ))
    }
}

// ── Free-standing helpers ──────────────────────────────────────

/// Build a full hierarchical folder name by walking up parent_id links.
///
/// E.g. if "Work" has parent "INBOX", the result is "INBOX/Work".
fn build_full_name(mbox: &JmapMailbox, by_id: &HashMap<&str, &JmapMailbox>) -> String {
    let mut parts = vec![mbox.name.clone()];
    let mut current = mbox.parent_id.as_deref();
    while let Some(pid) = current {
        if let Some(parent) = by_id.get(pid) {
            parts.push(parent.name.clone());
            current = parent.parent_id.as_deref();
        } else {
            break;
        }
    }
    parts.reverse();
    parts.join("/")
}

/// Generate a stable synthetic u32 UID from a JMAP string ID.
///
/// JMAP uses opaque string IDs; our `EmailEnvelope` model uses `u32`
/// UIDs (inherited from IMAP). We hash the ID to get a deterministic
/// u32. The positional index is mixed in to handle hash collisions
/// within a single result set (extremely unlikely but defensive).
fn synthetic_uid(jmap_id: &str, position: usize) -> u32 {
    // Simple FNV-1a hash — fast, good distribution, no crypto needed.
    let mut hash: u32 = 2_166_136_261;
    for byte in jmap_id.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    // Mix in position to avoid collisions in the same result set.
    hash ^ (position as u32)
}

/// Parse a JMAP date string (RFC 3339 / ISO 8601) to chrono UTC.
fn parse_jmap_date(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            // Some servers omit the timezone offset — try naive parse.
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|naive| naive.and_utc())
        })
}
