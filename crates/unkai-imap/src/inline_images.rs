//! Inline body images (#471) — the MIME parts an HTML body pulls in
//! with `<img src="cid:…">`.
//!
//! RFC 2392 lets a message reference one of its own MIME parts from
//! the HTML body by Content-ID: the sender stamps
//! `Content-ID: <logo@example>` on an image part and writes
//! `<img src="cid:logo@example">` in the body.  The reading pane can
//! only render those once it holds the part's bytes, so this module
//! pulls every referenceable image out of a raw message in one pass.
//!
//! Why a bulk extractor instead of reusing `fetch_attachment` per
//! image: that path opens an IMAP connection and re-FETCHes the whole
//! message *per part*.  A newsletter with a dozen inline images would
//! pay a dozen connections and a dozen full-message downloads to
//! paint one body.  Here the caller fetches the raw message once and
//! every inline part falls out of the same parse.
//!
//! Part-id indexing matches [`crate::ImapClient::fetch_attachment`]
//! exactly (primary: the `attachments()` iterator order; fallback:
//! the flat `parts` array), so an `InlineImage.part_id` addresses the
//! same bytes as the equally-indexed `EmailAttachment.part_id` the
//! listing path stamped.  That keeps a cid image and its attachment
//! chip pointing at one part.

use mail_parser::{Message, MessageParser, MessagePart, MimeHeaders};
use tracing::warn;
use unkai_core::crypto::CryptoBridge;
use unkai_core::error::UnkaiError;

use crate::attachment_filename::decode_attachment_filename;

/// Largest single inline image we ship to the renderer.  Anything
/// bigger is almost certainly a full-size photo the sender attached
/// rather than page furniture, and inlining it would push a multi-
/// megabyte base64 string through the IPC boundary for one `<img>`.
/// Oversized parts stay reachable through the normal attachment
/// chip (download / preview), they just don't render in the body.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Ceiling across all inline images of one message, so a message
/// carrying fifty just-under-the-limit images can't blow the
/// renderer's memory.  Parts are taken in document order until the
/// budget is spent.
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// One image part that the HTML body may reference by `cid:`.
#[derive(Debug, Clone)]
pub struct InlineImage {
    /// Same index space as `EmailAttachment.part_id` — see the module
    /// docs on why the two have to agree.
    pub part_id: u32,
    /// RFC 2392 Content-ID without the angle brackets (mail-parser
    /// strips them).  `None` for a part that only declared
    /// `Content-Disposition: inline`; the renderer can still match
    /// those by filename.
    pub content_id: Option<String>,
    pub filename: String,
    /// `type/subtype`, e.g. `image/png` — becomes the Blob's MIME
    /// type on the renderer side.
    pub content_type: String,
    /// Transfer-decoded bytes (mail-parser has already resolved
    /// base64 / quoted-printable by the time we read `contents()`).
    pub bytes: Vec<u8>,
}

/// Pull every inline-referenceable image out of a raw RFC 5322
/// message.
///
/// A part qualifies when its Content-Type is `image/*` **and** it
/// either carries a Content-ID (so the body can name it) or declares
/// `Content-Disposition: inline` (senders that reference the part by
/// filename instead — rare, but cheap to support since the renderer
/// falls back to filename matching anyway).
///
/// A plain photo attachment — `image/jpeg` with
/// `Content-Disposition: attachment` and no Content-ID — is
/// deliberately *not* returned: nothing in the body can reference it,
/// so shipping its bytes would just inflate the response.
pub fn collect_inline_images(raw: &[u8]) -> Result<Vec<InlineImage>, UnkaiError> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| UnkaiError::Protocol("Failed to parse message".into()))?;
    Ok(collect_from_parsed(&parsed))
}

/// The encrypted counterpart to [`collect_inline_images`], mirroring
/// [`crate::extract_decrypted_attachment`]: decrypt the envelope
/// first, then walk the *inner* MIME tree, because a PGP/MIME or
/// S/MIME message's outer envelope carries no image parts at all —
/// the body and everything it references live inside the ciphertext.
///
/// Returns `Ok(None)` when `raw` isn't an encrypted envelope of
/// either stack (including clear-signed `multipart/signed`, whose
/// parts are already readable), so the caller can fall back to the
/// plaintext path.
pub fn extract_decrypted_inline_images(
    raw: &[u8],
    bridge: &dyn CryptoBridge,
) -> Result<Option<Vec<InlineImage>>, UnkaiError> {
    let Some(plaintext) = crate::client::decrypt_envelope_plaintext(raw, bridge)? else {
        return Ok(None);
    };
    let parsed = MessageParser::default()
        .parse(plaintext.as_slice())
        .ok_or_else(|| UnkaiError::Protocol("Failed to parse decrypted message".into()))?;
    Ok(Some(collect_from_parsed(&parsed)))
}

/// Shared walk over an already-parsed message.
fn collect_from_parsed(parsed: &Message<'_>) -> Vec<InlineImage> {
    let mut out: Vec<InlineImage> = Vec::new();
    let mut budget = MAX_TOTAL_BYTES;

    // Primary pass — the same `attachments()` enumeration the listing
    // path uses to stamp `EmailAttachment.part_id`, so ids line up.
    for (idx, part) in parsed.attachments().enumerate() {
        if !is_inline_image(part) {
            continue;
        }
        push_part(&mut out, &mut budget, parsed, part, idx as u32);
    }

    // Fallback pass — an image part that mail-parser didn't classify
    // as an attachment (nested `message/rfc822`, senders that mark
    // the part `inline` inside a `multipart/alternative`, …) is still
    // referenceable by cid, and `fetch_attachment` can still resolve
    // it via its parts-array fallback.  Only parts carrying a
    // Content-ID we haven't already collected qualify, which keeps
    // this from re-adding anything the primary pass took under a
    // second, different part_id.
    for (idx, part) in parsed.parts.iter().enumerate() {
        let Some(cid) = part.content_id() else {
            continue;
        };
        if !is_image_part(part) {
            continue;
        }
        if out.iter().any(|i| {
            i.content_id
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(cid))
        }) {
            continue;
        }
        push_part(&mut out, &mut budget, parsed, part, idx as u32);
    }

    out
}

/// Materialise one part into the result list, honouring the per-image
/// and whole-message size ceilings.
fn push_part(
    out: &mut Vec<InlineImage>,
    budget: &mut usize,
    parsed: &Message<'_>,
    part: &MessagePart<'_>,
    part_id: u32,
) {
    let bytes = part.contents();
    if bytes.len() > MAX_IMAGE_BYTES {
        warn!(
            "inline image part #{part_id} is {} bytes — over the {MAX_IMAGE_BYTES} byte \
             per-image limit; leaving it to the attachment list",
            bytes.len()
        );
        return;
    }
    if bytes.len() > *budget {
        warn!(
            "inline image part #{part_id} would exceed the {MAX_TOTAL_BYTES} byte per-message \
             budget; skipping the remaining inline images"
        );
        return;
    }
    *budget -= bytes.len();
    out.push(InlineImage {
        part_id,
        content_id: part.content_id().map(|s| s.to_string()),
        filename: decode_attachment_filename(parsed, part),
        content_type: mime_of(part),
        bytes: bytes.to_vec(),
    });
}

/// `image/*`, case-insensitively.
fn is_image_part(part: &MessagePart<'_>) -> bool {
    part.content_type()
        .is_some_and(|ct| ct.ctype().eq_ignore_ascii_case("image"))
}

/// An image the body could plausibly reference: it names itself with
/// a Content-ID, or the sender flagged it `inline`.
fn is_inline_image(part: &MessagePart<'_>) -> bool {
    if !is_image_part(part) {
        return false;
    }
    part.content_id().is_some() || part.content_disposition().is_some_and(|cd| cd.is_inline())
}

/// Rebuild the `type/subtype` string for the renderer's Blob type.
fn mime_of(part: &MessagePart<'_>) -> String {
    part.content_type()
        .map(|ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub),
            None => ct.ctype().to_string(),
        })
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `multipart/related` with an HTML body and two image parts:
    /// one referenced by cid, one a plain attachment.  This is the
    /// canonical shape #471 is about.
    const RELATED_EML: &[u8] = b"From: Alex Morgan <alex@example.com>\r\n\
To: you@example.com\r\n\
Subject: Inline logo\r\n\
Content-Type: multipart/related; boundary=\"outer\"\r\n\
\r\n\
--outer\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>Hello</p><img src=\"cid:logo@example\">\r\n\
--outer\r\n\
Content-Type: image/png\r\n\
Content-ID: <logo@example>\r\n\
Content-Disposition: inline; filename=\"logo.png\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
aGVsbG8=\r\n\
--outer\r\n\
Content-Type: image/jpeg\r\n\
Content-Disposition: attachment; filename=\"photo.jpg\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
d29ybGQ=\r\n\
--outer--\r\n";

    #[test]
    fn collects_the_cid_referenced_image_only() {
        let images = collect_inline_images(RELATED_EML).unwrap();
        assert_eq!(images.len(), 1, "the plain attachment must not be inlined");
        let img = &images[0];
        assert_eq!(img.content_id.as_deref(), Some("logo@example"));
        assert_eq!(img.filename, "logo.png");
        assert_eq!(img.content_type, "image/png");
        // base64 "aGVsbG8=" — proves we hand back transfer-decoded bytes.
        assert_eq!(img.bytes, b"hello");
    }

    /// The part_id an inline image reports has to address the same
    /// part `fetch_attachment` would return, otherwise a click on the
    /// matching attachment chip downloads the wrong bytes.
    #[test]
    fn part_id_matches_the_attachment_indexing() {
        let images = collect_inline_images(RELATED_EML).unwrap();
        let parsed = MessageParser::default().parse(RELATED_EML).unwrap();
        let attachment = parsed.attachment(images[0].part_id).unwrap();
        assert_eq!(attachment.contents(), b"hello");
    }

    /// `Content-Disposition: inline` without a Content-ID still
    /// qualifies — the renderer matches those by filename.
    #[test]
    fn collects_inline_disposition_without_content_id() {
        let eml = b"Subject: t\r\n\
Content-Type: multipart/related; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/html\r\n\
\r\n\
<img src=\"cid:pic.png\">\r\n\
--b\r\n\
Content-Type: image/png\r\n\
Content-Disposition: inline; filename=\"pic.png\"\r\n\
\r\n\
raw-bytes\r\n\
--b--\r\n";
        let images = collect_inline_images(eml).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].content_id, None);
        assert_eq!(images[0].filename, "pic.png");
    }

    /// A message with nothing referenceable comes back empty rather
    /// than erroring — the renderer skips the IPC entirely in that
    /// case, but the backend must not be the thing that breaks if it
    /// is called anyway.
    #[test]
    fn plain_message_yields_nothing() {
        let eml = b"Subject: t\r\nContent-Type: text/plain\r\n\r\nhi\r\n";
        assert!(collect_inline_images(eml).unwrap().is_empty());
    }

    /// Non-image parts never ride along, even when they carry a
    /// Content-ID (the compose path stamps one on *every* attachment
    /// it references from the editor, #93).
    #[test]
    fn non_image_parts_with_a_content_id_are_ignored() {
        let eml = b"Subject: t\r\n\
Content-Type: multipart/related; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/html\r\n\
\r\n\
<a href=\"cid:doc@x\">doc</a>\r\n\
--b\r\n\
Content-Type: application/pdf\r\n\
Content-ID: <doc@x>\r\n\
Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
\r\n\
%PDF-1.4\r\n\
--b--\r\n";
        assert!(collect_inline_images(eml).unwrap().is_empty());
    }
}
