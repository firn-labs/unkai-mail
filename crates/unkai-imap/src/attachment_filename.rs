//! Robust attachment-filename decoding (#329 follow-up).
//!
//! `mail-parser`'s [`attachment_name`][1] handles RFC 2047 encoded
//! words and properly-tagged RFC 2231 parameters out of the box, but
//! falls through to `String::from_utf8_lossy` for senders that put
//! raw 8-bit bytes directly in a `filename=` parameter — and
//! `from_utf8_lossy` replaces every invalid byte with U+FFFD, the
//! replacement character (which most fonts render as `?`).  That's
//! the failure mode behind real-world filenames showing up as
//! `Preis??nde.pdf` instead of `Preisände.pdf` when the sender's
//! client is sloppy.
//!
//! Two patterns produce *recoverable* U+FFFD here:
//!
//! 1. Sender wrote raw Latin-1 / Windows-1252 bytes in the parameter
//!    (e.g. `filename=Preis\xE4nde.pdf` for `ä = 0xE4`).  Each
//!    non-ASCII byte is invalid UTF-8 on its own → one U+FFFD per
//!    affected character.
//! 2. Sender used RFC 2231 continuation (`filename*0`, `filename*1`,
//!    …) to split the value *without* the leading charset wrapper
//!    (`filename*=UTF-8''…`), and the split landed mid-multibyte —
//!    e.g. UTF-8 `ä = 0xC3 0xA4` split as `0xC3` at the end of one
//!    continuation and `0xA4` at the start of the next.  mail-parser
//!    decodes each continuation as UTF-8 individually before
//!    concatenating, so each half becomes its own U+FFFD: two in a
//!    row where the multibyte boundary was crossed.
//!
//! There's a third pattern we *can't* recover: senders whose own
//! software replaced the character with U+FFFD before constructing
//! the MIME header, so the wire bytes already carry the replacement
//! chars baked into a properly-RFC-2231-encoded value (e.g.
//! `filename*="UTF-8''Preis%EF%BF%BD%EF%BF%BDnderung.pdf"`).  Once
//! the source character has been lost upstream, no parsing layer
//! can guess what it was.  Confirmed in the wild during #329 test;
//! see `tests::pre_corrupted_ufffd_preserved_faithfully` for the
//! lock-in behaviour: we surface the U+FFFD chars unchanged, which
//! keeps the filename truthful (and keeps the on-disk name the user
//! gets from "Download" matching the one shown in the chip).
//!
//! Recovery strategy for the two recoverable patterns:
//!
//! - Trust mail-parser's result when it doesn't contain U+FFFD —
//!   that path is correct for every RFC-compliant encoding plus the
//!   common case of raw UTF-8 bytes that happen to be valid UTF-8.
//! - When U+FFFD shows up, re-extract the raw bytes from the
//!   `Content-Disposition` / `Content-Type` header by indexing into
//!   `Message::raw_message` via the per-header `offset_start /
//!   offset_end`, concatenate any RFC 2231 continuations into a
//!   single byte buffer, then try UTF-8 first (recovers the split-
//!   multibyte case) and fall back to bytes-as-Latin-1 (recovers
//!   the raw-Latin-1 case).  The RFC 2231 charset-tagged shapes
//!   (`filename*=charset''…`, `filename*0*=charset''…`) are mail-
//!   parser's territory; we don't attempt to re-parse them, so the
//!   pre-corrupted case above falls through to the U+FFFD-preserving
//!   final branch.
//!
//! Bytes-as-Latin-1 — each byte cast to `char` — maps 1:1 onto
//! U+0000..=U+00FF.  Latin-1 and Windows-1252 disagree only at
//! 0x80..0x9F (CP1252 has typography glyphs there; Latin-1 has C1
//! control codes); none of those bytes appear in real-world German
//! filenames, so the simpler conversion is sufficient and avoids
//! pulling in a charset crate.
//!
//! [1]: https://docs.rs/mail-parser/0.11.3/mail_parser/trait.MessagePart.html#method.attachment_name

use mail_parser::{Message, MessagePart, MimeHeaders};

/// Decode the attachment filename for a MIME part, recovering from
/// mail-parser's lossy UTF-8 fallback when the sender put raw 8-bit
/// bytes in `filename=` / `name=` directly.  Returns `"attachment"`
/// when the part doesn't carry either parameter — same fallback the
/// previous in-line `.unwrap_or_else(|| "attachment".to_string())`
/// produced.
pub fn decode_attachment_filename(parsed: &Message<'_>, part: &MessagePart<'_>) -> String {
    if let Some(name) = part.attachment_name() {
        if !name.contains('\u{FFFD}') {
            return name.to_string();
        }
        if let Some(recovered) = recover_from_raw_headers(parsed, part) {
            return recovered;
        }
        // No recoverable bytes — either the headers carry the
        // U+FFFD already encoded (sender-side corruption past our
        // reach) or we couldn't reach the raw header slice.  Return
        // mail-parser's string as-is rather than fabricating
        // "attachment"; the user can still recognise most of the
        // name and the on-disk name from "Download" stays
        // consistent with what the chip shows.
        return name.to_string();
    }
    "attachment".to_string()
}

/// Walk the part's headers, locate Content-Disposition or
/// Content-Type, pull the raw byte slice for each via the offsets
/// recorded by mail-parser, and re-parse the filename / name
/// parameter at byte level so non-UTF-8 senders survive.
fn recover_from_raw_headers(parsed: &Message<'_>, part: &MessagePart<'_>) -> Option<String> {
    let raw = parsed.raw_message.as_ref();
    // Content-Disposition's `filename` takes precedence over
    // Content-Type's `name`, same priority order mail-parser uses
    // in its own `attachment_name` impl.
    let candidates: &[(&str, &[u8])] = &[
        ("Content-Disposition", b"filename"),
        ("Content-Type", b"name"),
    ];
    for (header_name, param_name) in candidates {
        for header in &part.headers {
            if !header.name.as_str().eq_ignore_ascii_case(header_name) {
                continue;
            }
            let bytes = raw.get(header.offset_start as usize..header.offset_end as usize)?;
            if let Some(value) = extract_param_value_bytes(bytes, param_name) {
                return Some(decode_value_bytes(&value));
            }
        }
    }
    None
}

/// Decode a concatenated parameter-value byte buffer.  UTF-8 first —
/// this rescues the split-multibyte-continuation case, where
/// concatenating bytes before decoding makes the multibyte sequence
/// whole again.  Latin-1 (byte → codepoint) as the fallback for
/// senders that wrote raw 8-bit bytes outright.
fn decode_value_bytes(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    bytes.iter().map(|&b| b as char).collect()
}

/// Pull the value bytes for `param` out of a raw MIME header.
///
/// Handles three real-world shapes:
///
/// - `param=value` (unquoted token)
/// - `param="value"` (quoted-string with `\` escapes)
/// - `param*0=value0; param*1=value1; …` (RFC 2231 continuation
///   without the charset wrapper — the *only* RFC 2231 shape that
///   reaches this path, because mail-parser handles the charset-
///   tagged `param*=charset''value` and `param*0*=charset''…`
///   shapes correctly and they wouldn't have produced U+FFFD).
///
/// Concatenates continuations in numeric order so a multibyte
/// character split across the boundary is re-fused before the
/// caller tries UTF-8 decoding on it.
fn extract_param_value_bytes(header_bytes: &[u8], param: &[u8]) -> Option<Vec<u8>> {
    let mut continuations: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut found_plain: Option<Vec<u8>> = None;

    // Skip past the header's main value (e.g. `attachment` for
    // Content-Disposition, `application/pdf` for Content-Type) to
    // the first parameter separator.
    let mut i = 0;
    while i < header_bytes.len() && header_bytes[i] != b';' {
        i += 1;
    }

    while i < header_bytes.len() {
        // Skip `;` separators and any whitespace (including folded
        // CRLF + space that wraps long headers).
        while i < header_bytes.len()
            && (header_bytes[i] == b';' || header_bytes[i].is_ascii_whitespace())
        {
            i += 1;
        }
        if i >= header_bytes.len() {
            break;
        }

        // Read the parameter name (token: anything up to whitespace,
        // `=`, or `;`).
        let key_start = i;
        while i < header_bytes.len()
            && header_bytes[i] != b'='
            && header_bytes[i] != b';'
            && !header_bytes[i].is_ascii_whitespace()
        {
            i += 1;
        }
        let key = &header_bytes[key_start..i];

        // Tolerate whitespace around the `=`.
        while i < header_bytes.len() && header_bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= header_bytes.len() || header_bytes[i] != b'=' {
            // Malformed segment — skip to next `;` and continue.
            while i < header_bytes.len() && header_bytes[i] != b';' {
                i += 1;
            }
            continue;
        }
        i += 1; // skip `=`
        while i < header_bytes.len() && header_bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        // Read the value — quoted-string or token.
        let mut value: Vec<u8> = Vec::new();
        if i < header_bytes.len() && header_bytes[i] == b'"' {
            i += 1;
            while i < header_bytes.len() && header_bytes[i] != b'"' {
                if header_bytes[i] == b'\\' && i + 1 < header_bytes.len() {
                    value.push(header_bytes[i + 1]);
                    i += 2;
                } else {
                    value.push(header_bytes[i]);
                    i += 1;
                }
            }
            if i < header_bytes.len() {
                i += 1; // closing `"`
            }
        } else {
            while i < header_bytes.len()
                && header_bytes[i] != b';'
                && !header_bytes[i].is_ascii_whitespace()
            {
                value.push(header_bytes[i]);
                i += 1;
            }
        }

        // Classify the key against the param we're looking for.
        // `filename` / `name` plain, or `filename*N` / `name*N`
        // continuation.  The `*` (charset-tagged single) and `*N*`
        // (continuation with first-segment charset) shapes are
        // mail-parser's territory and would not have reached this
        // recovery path — skip them.
        if key.eq_ignore_ascii_case(param) {
            found_plain = Some(value);
            continue;
        }
        if key.len() <= param.len() || !key[..param.len()].eq_ignore_ascii_case(param) {
            continue;
        }
        let suffix = &key[param.len()..];
        if suffix == b"*" || suffix.last() == Some(&b'*') {
            continue;
        }
        if !suffix.starts_with(b"*") {
            continue;
        }
        if let Ok(idx_str) = std::str::from_utf8(&suffix[1..])
            && let Ok(idx) = idx_str.parse::<u32>()
        {
            continuations.push((idx, value));
        }
    }

    if !continuations.is_empty() {
        continuations.sort_by_key(|(n, _)| *n);
        let mut joined = Vec::new();
        for (_, v) in continuations {
            joined.extend_from_slice(&v);
        }
        return Some(joined);
    }
    found_plain
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::MessageParser;

    // 0xE4 is `ä` in Latin-1 / Windows-1252; the same character in
    // UTF-8 is 0xC3 0xA4.  These tests construct synthetic .eml
    // bodies with each encoding and confirm the helper recovers
    // the German `Preisände.pdf` filename in every case.

    fn build_eml(filename_header_line: &[u8]) -> Vec<u8> {
        let mut eml: Vec<u8> = Vec::new();
        eml.extend_from_slice(b"From: a@b.example\r\n");
        eml.extend_from_slice(b"To: c@d.example\r\n");
        eml.extend_from_slice(b"Subject: t\r\n");
        eml.extend_from_slice(b"MIME-Version: 1.0\r\n");
        eml.extend_from_slice(b"Content-Type: multipart/mixed; boundary=BNDR\r\n");
        eml.extend_from_slice(b"\r\n");
        eml.extend_from_slice(b"--BNDR\r\n");
        eml.extend_from_slice(b"Content-Type: text/plain\r\n\r\nhi\r\n");
        eml.extend_from_slice(b"--BNDR\r\n");
        eml.extend_from_slice(b"Content-Type: application/pdf\r\n");
        eml.extend_from_slice(filename_header_line);
        eml.extend_from_slice(b"\r\n");
        eml.extend_from_slice(b"Content-Transfer-Encoding: base64\r\n\r\n");
        eml.extend_from_slice(b"aGVsbG8=\r\n");
        eml.extend_from_slice(b"--BNDR--\r\n");
        eml
    }

    fn decoded_attachment_name(eml: &[u8]) -> String {
        let msg = MessageParser::default().parse(eml).expect("parse");
        let part = msg.attachments().next().expect("attachment");
        decode_attachment_filename(&msg, part)
    }

    #[test]
    fn plain_ascii_filename_unchanged() {
        let eml = build_eml(b"Content-Disposition: attachment; filename=\"plain.pdf\"");
        assert_eq!(decoded_attachment_name(&eml), "plain.pdf");
    }

    #[test]
    fn raw_utf8_bytes_decoded_via_mail_parser() {
        // `ä` as raw UTF-8 (0xC3 0xA4) is *valid* UTF-8 — mail-parser
        // hands us the right string directly without our fallback
        // ever firing.
        let mut header = b"Content-Disposition: attachment; filename=\"Preis".to_vec();
        header.extend_from_slice(b"\xC3\xA4");
        header.extend_from_slice(b"nde.pdf\"");
        let eml = build_eml(&header);
        assert_eq!(decoded_attachment_name(&eml), "Preisände.pdf");
    }

    #[test]
    fn raw_latin1_bytes_recovered_via_fallback() {
        // `ä` as raw Latin-1 (single byte 0xE4) is *invalid* UTF-8 —
        // mail-parser produces "Preis\u{FFFD}nde.pdf", our fallback
        // re-reads the header bytes and decodes them as Latin-1.
        let mut header = b"Content-Disposition: attachment; filename=\"Preis".to_vec();
        header.push(0xE4);
        header.extend_from_slice(b"nde.pdf\"");
        let eml = build_eml(&header);
        assert_eq!(decoded_attachment_name(&eml), "Preisände.pdf");
    }

    #[test]
    fn raw_latin1_bytes_unquoted_recovered() {
        let mut header = b"Content-Disposition: attachment; filename=Preis".to_vec();
        header.push(0xE4);
        header.extend_from_slice(b"nde.pdf");
        let eml = build_eml(&header);
        assert_eq!(decoded_attachment_name(&eml), "Preisände.pdf");
    }

    #[test]
    fn pre_corrupted_ufffd_preserved_faithfully() {
        // Real-world case (#329 testing): the sender's mail client
        // replaced `ä` with two U+FFFD chars *before* RFC-2231-
        // encoding the filename, so the wire bytes already carry
        // `%EF%BF%BD%EF%BF%BD` inside a properly charset-tagged
        // value.  mail-parser correctly decodes that to two U+FFFD;
        // recovery from raw bytes would return the same U+FFFD
        // because the source character is gone.  We lock the
        // faithful-pass-through behaviour: surface what came in,
        // don't guess, don't fabricate, don't drop to "attachment".
        let header = b"Content-Disposition: attachment;\r\n\
            \tfilename*=\"UTF-8''Preis%EF%BF%BD%EF%BF%BDnderung.pdf\"";
        let eml = build_eml(header);
        let expected = format!("Preis{}{}nderung.pdf", '\u{FFFD}', '\u{FFFD}');
        assert_eq!(decoded_attachment_name(&eml), expected);
    }

    #[test]
    fn rfc2231_split_multibyte_recovered() {
        // RFC 2231 continuation that splits the UTF-8 bytes of `ä`
        // (0xC3 0xA4) across two segments without the charset
        // wrapper.  mail-parser decodes each segment as UTF-8 on
        // its own, producing two U+FFFD where the split lands; our
        // fallback concatenates the raw bytes and UTF-8-decodes
        // the whole buffer to recover the character.
        let mut header = b"Content-Disposition: attachment; filename*0=\"Preis".to_vec();
        header.push(0xC3);
        header.extend_from_slice(b"\"; filename*1=\"");
        header.push(0xA4);
        header.extend_from_slice(b"nde.pdf\"");
        let eml = build_eml(&header);
        assert_eq!(decoded_attachment_name(&eml), "Preisände.pdf");
    }

    #[test]
    fn content_type_name_fallback() {
        // When only Content-Type carries the filename via the
        // legacy `name=` parameter and the bytes are raw Latin-1,
        // the fallback finds them via the second candidate header.
        let mut eml: Vec<u8> = Vec::new();
        eml.extend_from_slice(b"From: a@b.example\r\n");
        eml.extend_from_slice(b"To: c@d.example\r\n");
        eml.extend_from_slice(b"Subject: t\r\n");
        eml.extend_from_slice(b"MIME-Version: 1.0\r\n");
        eml.extend_from_slice(b"Content-Type: multipart/mixed; boundary=BNDR\r\n\r\n");
        eml.extend_from_slice(b"--BNDR\r\n");
        eml.extend_from_slice(b"Content-Type: text/plain\r\n\r\nhi\r\n");
        eml.extend_from_slice(b"--BNDR\r\n");
        eml.extend_from_slice(b"Content-Type: application/pdf; name=\"Preis");
        eml.push(0xE4);
        eml.extend_from_slice(b"nde.pdf\"\r\n");
        eml.extend_from_slice(b"Content-Disposition: attachment\r\n");
        eml.extend_from_slice(b"Content-Transfer-Encoding: base64\r\n\r\n");
        eml.extend_from_slice(b"aGVsbG8=\r\n");
        eml.extend_from_slice(b"--BNDR--\r\n");
        assert_eq!(decoded_attachment_name(&eml), "Preisände.pdf");
    }

    #[test]
    fn no_filename_returns_attachment_sentinel() {
        let mut eml: Vec<u8> = Vec::new();
        eml.extend_from_slice(b"From: a@b.example\r\n");
        eml.extend_from_slice(b"To: c@d.example\r\n");
        eml.extend_from_slice(b"Subject: t\r\n");
        eml.extend_from_slice(b"MIME-Version: 1.0\r\n");
        eml.extend_from_slice(b"Content-Type: multipart/mixed; boundary=BNDR\r\n\r\n");
        eml.extend_from_slice(b"--BNDR\r\n");
        eml.extend_from_slice(b"Content-Type: text/plain\r\n\r\nhi\r\n");
        eml.extend_from_slice(b"--BNDR\r\n");
        eml.extend_from_slice(b"Content-Type: application/pdf\r\n");
        eml.extend_from_slice(b"Content-Disposition: attachment\r\n");
        eml.extend_from_slice(b"Content-Transfer-Encoding: base64\r\n\r\n");
        eml.extend_from_slice(b"aGVsbG8=\r\n");
        eml.extend_from_slice(b"--BNDR--\r\n");
        assert_eq!(decoded_attachment_name(&eml), "attachment");
    }
}
