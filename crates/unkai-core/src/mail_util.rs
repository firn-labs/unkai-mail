//! Small protocol-adjacent helpers shared between the Tauri
//! command layer and the MCP mail tools (#440).
//!
//! Both live here (rather than in `unkai-imap` / `unkai-smtp`)
//! because they are pure functions over core models and raw
//! bytes — extracting them avoided giving `unkai-mcp` a copy of
//! logic `src-tauri` already had.

use crate::models::Folder;

/// Pick the most likely Drafts folder from a cached folder list.
///
/// Prefers folders flagged with the IMAP `\Drafts` special-use
/// attribute (the canonical, locale-independent answer) and falls
/// back to common localized folder names.
pub fn pick_drafts_folder(folders: &[Folder]) -> Option<String> {
    if let Some(by_attr) = folders.iter().find(|f| {
        f.attributes
            .iter()
            .any(|a| a.eq_ignore_ascii_case("drafts") || a.eq_ignore_ascii_case("\\drafts"))
    }) {
        return Some(by_attr.name.clone());
    }

    const NAME_HINTS: &[&str] = &[
        "drafts",
        "draft",
        "entwürfe",
        "entwurf",
        "brouillons",
        "brouillon",
    ];
    folders
        .iter()
        .find(|f| {
            let lower = f.name.to_lowercase();
            NAME_HINTS.iter().any(|h| lower.contains(h))
        })
        .map(|f| f.name.clone())
}

/// Pull the `Message-ID` header value out of a raw RFC 822 message.
///
/// Returns the bare bracketed form (e.g. `<uuid@host>`) so the
/// caller can hand it straight to `find_uid_by_message_id`, which
/// SEARCHes on the literal header value the IMAP server stored.
///
/// Tolerant of casing variants (`Message-ID:` / `Message-Id:` /
/// `message-id:`) since RFC 5322 header field names are case-
/// insensitive. Folded continuation lines aren't expected for
/// Message-ID values (lettre emits a single short line) but the
/// scanner stops at the first match and bails on the first blank
/// line, which is the conventional header/body separator.
pub fn extract_message_id(raw: &[u8]) -> Option<String> {
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&raw[..header_end]).ok()?;
    for line in headers.split("\r\n") {
        let prefix_len = if line.len() >= "Message-ID:".len()
            && line[..="Message-ID:".len() - 1].eq_ignore_ascii_case("Message-ID:")
        {
            "Message-ID:".len()
        } else {
            continue;
        };
        let value = line[prefix_len..].trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, attributes: &[&str]) -> Folder {
        Folder {
            name: name.into(),
            delimiter: Some("/".into()),
            attributes: attributes.iter().map(|a| a.to_string()).collect(),
            unread_count: None,
        }
    }

    #[test]
    fn drafts_by_special_use_attribute_wins_over_name() {
        let folders = [folder("Drafts", &[]), folder("Oddly-Named", &["\\Drafts"])];
        assert_eq!(pick_drafts_folder(&folders).as_deref(), Some("Oddly-Named"));
    }

    #[test]
    fn drafts_by_localized_name_fallback() {
        let folders = [folder("INBOX", &[]), folder("Entwürfe", &[])];
        assert_eq!(pick_drafts_folder(&folders).as_deref(), Some("Entwürfe"));
        assert_eq!(pick_drafts_folder(&[folder("INBOX", &[])]), None);
    }

    #[test]
    fn message_id_extraction_is_case_insensitive_and_stops_at_body() {
        let raw = b"Subject: hi\r\nmessage-id: <abc@example.com>\r\n\r\nMessage-ID: <not-this@example.com>\r\n";
        assert_eq!(
            extract_message_id(raw).as_deref(),
            Some("<abc@example.com>")
        );
        assert_eq!(extract_message_id(b"Subject: no id\r\n\r\nbody"), None);
    }
}
