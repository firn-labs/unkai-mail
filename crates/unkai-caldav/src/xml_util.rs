//! Tiny shared helpers for reading WebDAV multistatus XML.
//!
//! Same shape as `unkai-carddav::xml_util` — the WebDAV multistatus
//! format is identical whether the body is a CardDAV addressbook or a
//! CalDAV calendar response. Copied rather than depended upon so this
//! crate stays standalone (no cross-crate coupling for a 90-line file).
//!
//! # Why ignore namespaces
//!
//! Different servers emit different prefixes for the same elements:
//! one server gives us `<d:multistatus>`, the next `<multistatus>`,
//! a third `<D:multistatus>`. The element's *local name* is what
//! actually identifies it — we strip the prefix and match on that.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// Local name of an element start tag, with any namespace prefix
/// stripped. `<d:href>` → `"href"`. Lower-cased for case-insensitive
/// matching.
pub fn local_name(start: &BytesStart<'_>) -> String {
    let name = start.name();
    let bytes = name.as_ref();
    let local = match bytes.iter().position(|&b| b == b':') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    };
    String::from_utf8_lossy(local).to_ascii_lowercase()
}

/// Local name of an end tag, same stripping rules as `local_name`.
pub fn local_name_end(end: &quick_xml::events::BytesEnd<'_>) -> String {
    let name_owned = end.name();
    let bytes = name_owned.as_ref();
    let local = match bytes.iter().position(|&b| b == b':') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    };
    String::from_utf8_lossy(local).to_ascii_lowercase()
}

/// Read accumulated text content until the matching end tag for
/// `start_local`. Handles CDATA (where servers stash raw iCalendar
/// bodies) and entity-decoded text.
pub fn read_text_until(
    reader: &mut Reader<&[u8]>,
    start_local: &str,
) -> Result<String, quick_xml::Error> {
    let mut buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(t)) => buf.push_str(&t.xml10_content().unwrap_or_default()),
            Ok(Event::CData(c)) => buf.push_str(&String::from_utf8_lossy(&c)),
            Ok(Event::End(end)) if local_name_end(&end).eq_ignore_ascii_case(start_local) => {
                return Ok(buf);
            }
            Ok(Event::Eof) => return Ok(buf),
            Err(e) => return Err(e),
            _ => {}
        }
    }
}

/// Skip past a subtree, consuming events until the matching close tag.
/// Used to drop branches we don't care about.
pub fn skip_subtree(reader: &mut Reader<&[u8]>, start_local: &str) -> Result<(), quick_xml::Error> {
    let mut depth = 1;
    loop {
        match reader.read_event() {
            Ok(Event::Start(s)) if local_name(&s) == start_local => {
                depth += 1;
            }
            Ok(Event::End(e)) if local_name_end(&e).eq_ignore_ascii_case(start_local) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            Ok(Event::Eof) => return Ok(()),
            Err(e) => return Err(e),
            _ => {}
        }
    }
}
