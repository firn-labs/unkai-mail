//! Tiny shared helpers for reading WebDAV multistatus XML.
//!
//! # Why ignore namespaces
//!
//! Different servers emit different prefixes for the same elements:
//! one server gives us `<d:multistatus>`, the next `<multistatus>`,
//! a third `<D:multistatus>`. The element's *local name* is what
//! actually identifies it. We strip the prefix and match on that —
//! lossy on paper, robust in practice.
//!
//! # Why event-driven, not serde
//!
//! `quick-xml`'s serde adapter doesn't handle XML namespaces or the
//! mixed-content propstat shape cleanly. The event reader does, and
//! these documents are small enough that walking them by hand is
//! fewer lines than coaxing serde into the right struct layout.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// Local name of an element start tag, with any namespace prefix
/// stripped. `<d:href>` → `"href"`. Lower-cased for case-insensitive
/// matching, since some servers shout `<DAV:>` prefixes.
pub fn local_name(start: &BytesStart<'_>) -> String {
    let name = start.name();
    let bytes = name.as_ref();
    let local = match bytes.iter().position(|&b| b == b':') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    };
    String::from_utf8_lossy(local).to_ascii_lowercase()
}

/// Read accumulated text content until the matching end tag for
/// `start_local`. Returns the raw concatenated text — caller decides
/// whether to trim. Handles CDATA and entity-decoded text.
///
/// Used for leaf elements like `<displayname>foo</displayname>`.
pub fn read_text_until(
    reader: &mut Reader<&[u8]>,
    start_local: &str,
) -> Result<String, quick_xml::Error> {
    let mut buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(t)) => buf.push_str(&t.unescape().unwrap_or_default()),
            Ok(Event::CData(c)) => buf.push_str(&String::from_utf8_lossy(&c)),
            Ok(Event::End(end)) => {
                let bytes = end.name();
                let name_bytes = bytes.as_ref();
                let local = match name_bytes.iter().position(|&b| b == b':') {
                    Some(i) => &name_bytes[i + 1..],
                    None => name_bytes,
                };
                if String::from_utf8_lossy(local).eq_ignore_ascii_case(start_local) {
                    return Ok(buf);
                }
            }
            Ok(Event::Eof) => return Ok(buf),
            Err(e) => return Err(e),
            _ => {}
        }
    }
}

/// Skip past a subtree, consuming events until the matching close tag.
/// Used to drop branches we don't care about (e.g. unknown propstats).
pub fn skip_subtree(reader: &mut Reader<&[u8]>, start_local: &str) -> Result<(), quick_xml::Error> {
    let mut depth = 1;
    loop {
        match reader.read_event() {
            Ok(Event::Start(s)) if local_name(&s) == start_local => {
                depth += 1;
            }
            Ok(Event::End(e)) => {
                let bytes = e.name();
                let name_bytes = bytes.as_ref();
                let local = match name_bytes.iter().position(|&b| b == b':') {
                    Some(i) => &name_bytes[i + 1..],
                    None => name_bytes,
                };
                if String::from_utf8_lossy(local).eq_ignore_ascii_case(start_local) {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
            }
            Ok(Event::Eof) => return Ok(()),
            Err(e) => return Err(e),
            _ => {}
        }
    }
}
