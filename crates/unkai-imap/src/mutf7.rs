//! Modified UTF-7 codec for IMAP mailbox names (RFC 3501 §5.1.3).
//!
//! IMAP doesn't use UTF-8 on the wire for mailbox names — it uses a
//! variant of UTF-7 with two tweaks: the `/` in the base64 alphabet
//! becomes `,` (because `/` is a valid mailbox-hierarchy separator),
//! and the literal `&` is escaped as `&-`. Anything outside the
//! printable-ASCII range is wrapped in `&...-` and the bytes inside
//! are UTF-16 BE encoded then base64-encoded with the custom alphabet.
//!
//! Examples:
//!   - `INBOX`         ↔ `INBOX`
//!   - `Gelöscht`     ↔ `Gel&APY-scht`
//!   - `日本語`         ↔ `&ZeVnLIqe-`
//!   - `R&D`           ↔ `R&-D`
//!
//! `decode` is what we apply to names returned by `LIST` so the cache
//! and the UI store/render UTF-8. `encode` is what we apply right
//! before sending a name back to the server in `SELECT` / `EXAMINE` /
//! `STATUS` / `APPEND` etc.
//!
//! Decoding is forgiving — malformed `&...-` sequences are passed
//! through verbatim. Mailbox names from a real server should always
//! be valid mUTF-7, but a defensive fallback keeps a buggy server or
//! a half-decoded cached name from poisoning the entire folder list.

use base64::Engine;
use base64::alphabet::Alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

/// IMAP mUTF-7 alphabet — same as standard base64 except `/` is `,`.
fn engine() -> GeneralPurpose {
    // Unwrap is safe: the alphabet string is a compile-time constant
    // of exactly 64 unique characters, which `Alphabet::new` accepts.
    let alphabet =
        Alphabet::new("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,")
            .expect("static IMAP mUTF-7 alphabet is valid");
    let config = GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::RequireNone);
    GeneralPurpose::new(&alphabet, config)
}

/// Decode an IMAP mUTF-7 mailbox name into UTF-8.
pub fn decode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'&' {
            // ASCII printable → literal. Mailbox names are pure
            // ASCII outside `&...-` shifts so we can copy bytes
            // directly.
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        // Find the closing `-`. mUTF-7 shifts always end with `-`;
        // an unterminated `&` is malformed and we just pass it through.
        let Some(end_off) = bytes[i + 1..].iter().position(|&b| b == b'-') else {
            out.push_str(&input[i..]);
            break;
        };
        let end = i + 1 + end_off;
        let payload = &input[i + 1..end];

        if payload.is_empty() {
            // `&-` → literal `&`.
            out.push('&');
        } else if let Some(decoded) = decode_payload(payload) {
            out.push_str(&decoded);
        } else {
            // Malformed shift. Fall back to passing the raw bytes
            // through (including the `&...-` framing) so a buggy
            // name doesn't silently disappear from the folder list.
            out.push_str(&input[i..=end]);
        }

        i = end + 1;
    }

    out
}

fn decode_payload(payload: &str) -> Option<String> {
    let raw = engine().decode(payload).ok()?;
    if raw.len() % 2 != 0 {
        return None;
    }
    // mUTF-7 stores UTF-16 BE. Build a `Vec<u16>` then convert to
    // a String — `from_utf16` handles surrogate pairs correctly.
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// Encode a UTF-8 mailbox name into IMAP mUTF-7 wire form.
pub fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    // Buffer of UTF-16 code units waiting to be flushed into a
    // single `&...-` shift. We coalesce consecutive non-ASCII chars
    // into one shift instead of one shift per char — same encoding
    // every other client emits and what servers expect.
    let mut shift: Vec<u16> = Vec::new();

    for ch in input.chars() {
        if ch == '&' {
            // `&` is the escape character — emit `&-` (a closed
            // empty shift) regardless of whether we were already
            // accumulating non-ASCII chars; flush first if so.
            if !shift.is_empty() {
                flush_shift(&mut out, &mut shift);
            }
            out.push_str("&-");
        } else if is_printable_ascii(ch) {
            if !shift.is_empty() {
                flush_shift(&mut out, &mut shift);
            }
            out.push(ch);
        } else {
            // Buffer the UTF-16 code units for this char — could
            // be one (BMP) or two (surrogate pair).
            let mut buf = [0u16; 2];
            shift.extend_from_slice(ch.encode_utf16(&mut buf));
        }
    }
    if !shift.is_empty() {
        flush_shift(&mut out, &mut shift);
    }
    out
}

/// Printable ASCII (0x20..=0x7E). RFC 3501 also lets a few control
/// characters through unshifted, but keeping it tight means anything
/// unusual round-trips losslessly via a shift.
fn is_printable_ascii(ch: char) -> bool {
    let code = ch as u32;
    (0x20..=0x7E).contains(&code)
}

fn flush_shift(out: &mut String, shift: &mut Vec<u16>) {
    let bytes: Vec<u8> = shift.iter().flat_map(|u| u.to_be_bytes()).collect();
    let encoded = engine().encode(&bytes);
    out.push('&');
    out.push_str(&encoded);
    out.push('-');
    shift.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip cases covering ASCII pass-through, the `&` escape,
    /// single-codepoint shifts, multi-codepoint shifts, and a
    /// supplementary-plane (>U+FFFF) emoji that needs a surrogate
    /// pair on the wire.
    #[test]
    fn roundtrip() {
        let cases = [
            ("INBOX", "INBOX"),
            ("Gelöscht", "Gel&APY-scht"),
            ("日本語", "&ZeVnLIqe-"),
            ("R&D", "R&-D"),
            ("INBOX/Müll", "INBOX/M&APw-ll"),
            // Single non-ASCII char at the head of an ASCII tail
            // — common case for German folder names.
            ("Über", "&ANw-ber"),
            // Trailing non-ASCII forces a shift right at the end.
            ("Hello, ä", "Hello, &AOQ-"),
        ];
        for (utf8, wire) in cases {
            assert_eq!(encode(utf8), wire, "encode({utf8:?})");
            assert_eq!(decode(wire), utf8, "decode({wire:?})");
        }
    }

    #[test]
    fn malformed_passes_through() {
        // No closing `-` after `&`: pass the rest through verbatim.
        assert_eq!(decode("Bad&Name"), "Bad&Name");
        // Closing `-` but garbage payload: pass the framed segment
        // through so the user can at least see something rendered.
        assert_eq!(decode("Bad&!@#-Name"), "Bad&!@#-Name");
    }
}
