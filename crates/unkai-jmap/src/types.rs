//! JMAP protocol types — serde models for the JSON structures defined
//! in RFC 8620 (JMAP Core) and RFC 8621 (JMAP Mail).
//!
//! These are internal to the crate; the public API uses `unkai_core::models`.
//!
//! Many fields are present for protocol completeness but not yet
//! consumed by the client — allow dead_code crate-wide for types.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Session (RFC 8620 §2) ──────────────────────────────────────

/// The JMAP Session resource, returned by `GET /.well-known/jmap`.
///
/// Contains the API endpoint URLs and the list of accounts the
/// authenticated user has access to, along with their capabilities.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// URL to POST method calls to.
    pub api_url: String,
    /// URL template for downloading blobs (attachments, raw messages).
    pub download_url: String,
    /// URL for uploading blobs.
    pub upload_url: String,
    /// URL for Server-Sent Events push notifications.
    pub event_source_url: String,
    /// Map of account ID → account metadata.
    pub accounts: HashMap<String, SessionAccount>,
    /// The ID of the "primary" account for mail (often the only one).
    pub primary_accounts: HashMap<String, String>,
}

/// Per-account metadata within the Session resource.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAccount {
    pub name: String,
    pub is_personal: bool,
    pub is_read_only: bool,
}

// ── Request / Response envelope (RFC 8620 §3) ──────────────────

/// A JMAP request body — wraps one or more method calls.
///
/// ```json
/// {
///   "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
///   "methodCalls": [
///     ["Mailbox/get", { "accountId": "abc" }, "call0"]
///   ]
/// }
/// ```
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JmapRequest {
    pub using: Vec<String>,
    pub method_calls: Vec<MethodCall>,
}

/// A single method invocation: `[methodName, arguments, callId]`.
///
/// Serialised as a JSON array (not an object) per the spec.
#[derive(Debug, Clone)]
pub struct MethodCall {
    pub name: String,
    pub args: serde_json::Value,
    pub call_id: String,
}

impl Serialize for MethodCall {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(3))?;
        seq.serialize_element(&self.name)?;
        seq.serialize_element(&self.args)?;
        seq.serialize_element(&self.call_id)?;
        seq.end()
    }
}

/// A JMAP response body — wraps method responses.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JmapResponse {
    pub method_responses: Vec<MethodResponse>,
    #[serde(default)]
    pub session_state: String,
}

/// A single method response: `[methodName, arguments, callId]`.
///
/// Deserialised from a JSON 3-element array.
#[derive(Debug, Clone)]
pub struct MethodResponse {
    pub name: String,
    pub args: serde_json::Value,
    pub call_id: String,
}

impl<'de> Deserialize<'de> for MethodResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let arr: Vec<serde_json::Value> = Vec::deserialize(deserializer)?;
        if arr.len() != 3 {
            return Err(serde::de::Error::custom(format!(
                "expected 3-element array, got {}",
                arr.len()
            )));
        }
        let name = arr[0]
            .as_str()
            .ok_or_else(|| serde::de::Error::custom("method name must be a string"))?
            .to_string();
        let call_id = arr[2]
            .as_str()
            .ok_or_else(|| serde::de::Error::custom("call ID must be a string"))?
            .to_string();
        Ok(Self {
            name,
            args: arr[1].clone(),
            call_id,
        })
    }
}

// ── Mailbox (RFC 8621 §2) ──────────────────────────────────────

/// A JMAP Mailbox — the equivalent of an IMAP folder.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JmapMailbox {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Standard role: "inbox", "sent", "drafts", "trash", "junk", "archive", etc.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub sort_order: u32,
    #[serde(default)]
    pub total_emails: u32,
    #[serde(default)]
    pub unread_emails: u32,
    #[serde(default)]
    pub total_threads: u32,
    #[serde(default)]
    pub unread_threads: u32,
}

// ── Email (RFC 8621 §4) ────────────────────────────────────────

/// A JMAP Email object — the full message with headers and body.
///
/// Which properties are returned depends on what we request in
/// `Email/get`'s `properties` argument.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JmapEmail {
    pub id: String,
    #[serde(default)]
    pub blob_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub mailbox_ids: HashMap<String, bool>,
    #[serde(default)]
    pub keywords: HashMap<String, bool>,
    #[serde(default)]
    pub from: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub to: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub cc: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub received_at: Option<String>,
    #[serde(default)]
    pub has_attachment: bool,
    /// Preview text — a short plain-text snippet of the body.
    #[serde(default)]
    pub preview: Option<String>,
    /// Full body values keyed by part ID.
    #[serde(default)]
    pub body_values: HashMap<String, BodyValue>,
    /// The text body parts (references into body_values).
    #[serde(default)]
    pub text_body: Vec<BodyPart>,
    /// The HTML body parts (references into body_values).
    #[serde(default)]
    pub html_body: Vec<BodyPart>,
    /// Attachment parts.
    #[serde(default)]
    pub attachments: Vec<BodyPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailAddress {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

impl EmailAddress {
    /// Format as "Name <email>" or just "email".
    pub fn display(&self) -> String {
        match (&self.name, &self.email) {
            (Some(n), Some(e)) if !n.is_empty() => format!("{n} <{e}>"),
            (_, Some(e)) => e.clone(),
            (Some(n), _) => n.clone(),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyValue {
    pub value: String,
    #[serde(default)]
    pub is_encoding_problem: bool,
    #[serde(default)]
    pub is_truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyPart {
    pub part_id: Option<String>,
    #[serde(default)]
    pub blob_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: u64,
}

// ── Email/query result ─────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailQueryResult {
    pub account_id: String,
    pub ids: Vec<String>,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub position: u64,
}

/// The `list` inside an `Email/get` or `Mailbox/get` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetResult<T> {
    pub account_id: String,
    pub state: String,
    pub list: Vec<T>,
    #[serde(default)]
    pub not_found: Vec<String>,
}

// ── EmailSubmission (RFC 8621 §7) ──────────────────────────────

/// An email to be created on the server via `Email/set`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailCreate {
    pub mailbox_ids: HashMap<String, bool>,
    pub from: Vec<EmailAddress>,
    pub to: Vec<EmailAddress>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<EmailAddress>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<EmailAddress>,
    pub subject: String,
    pub body_values: HashMap<String, BodyValueCreate>,
    pub text_body: Vec<BodyPartCreate>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub html_body: Vec<BodyPartCreate>,
    pub keywords: HashMap<String, bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyValueCreate {
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyPartCreate {
    pub part_id: String,
    #[serde(rename = "type")]
    pub content_type: String,
}

/// A submission record that tells the server to actually send the email.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailSubmissionCreate {
    /// Reference to the email ID (use `#emailCreate` for back-reference).
    pub email_id: String,
    pub identity_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope: Option<SubmissionEnvelope>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionEnvelope {
    pub mail_from: SubmissionAddress,
    pub rcpt_to: Vec<SubmissionAddress>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionAddress {
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

// ── Identity (for EmailSubmission) ─────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
}
