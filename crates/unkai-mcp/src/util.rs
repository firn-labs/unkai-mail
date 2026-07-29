//! Shared helpers for tool handlers: argument parsing, error
//! construction, and result shaping (#441 — extracted from the
//! mail tools so the groupware tool modules use one vocabulary).
//!
//! Conventions:
//!
//! - Argument errors are `invalid_params` with a message that tells
//!   the agent how to fix the call (which parameter, what shape,
//!   which discovery tool lists valid values).
//! - Infrastructure failures are `internal_error`.
//! - Tool output is one JSON text block with snake_case keys.

use chrono::{DateTime, Utc};
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject};
use serde_json::Value;
use unkai_core::models::Account;
use unkai_store::account_store;

use crate::registry::ToolContext;

pub(crate) fn invalid(message: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(message.into(), None)
}

pub(crate) fn internal(message: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(message.into(), None)
}

pub(crate) fn arg<'a>(args: &'a Option<JsonObject>, key: &str) -> Option<&'a Value> {
    args.as_ref()
        .and_then(|a| a.get(key))
        .filter(|v| !v.is_null())
}

pub(crate) fn optional_str(
    args: &Option<JsonObject>,
    key: &str,
) -> Result<Option<String>, ErrorData> {
    match arg(args, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(invalid(format!("parameter '{key}' must be a string"))),
    }
}

pub(crate) fn required_str(args: &Option<JsonObject>, key: &str) -> Result<String, ErrorData> {
    optional_str(args, key)?
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid(format!("parameter '{key}' is required")))
}

pub(crate) fn optional_u32(args: &Option<JsonObject>, key: &str) -> Result<Option<u32>, ErrorData> {
    match arg(args, key) {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| invalid(format!("parameter '{key}' is out of range"))),
        Some(_) => Err(invalid(format!("parameter '{key}' must be an integer"))),
    }
}

pub(crate) fn required_u32(args: &Option<JsonObject>, key: &str) -> Result<u32, ErrorData> {
    optional_u32(args, key)?.ok_or_else(|| invalid(format!("parameter '{key}' is required")))
}

pub(crate) fn optional_bool(
    args: &Option<JsonObject>,
    key: &str,
) -> Result<Option<bool>, ErrorData> {
    match arg(args, key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid(format!("parameter '{key}' must be a boolean"))),
    }
}

pub(crate) fn optional_str_list(
    args: &Option<JsonObject>,
    key: &str,
) -> Result<Vec<String>, ErrorData> {
    match arg(args, key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) if !s.trim().is_empty() => Ok(s.clone()),
                _ => Err(invalid(format!(
                    "parameter '{key}' must be an array of non-empty strings"
                ))),
            })
            .collect(),
        Some(_) => Err(invalid(format!(
            "parameter '{key}' must be an array of strings"
        ))),
    }
}

pub(crate) fn required_str_list(
    args: &Option<JsonObject>,
    key: &str,
) -> Result<Vec<String>, ErrorData> {
    let list = optional_str_list(args, key)?;
    if list.is_empty() {
        return Err(invalid(format!(
            "parameter '{key}' is required and must not be empty"
        )));
    }
    Ok(list)
}

/// RFC 3339 timestamp parameter (e.g. `2026-08-01T09:00:00Z` or
/// with a numeric offset).  Everything is normalised to UTC —
/// the same convention the cache stores events in.
pub(crate) fn required_datetime(
    args: &Option<JsonObject>,
    key: &str,
) -> Result<DateTime<Utc>, ErrorData> {
    let raw = required_str(args, key)?;
    DateTime::parse_from_rfc3339(&raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            invalid(format!(
                "parameter '{key}' must be an RFC 3339 timestamp like \
                 2026-08-01T09:00:00Z ({e})"
            ))
        })
}

// ── Mail-account helpers ────────────────────────────────────────

pub(crate) fn load_accounts(ctx: &ToolContext) -> Result<Vec<Account>, ErrorData> {
    account_store::load_accounts(&ctx.cache)
        .map_err(|e| internal(format!("could not load accounts: {e}")))
}

/// Reject unknown account ids with a pointer at `list_accounts` —
/// without this, a typo'd id would just return an empty folder
/// list and send the agent down a "no folders synced" dead end.
pub(crate) fn require_known_account(ctx: &ToolContext, account_id: &str) -> Result<(), ErrorData> {
    if load_accounts(ctx)?.iter().any(|a| a.id == account_id) {
        Ok(())
    } else {
        Err(invalid(format!(
            "unknown account_id '{account_id}' — call list_accounts for valid ids"
        )))
    }
}

// ── Output helpers ──────────────────────────────────────────────

pub(crate) fn json_result(value: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

pub(crate) fn schema(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => unreachable!("tool schemas are object literals"),
    }
}
