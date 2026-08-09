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
/// whether to trim. Handles CDATA, entity/character references, and
/// entity-decoded text.
///
/// Used for leaf elements like `<displayname>foo</displayname>`.
///
/// # Why the `GeneralRef` arm is load-bearing (#479)
///
/// Since quick-xml 0.37, entity and character references inside text
/// (`&#13;`, `&quot;`, `&amp;`, …) are no longer folded into the
/// surrounding `Event::Text` — they arrive as their own
/// `Event::GeneralRef` events. Sabre/Nextcloud encodes the CR of every
/// CRLF line ending inside `<address-data>` / `<calendar-data>` as
/// `&#13;`, so dropping these events silently deletes every line break
/// of every fetched vCard/iCalendar body and the downstream parsers
/// reject the whole record. The readers must also run with text
/// trimming *disabled* (see the sync parsers) — with trimming on, the
/// text fragments *between* two references lose their leading/trailing
/// whitespace, which destroys the `\n` half of the CRLF and the
/// leading space of folded continuation lines.
pub fn read_text_until(
    reader: &mut Reader<&[u8]>,
    start_local: &str,
) -> Result<String, quick_xml::Error> {
    let mut buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(t)) => buf.push_str(&t.xml10_content().unwrap_or_default()),
            Ok(Event::CData(c)) => buf.push_str(&String::from_utf8_lossy(&c)),
            Ok(Event::GeneralRef(r)) => push_general_ref(&mut buf, &r)?,
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

/// Resolve one entity / character reference event into its literal
/// character(s) and append it to `buf`.
///
/// Numeric character references (`&#13;`, `&#x0D;`) resolve to their
/// code point; the five XML predefined entities resolve to their
/// literal. Anything else (an undeclared custom entity — servers we
/// talk to never emit these) is preserved verbatim in `&name;` form so
/// no data is silently lost.
fn push_general_ref(
    buf: &mut String,
    r: &quick_xml::events::BytesRef<'_>,
) -> Result<(), quick_xml::Error> {
    if let Some(ch) = r.resolve_char_ref()? {
        buf.push(ch);
        return Ok(());
    }
    let name = r.decode()?;
    match name.as_ref() {
        "lt" => buf.push('<'),
        "gt" => buf.push('>'),
        "amp" => buf.push('&'),
        "apos" => buf.push('\''),
        "quot" => buf.push('"'),
        other => {
            buf.push('&');
            buf.push_str(other);
            buf.push(';');
        }
    }
    Ok(())
}

/// Read the text of a *scalar* leaf element (an href, etag, status
/// line, sync token, display name, …) and trim surrounding
/// whitespace. Sibling of [`read_text_until`] for the values where
/// incidental XML pretty-printing padding is never meaningful —
/// content elements (`address-data`) keep the raw form, where interior
/// whitespace IS data.
pub fn read_scalar_until(
    reader: &mut Reader<&[u8]>,
    start_local: &str,
) -> Result<String, quick_xml::Error> {
    Ok(read_text_until(reader, start_local)?.trim().to_string())
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
