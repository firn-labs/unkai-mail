//! MCP contact tools (#441): `search_contacts` (read, default on)
//! and `create_contact` (write, default off).
//!
//! Reads come straight from the local contact cache — the same
//! rows the in-app autocomplete searches.  The write path mirrors
//! the in-app create flow: build a vCard, PUT it via CardDAV with
//! `If-None-Match: *`, and upsert the new row into the cache so
//! the Contacts view shows it without waiting for the next sync.

use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{Value, json};
use unkai_core::models::{ContactEmail, ContactPhone, NextcloudAccount};
use unkai_store::cache::ContactRow;

use crate::nc::{carddav_home_of, nc_password, resolve_nc_account};
use crate::registry::{NextcloudFeature, ToolAccess, ToolContext, ToolDescriptor, ToolRegistry};
use crate::util::{
    internal, invalid, json_result, optional_str, optional_str_list, optional_u32, required_str,
    schema,
};

const DEFAULT_SEARCH_LIMIT: u32 = 25;
const MAX_SEARCH_LIMIT: u32 = 100;

/// Collection name for the single addressbook a local source is
/// seeded with — mirrors the Tauri layer's constant.
const LOCAL_ADDRESSBOOK_NAME: &str = "local";

pub(crate) fn register_contact_tools(registry: &mut ToolRegistry) {
    registry.register(
        ToolDescriptor {
            id: "search_contacts",
            category: "contacts",
            access: ToolAccess::Read,
            requires: Some(NextcloudFeature::Contacts),
            description: "Search the user's locally synced contacts by name or email address \
                 (case-insensitive substring match). Returns name, email addresses, phone \
                 numbers, and organization — never photos. Only contacts already synced by \
                 Unkai Mail are searchable.",
        },
        schema(json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Substring to match against contact names and email addresses."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SEARCH_LIMIT,
                    "description": "Maximum contacts to return. Default 25."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(search_contacts(ctx, args))),
    );

    registry.register(
        ToolDescriptor {
            id: "create_contact",
            category: "contacts",
            access: ToolAccess::Write,
            requires: Some(NextcloudFeature::Contacts),
            description:
                "Create a new contact in the user's addressbook (CardDAV). Provide at least a \
                 display name; emails, phone numbers, an organization, and a note are \
                 optional. The contact is created in the connection's default addressbook \
                 unless addressbook_url picks another one.",
        },
        schema(json!({
            "type": "object",
            "required": ["display_name"],
            "properties": {
                "display_name": {
                    "type": "string",
                    "description": "Full name of the contact (vCard FN)."
                },
                "emails": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Email addresses, bare (no display-name part)."
                },
                "phones": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Phone numbers."
                },
                "organization": {"type": "string"},
                "note": {"type": "string"},
                "nextcloud_account_id": {
                    "type": "string",
                    "description": "Which connected source to create the contact in. Only needed when several offer contacts."
                },
                "addressbook_url": {
                    "type": "string",
                    "description": "Absolute URL of the target addressbook. Omit for the default addressbook."
                }
            }
        })),
        Arc::new(|ctx, args| Box::pin(create_contact(ctx, args))),
    );
}

// ── Handlers ────────────────────────────────────────────────────

async fn search_contacts(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let query = required_str(&args, "query")?;
    let limit = optional_u32(&args, "limit")?
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);

    let contacts = ctx
        .cache
        .search_contacts(&query, limit)
        .map_err(|e| internal(format!("contact search failed: {e}")))?;

    // Sanitized projection: identity and reachability fields only.
    // Photos (bytes), notes, and postal addresses stay in the app.
    let contacts: Vec<Value> = contacts
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "display_name": c.display_name,
                "emails": c.email.iter().map(|e| json!({
                    "kind": e.kind,
                    "value": e.value,
                })).collect::<Vec<_>>(),
                "phones": c.phone.iter().map(|p| json!({
                    "kind": p.kind,
                    "value": p.value,
                })).collect::<Vec<_>>(),
                "organization": c.organization,
                "nextcloud_account_id": c.nextcloud_account_id,
                "addressbook": c.addressbook,
            })
        })
        .collect();

    Ok(json_result(json!({
        "result_count": contacts.len(),
        "contacts": contacts,
    })))
}

async fn create_contact(
    ctx: ToolContext,
    args: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let display_name = required_str(&args, "display_name")?;
    let emails = optional_str_list(&args, "emails")?;
    let phones = optional_str_list(&args, "phones")?;
    let organization = optional_str(&args, "organization")?;
    let note = optional_str(&args, "note")?;

    let account = resolve_nc_account(&ctx, &args, NextcloudFeature::Contacts)?;
    let (addressbook_url, addressbook_name) =
        resolve_addressbook(&account, optional_str(&args, "addressbook_url")?).await?;

    let uid = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let parsed = unkai_carddav::ParsedVcard {
        uid: uid.clone(),
        display_name: display_name.clone(),
        emails: emails
            .iter()
            .map(|value| unkai_carddav::VcardEmail {
                kind: String::new(),
                value: value.clone(),
            })
            .collect(),
        phones: phones
            .iter()
            .map(|value| unkai_carddav::VcardPhone {
                kind: String::new(),
                value: value.clone(),
            })
            .collect(),
        organization: organization.clone(),
        note: note.clone(),
        ..Default::default()
    };
    let vcard = unkai_carddav::build_vcard(&parsed);

    // Local sources have no remote — the cache row below IS the
    // contact; remote sources PUT with `If-None-Match: *` so a UID
    // collision can never overwrite an existing card.
    let outcome = if account.is_local() {
        unkai_carddav::WriteOutcome {
            href: format!("{}/{uid}.vcf", addressbook_url.trim_end_matches('/')),
            etag: uuid::Uuid::new_v4().to_string(),
        }
    } else {
        let app_password = nc_password(&account)?;
        unkai_carddav::create_contact(
            &account.server_url,
            &addressbook_url,
            &account.username,
            &app_password,
            &uid,
            &vcard,
            &account.trusted_certs,
        )
        .await
        .map_err(|e| internal(format!("CardDAV create failed: {e}")))?
    };

    let row = ContactRow {
        href: outcome.href,
        etag: outcome.etag,
        vcard_uid: uid.clone(),
        display_name: display_name.clone(),
        emails: emails
            .into_iter()
            .map(|value| ContactEmail {
                kind: String::new(),
                value,
            })
            .collect(),
        phones: phones
            .into_iter()
            .map(|value| ContactPhone {
                kind: String::new(),
                value,
            })
            .collect(),
        organization,
        photo_mime: None,
        photo_data: None,
        title: None,
        birthday: None,
        note,
        addresses: Vec::new(),
        urls: Vec::new(),
        vcard_raw: vcard,
        kind: String::new(),
        member_uids: Vec::new(),
        categories: Vec::new(),
    };
    ctx.cache
        .upsert_single_contact(&account.id, &addressbook_name, &row)
        .map_err(|e| internal(format!("cache write failed: {e}")))?;

    Ok(json_result(json!({
        "status": "contact_created",
        "contact_id": format!("{}::{uid}", account.id),
        "nextcloud_account_id": account.id,
        "addressbook": addressbook_name,
        "display_name": display_name,
    })))
}

/// Pick the target addressbook: an explicit URL wins; otherwise
/// the source's default — the seeded book for local sources, the
/// server's `contacts` book (or the first listed) for remote ones.
async fn resolve_addressbook(
    account: &NextcloudAccount,
    explicit_url: Option<String>,
) -> Result<(String, String), ErrorData> {
    if let Some(url) = explicit_url {
        let name = url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("contacts")
            .to_string();
        return Ok((url, name));
    }
    if account.is_local() {
        return Ok((
            format!("local://{}/{LOCAL_ADDRESSBOOK_NAME}", account.id),
            LOCAL_ADDRESSBOOK_NAME.to_string(),
        ));
    }
    let app_password = nc_password(account)?;
    let books = unkai_carddav::list_addressbooks_at(
        &carddav_home_of(account),
        &account.username,
        &app_password,
        &account.trusted_certs,
    )
    .await
    .map_err(|e| internal(format!("could not list addressbooks: {e}")))?;
    let book = books
        .iter()
        .find(|b| b.name.eq_ignore_ascii_case("contacts"))
        .or_else(|| books.first())
        .ok_or_else(|| {
            invalid(
                "the server has no addressbook to create the contact in — create one in \
                 Nextcloud Contacts first",
            )
        })?;
    Ok((book.path.clone(), book.name.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nc::test_support::{caps, nc_account};
    use crate::testutil::{invoke, result_payload, test_context};
    use unkai_core::models::DavSourceKind;
    use unkai_store::nextcloud_store;

    fn seed_local_contacts_source(ctx: &crate::registry::ToolContext) {
        nextcloud_store::upsert_account(
            &ctx.cache,
            nc_account("acc", DavSourceKind::Local, Some(caps(false, false, true))),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn create_contact_on_a_local_source_is_searchable() {
        let ctx = test_context();
        seed_local_contacts_source(&ctx);

        let result = invoke(
            &ctx,
            "create_contact",
            json!({
                "display_name": "Jane Smith",
                "emails": ["jane.smith@example.com"],
                "phones": ["+49 30 1234567"],
                "organization": "Firn Labs",
            }),
        )
        .await
        .unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["status"], "contact_created");
        assert_eq!(payload["nextcloud_account_id"], "acc");
        assert_eq!(payload["addressbook"], "local");

        let result = invoke(&ctx, "search_contacts", json!({"query": "jane"}))
            .await
            .unwrap();
        let payload = result_payload(&result);
        assert_eq!(payload["result_count"], 1);
        let contact = &payload["contacts"][0];
        assert_eq!(contact["display_name"], "Jane Smith");
        assert_eq!(contact["emails"][0]["value"], "jane.smith@example.com");
        assert_eq!(contact["organization"], "Firn Labs");
        // Sanitized projection: no photo bytes, no raw vCard.
        assert!(contact.get("photo_data").is_none());
        assert!(contact.get("vcard_raw").is_none());
    }

    #[tokio::test]
    async fn create_contact_requires_a_contacts_capable_source() {
        let ctx = test_context();
        // Connected source has calendars but NOT contacts.
        nextcloud_store::upsert_account(
            &ctx.cache,
            nc_account("acc", DavSourceKind::Local, Some(caps(false, true, false))),
        )
        .unwrap();
        let err = invoke(&ctx, "create_contact", json!({"display_name": "X"}))
            .await
            .expect_err("no contacts-capable source should error");
        assert!(err.message.contains("contacts"));
    }

    #[tokio::test]
    async fn search_contacts_requires_a_query() {
        let ctx = test_context();
        let err = invoke(&ctx, "search_contacts", json!({}))
            .await
            .expect_err("missing query should error");
        assert!(err.message.contains("query"));
    }
}
