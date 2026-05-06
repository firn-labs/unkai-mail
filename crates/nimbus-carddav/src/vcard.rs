//! vCard → flat struct mapping.
//!
//! We use the `ical` crate's vCard parser to handle the painful parts
//! (line folding, encoded values, escape sequences) and walk the
//! resulting properties to extract the handful of fields we care
//! about.
//!
//! # Field selection
//!
//! Address-autocomplete needs name + email + photo at minimum; we
//! also keep phone numbers and the organisation since they're
//! cheap to grab and useful for the contact card. Birthday, address,
//! categories etc. live in `vcard_raw` for now — the row stays
//! re-extractable when we build a richer contact view later.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ical::parser::vcard::VcardParser;
use ical::property::Property;

use nimbus_core::NimbusError;

/// The fields we lift out of a vCard. `uid` is required by RFC 6350,
/// so a missing UID makes the vCard unusable for sync (we have no
/// stable identifier) and we surface that as an error rather than
/// fabricating one — the caller will skip and warn.
#[derive(Debug, Clone, Default)]
pub struct ParsedVcard {
    pub uid: String,
    pub display_name: String,
    pub emails: Vec<VcardEmail>,
    pub phones: Vec<VcardPhone>,
    pub organization: Option<String>,
    pub title: Option<String>,
    pub addresses: Vec<VcardAddress>,
    pub birthday: Option<String>,
    pub urls: Vec<String>,
    pub note: Option<String>,
    pub photo_mime: Option<String>,
    pub photo_data: Option<Vec<u8>>,
    /// `KIND` property (RFC 6350 §6.1.4).  Lower-cased, empty
    /// when absent.  We currently care about `"group"` so the
    /// store can treat group cards as a separate kind, leaving
    /// individual contacts unaffected.
    pub kind: String,
    /// `MEMBER` property values for `KIND:group` cards (RFC
    /// 6350 §6.6.5).  We preserve the URI as written
    /// (`urn:uuid:<uid>` / `mailto:<addr>`) and let callers
    /// resolve them to other vCards.  Empty for non-group cards.
    pub members: Vec<String>,
    /// `CATEGORIES` tag list (RFC 6350 §6.7.1) — what NC's
    /// Contacts UI calls "Kontaktgruppen" and what iOS shows
    /// as Groups.  Comma-separated on the wire; we keep them
    /// as a Vec so callers can mutate individual tags without
    /// re-parsing.  Empty when the vCard has no CATEGORIES.
    pub categories: Vec<String>,
    // ── #143: vCard 4 fields surfaced in the contact form ──────
    /// `N` structured name parts (family / given / additional /
    /// prefix / suffix).  Empty when the card only carried FN.
    pub structured_name: VcardStructuredName,
    /// `NICKNAME` — single value (we collapse comma-separated
    /// lists into the first entry, matching how every client
    /// renders them).
    pub nickname: Option<String>,
    /// `ANNIVERSARY` — same wire shape as BDAY.
    pub anniversary: Option<String>,
    /// `GENDER` — raw vCard string per RFC 6350 §6.2.7.
    pub gender: Option<String>,
    /// `IMPP` — instant-messaging URIs with kind hints.
    pub impp: Vec<VcardImpp>,
    /// `ROLE` — function within the organisation, distinct from
    /// TITLE.
    pub role: Option<String>,
    /// `LANG` — BCP-47 language tags in preference order.
    pub languages: Vec<String>,
    /// `GEO` — vCard `geo:<lat>,<lon>` URI, kept raw.
    pub geo: Option<String>,
    /// `TZ` — IANA tag or UTC offset, kept raw.
    pub timezone: Option<String>,
    /// `KEY` — public-key material (PGP / X.509) either inline
    /// or by URL.  Round-tripped today; UI surface deferred to
    /// a future key-management issue.
    pub keys: Vec<String>,
}

/// One vCard `N` structured-name property.  Mirrors
/// `nimbus_core::models::StructuredName` so the carddav crate can
/// stay free of the core models dependency.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct VcardStructuredName {
    pub family: String,
    pub given: String,
    pub additional: String,
    pub prefix: String,
    pub suffix: String,
}

impl VcardStructuredName {
    pub fn is_empty(&self) -> bool {
        self.family.trim().is_empty()
            && self.given.trim().is_empty()
            && self.additional.trim().is_empty()
            && self.prefix.trim().is_empty()
            && self.suffix.trim().is_empty()
    }
}

/// One vCard `IMPP` property — IM URI plus a kind hint pulled
/// from `TYPE=` or inferred from the URI scheme.  Mirrors
/// `nimbus_core::models::ContactImpp`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct VcardImpp {
    pub kind: String,
    pub value: String,
}

/// One vCard `ADR` property. Mirrors `nimbus_core::models::ContactAddress`
/// so the carddav crate can stay free of the core models dependency.
/// `Serialize + Deserialize` so it round-trips inside `RawContact`
/// over the Tauri IPC boundary.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct VcardAddress {
    pub kind: String,
    pub street: String,
    pub locality: String,
    pub region: String,
    pub postal_code: String,
    pub country: String,
}

/// One vCard `TEL` property — the number plus a kind hint pulled
/// from `TYPE=`. Mirrors `nimbus_core::models::ContactPhone`; same
/// dependency-direction reasoning as `VcardAddress`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct VcardPhone {
    pub kind: String,
    pub value: String,
}

/// One vCard `EMAIL` property. Same shape as `VcardPhone`; the
/// kind hint comes from `TYPE=home` / `TYPE=work` (with the legacy
/// `INTERNET` collapsed into `"other"`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct VcardEmail {
    pub kind: String,
    pub value: String,
}

/// Parse a single vCard string. The input is the raw `BEGIN:VCARD … END:VCARD`
/// block; the `ical` parser returns at most one card from it.
pub fn parse_vcard(raw: &str) -> Result<ParsedVcard, NimbusError> {
    let reader = std::io::BufReader::new(raw.as_bytes());
    let mut parser = VcardParser::new(reader);

    let card = parser
        .next()
        .ok_or_else(|| NimbusError::Protocol("empty vCard".to_string()))?
        .map_err(|e| NimbusError::Protocol(format!("vCard parse: {e}")))?;

    let mut uid: Option<String> = None;
    let mut formatted_name = String::new();
    let mut structured_display_name = String::new();
    let mut structured_name = VcardStructuredName::default();
    let mut emails: Vec<VcardEmail> = Vec::new();
    let mut phones: Vec<VcardPhone> = Vec::new();
    let mut organization: Option<String> = None;
    let mut title: Option<String> = None;
    let mut addresses: Vec<VcardAddress> = Vec::new();
    let mut birthday: Option<String> = None;
    let mut urls: Vec<String> = Vec::new();
    let mut note: Option<String> = None;
    let mut photo_mime: Option<String> = None;
    let mut photo_data: Option<Vec<u8>> = None;
    let mut kind: String = String::new();
    let mut members: Vec<String> = Vec::new();
    let mut categories: Vec<String> = Vec::new();
    // #143 — additional vCard fields surfaced in the form.
    let mut nickname: Option<String> = None;
    let mut anniversary: Option<String> = None;
    let mut gender: Option<String> = None;
    let mut impp: Vec<VcardImpp> = Vec::new();
    let mut role: Option<String> = None;
    let mut languages: Vec<String> = Vec::new();
    let mut geo: Option<String> = None;
    let mut timezone: Option<String> = None;
    let mut keys: Vec<String> = Vec::new();

    for prop in &card.properties {
        let upper = prop.name.to_ascii_uppercase();
        // Strip the optional group qualifier per RFC 6350 §3.3:
        // properties may be written as `<group>.<PROPERTY>` (Apple-
        // style `item1.ADR`, `item2.EMAIL`, etc.) to link related
        // properties — typically an ADR or TEL with a sibling
        // X-ABLABEL that gives it a custom label.  The qualifier
        // is just a free-form group name; it doesn't change how
        // the property value is interpreted, so dispatch on the
        // unqualified name.  Without this stripping every Apple-
        // exported (or just relabelled-in-NC-Contacts) ADR / TEL /
        // EMAIL would silently fall through the match and disappear.
        let name = upper
            .rsplit_once('.')
            .map(|(_, n)| n)
            .unwrap_or(upper.as_str());
        let Some(value) = &prop.value else { continue };
        match name {
            "UID" => uid = Some(value.clone()),
            "FN" => formatted_name = value.clone(),
            "N" => {
                // N is Family;Given;Additional;Prefix;Suffix per
                // RFC 6350 §6.2.2.  We capture each piece for
                // the contact form (#143) and also keep a flat
                // "given family" string as the FN-fallback.
                let parts: Vec<&str> = value.split(';').collect();
                let family = parts.first().copied().unwrap_or("").trim().to_string();
                let given = parts.get(1).copied().unwrap_or("").trim().to_string();
                let additional = parts.get(2).copied().unwrap_or("").trim().to_string();
                let prefix = parts.get(3).copied().unwrap_or("").trim().to_string();
                let suffix = parts.get(4).copied().unwrap_or("").trim().to_string();
                structured_display_name = format!("{given} {family}").trim().to_string();
                structured_name = VcardStructuredName {
                    family,
                    given,
                    additional,
                    prefix,
                    suffix,
                };
            }
            "EMAIL" => {
                let v = value.trim().to_string();
                if v.is_empty() {
                    continue;
                }
                let kind = email_kind(prop);
                // Dedup by value (kind-agnostic) so a card that
                // lists the same address with and without a TYPE
                // doesn't duplicate.
                if !emails.iter().any(|e| e.value == v) {
                    emails.push(VcardEmail { kind, value: v });
                }
            }
            "TEL" => {
                let v = value.trim().to_string();
                if v.is_empty() {
                    continue;
                }
                let kind = phone_kind(prop);
                // Dedup by value (kind-agnostic) — vCards from older
                // syncs sometimes carry the same number twice with
                // and without a TYPE; we keep the first occurrence.
                if !phones.iter().any(|p| p.value == v) {
                    phones.push(VcardPhone { kind, value: v });
                }
            }
            "ORG" => {
                // ORG is Company;Department;... — first segment is the
                // organisation proper.
                let first = value.split(';').next().unwrap_or("").trim().to_string();
                if !first.is_empty() {
                    organization = Some(first);
                }
            }
            "TITLE" => {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    title = Some(v);
                }
            }
            "ADR" => {
                // ADR is PO-box;Extended;Street;Locality;Region;Postal;Country.
                // PO-box and Extended are commonly empty; keep them absent
                // from our flat model.
                let parts: Vec<&str> = value.split(';').collect();
                let street = parts.get(2).copied().unwrap_or("").trim().to_string();
                let locality = parts.get(3).copied().unwrap_or("").trim().to_string();
                let region = parts.get(4).copied().unwrap_or("").trim().to_string();
                let postal_code = parts.get(5).copied().unwrap_or("").trim().to_string();
                let country = parts.get(6).copied().unwrap_or("").trim().to_string();
                if street.is_empty()
                    && locality.is_empty()
                    && region.is_empty()
                    && postal_code.is_empty()
                    && country.is_empty()
                {
                    continue;
                }
                let kind = address_kind(prop);
                addresses.push(VcardAddress {
                    kind,
                    street,
                    locality,
                    region,
                    postal_code,
                    country,
                });
            }
            "BDAY" => {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    birthday = Some(v);
                }
            }
            "URL" => {
                let v = value.trim().to_string();
                if !v.is_empty() && !urls.contains(&v) {
                    urls.push(v);
                }
            }
            "NOTE" => {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    note = Some(v);
                }
            }
            "PHOTO" => {
                if let Some((mime, bytes)) = decode_photo(prop, value) {
                    photo_mime = Some(mime);
                    photo_data = Some(bytes);
                }
            }
            "KIND" | "X-ADDRESSBOOKSERVER-KIND" => {
                // X-ADDRESSBOOKSERVER-KIND is Apple's
                // pre-vCard-4.0 spelling that NC Contacts also
                // round-trips for backwards compat — accept either.
                kind = value.trim().to_ascii_lowercase();
            }
            "MEMBER" | "X-ADDRESSBOOKSERVER-MEMBER" => {
                let v = value.trim().to_string();
                if !v.is_empty() && !members.contains(&v) {
                    members.push(v);
                }
            }
            "CATEGORIES" => {
                // Comma-separated per RFC 6350; some old clients
                // emit semicolons.  Accept both, dedupe within
                // the same property, preserve cross-property
                // accumulation in case a card has CATEGORIES
                // listed twice.
                for raw in value.split([',', ';']) {
                    let t = raw.trim();
                    if !t.is_empty() && !categories.iter().any(|c| c == t) {
                        categories.push(t.to_string());
                    }
                }
            }
            // ── #143: vCard 4 fields ─────────────────────────────
            "NICKNAME" => {
                // RFC 6350 §6.2.3 allows a comma-separated list,
                // but every client we've seen treats the field
                // as a single nickname.  Take the first non-empty
                // entry and ignore the rest.
                let v = value
                    .split(',')
                    .map(str::trim)
                    .find(|s| !s.is_empty())
                    .unwrap_or("")
                    .to_string();
                if !v.is_empty() {
                    nickname = Some(v);
                }
            }
            "ANNIVERSARY" => {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    anniversary = Some(v);
                }
            }
            "GENDER" => {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    gender = Some(v);
                }
            }
            "IMPP" => {
                let v = value.trim().to_string();
                if v.is_empty() {
                    continue;
                }
                let kind = impp_kind(prop, &v);
                if !impp.iter().any(|i| i.value == v) {
                    impp.push(VcardImpp { kind, value: v });
                }
            }
            "ROLE" => {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    role = Some(v);
                }
            }
            "LANG" => {
                let v = value.trim().to_string();
                if !v.is_empty() && !languages.contains(&v) {
                    languages.push(v);
                }
            }
            "GEO" => {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    geo = Some(v);
                }
            }
            "TZ" => {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    timezone = Some(v);
                }
            }
            "KEY" => {
                let v = value.trim().to_string();
                if !v.is_empty() && !keys.contains(&v) {
                    keys.push(v);
                }
            }
            _ => {}
        }
    }

    // Prefer FN (formatted name) — it's what RFC 6350 says clients
    // should display. Fall back to N → first email → "(unnamed)".
    let display_name = if !formatted_name.is_empty() {
        formatted_name
    } else if !structured_display_name.is_empty() {
        structured_display_name
    } else if let Some(first) = emails.first() {
        first.value.clone()
    } else {
        "(unnamed)".to_string()
    };

    let uid = uid.ok_or_else(|| NimbusError::Protocol("vCard missing UID".to_string()))?;

    Ok(ParsedVcard {
        uid,
        display_name,
        emails,
        phones,
        organization,
        title,
        addresses,
        birthday,
        urls,
        note,
        photo_mime,
        photo_data,
        kind,
        members,
        categories,
        structured_name,
        nickname,
        anniversary,
        gender,
        impp,
        role,
        languages,
        geo,
        timezone,
        keys,
    })
}

/// Pull a kind hint for an `IMPP` property — TYPE param wins,
/// otherwise we infer from the URI scheme so a card written by a
/// minimal client (no TYPE) still groups correctly in the form.
fn impp_kind(prop: &Property, value: &str) -> String {
    // Explicit TYPE takes priority.
    if let Some(params) = &prop.params {
        for (key, vals) in params {
            if !key.eq_ignore_ascii_case("TYPE") {
                continue;
            }
            for v in vals {
                for piece in v.split(',') {
                    let lower = piece.trim().to_ascii_lowercase();
                    if matches!(
                        lower.as_str(),
                        "matrix" | "xmpp" | "telegram" | "signal" | "skype" | "whatsapp"
                    ) {
                        return lower;
                    }
                }
            }
        }
    }
    // Fall back to URI-scheme inference.
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("matrix:") {
        "matrix".into()
    } else if lower.starts_with("xmpp:") {
        "xmpp".into()
    } else if lower.starts_with("tg://") || lower.contains("t.me/") {
        "telegram".into()
    } else if lower.contains("signal.me") {
        "signal".into()
    } else if lower.starts_with("skype:") {
        "skype".into()
    } else if lower.contains("wa.me/") || lower.contains("whatsapp.com") {
        "whatsapp".into()
    } else {
        "other".into()
    }
}

/// Pull a "home" / "work" / "other" hint from a vCard property's
/// `TYPE` parameter. vCard 4 lets the type be a comma-separated list
/// (e.g. `TYPE="home,pref"`) — we take the first recognised value.
fn address_kind(prop: &Property) -> String {
    pick_type(prop, &["home", "work"])
}

/// Same as `address_kind` but with the vCard `TEL` value set
/// — `cell` (mobile), `fax`, plus the home/work pair. Anything
/// else (pager, video, text, etc.) falls back to `"other"`.
fn phone_kind(prop: &Property) -> String {
    pick_type(prop, &["home", "work", "cell", "fax"])
}

/// Same as `address_kind` for `EMAIL`. Only `home` / `work` are
/// meaningful; `INTERNET` (a vCard 3 marker for "this is an
/// internet email address" rather than X.400 — useless today)
/// and any other value collapse into `"other"`.
fn email_kind(prop: &Property) -> String {
    pick_type(prop, &["home", "work"])
}

/// Walk a property's `TYPE=` parameter (which may be a single value
/// or a comma-separated list) and return the first piece that
/// matches one of `accepted`. Returns `"other"` if nothing matches.
fn pick_type(prop: &Property, accepted: &[&str]) -> String {
    if let Some(params) = &prop.params {
        for (key, vals) in params {
            if !key.eq_ignore_ascii_case("TYPE") {
                continue;
            }
            for v in vals {
                for piece in v.split(',') {
                    let lower = piece.trim().to_ascii_lowercase();
                    if accepted.iter().any(|a| *a == lower) {
                        return lower;
                    }
                }
            }
        }
    }
    "other".to_string()
}

/// Decode a PHOTO property into `(mime, bytes)`.
///
/// Two shapes show up in the wild:
///
/// - **vCard 3 inline:** `PHOTO;ENCODING=b;TYPE=JPEG:<base64>` — the
///   value is base64 text, the type comes from a TYPE param.
/// - **vCard 4 data URI:** `PHOTO:data:image/jpeg;base64,<base64>` —
///   the value embeds both mime and bytes.
///
/// External URLs (`PHOTO:https://…`) are skipped — we don't fetch
/// them in this pass.
fn decode_photo(prop: &Property, value: &str) -> Option<(String, Vec<u8>)> {
    // vCard 4 data URI form.
    if let Some(rest) = value.strip_prefix("data:") {
        let (meta, b64) = rest.split_once(',')?;
        let mime = meta
            .split(';')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = BASE64.decode(b64).ok()?;
        return Some((mime, bytes));
    }
    // vCard 3 inline form — value is bare base64, type/encoding in params.
    let mut is_base64 = false;
    let mut mime = "image/jpeg".to_string(); // safe default for NC
    if let Some(params) = &prop.params {
        for (key, vals) in params {
            let upper = key.to_ascii_uppercase();
            if upper == "ENCODING" {
                if vals
                    .iter()
                    .any(|v| matches!(v.to_ascii_lowercase().as_str(), "b" | "base64"))
                {
                    is_base64 = true;
                }
            } else if upper == "TYPE"
                && let Some(t) = vals.first()
            {
                let t = t.to_ascii_lowercase();
                if !t.is_empty() {
                    mime = if t.starts_with("image/") {
                        t
                    } else {
                        format!("image/{t}")
                    };
                }
            }
        }
    }
    if !is_base64 {
        return None;
    }
    let cleaned: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = BASE64.decode(cleaned).ok()?;
    Some((mime, bytes))
}

/// Render a `ParsedVcard` back into the wire format.
///
/// We emit **vCard 4.0** because it's the format Nextcloud itself
/// generates today and it has the cleanest PHOTO encoding (a
/// `data:` URI rather than the awkward vCard-3 base64-with-params
/// form). All values are escaped per RFC 6350 §3.4 — newlines,
/// commas, semicolons and backslashes get the standard `\n`, `\,`,
/// `\;`, `\\` treatment so a name like `Smith; Jr.` round-trips
/// cleanly.
///
/// Long lines (>75 octets) are folded by inserting a CRLF + space
/// continuation, also per RFC 6350; this matters for embedded
/// photos which can run to thousands of characters.
pub fn build_vcard(card: &ParsedVcard) -> String {
    let mut out = String::new();
    out.push_str("BEGIN:VCARD\r\n");
    out.push_str("VERSION:4.0\r\n");
    push_line(&mut out, &format!("UID:{}", escape_value(&card.uid)));
    push_line(
        &mut out,
        &format!("FN:{}", escape_value(&card.display_name)),
    );
    // N is required by RFC 6350.  When the card carries
    // structured-name pieces (set via the redesigned form in
    // #143), emit them in the proper Family;Given;Additional;
    // Prefix;Suffix order; otherwise fall back to stuffing FN
    // into the family slot — that round-trips through every
    // client we've tested.
    if card.structured_name.is_empty() {
        push_line(
            &mut out,
            &format!("N:{};;;;", escape_value(&card.display_name)),
        );
    } else {
        push_line(
            &mut out,
            &format!(
                "N:{};{};{};{};{}",
                escape_value(&card.structured_name.family),
                escape_value(&card.structured_name.given),
                escape_value(&card.structured_name.additional),
                escape_value(&card.structured_name.prefix),
                escape_value(&card.structured_name.suffix),
            ),
        );
    }
    if let Some(n) = &card.nickname
        && !n.trim().is_empty()
    {
        push_line(&mut out, &format!("NICKNAME:{}", escape_value(n)));
    }
    for email in &card.emails {
        // Same `;TYPE=…` round-trip as TEL — empty kind drops the
        // param (servers don't all accept `TYPE=` with no value).
        let typ = if email.kind.is_empty() {
            String::new()
        } else {
            format!(";TYPE={}", email.kind)
        };
        push_line(
            &mut out,
            &format!("EMAIL{typ}:{}", escape_value(&email.value)),
        );
    }
    for phone in &card.phones {
        // Mirror the address `TYPE` round-trip: emit `TEL;TYPE=cell:…`
        // so kind survives the round-trip. Empty kind drops the param
        // (some servers reject `TYPE=` with no value).
        let typ = if phone.kind.is_empty() {
            String::new()
        } else {
            format!(";TYPE={}", phone.kind)
        };
        push_line(
            &mut out,
            &format!("TEL{typ}:{}", escape_value(&phone.value)),
        );
    }
    if let Some(org) = &card.organization {
        push_line(&mut out, &format!("ORG:{}", escape_value(org)));
    }
    if let Some(t) = &card.title {
        push_line(&mut out, &format!("TITLE:{}", escape_value(t)));
    }
    for adr in &card.addresses {
        // `home`/`work`/`other` ride through in the TYPE param so a
        // round-trip keeps the user's grouping intact.
        let typ = if adr.kind.is_empty() {
            String::new()
        } else {
            format!(";TYPE={}", adr.kind)
        };
        // Empty PO-box and Extended slots, then street/locality/region/
        // postal/country in RFC 6350 order.
        let payload = format!(
            ";;{};{};{};{};{}",
            escape_value(&adr.street),
            escape_value(&adr.locality),
            escape_value(&adr.region),
            escape_value(&adr.postal_code),
            escape_value(&adr.country),
        );
        push_line(&mut out, &format!("ADR{typ}:{payload}"));
    }
    if let Some(b) = &card.birthday {
        push_line(&mut out, &format!("BDAY:{}", escape_value(b)));
    }
    if let Some(a) = &card.anniversary
        && !a.trim().is_empty()
    {
        push_line(&mut out, &format!("ANNIVERSARY:{}", escape_value(a)));
    }
    if let Some(g) = &card.gender
        && !g.trim().is_empty()
    {
        push_line(&mut out, &format!("GENDER:{}", escape_value(g)));
    }
    if let Some(r) = &card.role
        && !r.trim().is_empty()
    {
        push_line(&mut out, &format!("ROLE:{}", escape_value(r)));
    }
    for ent in &card.impp {
        let typ = if ent.kind.is_empty() {
            String::new()
        } else {
            format!(";TYPE={}", ent.kind)
        };
        push_line(&mut out, &format!("IMPP{typ}:{}", escape_value(&ent.value)));
    }
    for lang in &card.languages {
        if !lang.trim().is_empty() {
            push_line(&mut out, &format!("LANG:{}", escape_value(lang)));
        }
    }
    if let Some(g) = &card.geo
        && !g.trim().is_empty()
    {
        push_line(&mut out, &format!("GEO:{}", escape_value(g)));
    }
    if let Some(tz) = &card.timezone
        && !tz.trim().is_empty()
    {
        push_line(&mut out, &format!("TZ:{}", escape_value(tz)));
    }
    for k in &card.keys {
        if !k.trim().is_empty() {
            push_line(&mut out, &format!("KEY:{}", escape_value(k)));
        }
    }
    for url in &card.urls {
        push_line(&mut out, &format!("URL:{}", escape_value(url)));
    }
    if let Some(n) = &card.note {
        push_line(&mut out, &format!("NOTE:{}", escape_value(n)));
    }
    // CATEGORIES — single comma-separated property per RFC 6350.
    // Empty list omits the line entirely so we don't emit a stray
    // `CATEGORIES:` that some servers reject.
    if !card.categories.is_empty() {
        let joined = card
            .categories
            .iter()
            .map(|c| escape_value(c))
            .collect::<Vec<_>>()
            .join(",");
        push_line(&mut out, &format!("CATEGORIES:{joined}"));
    }
    if let (Some(mime), Some(bytes)) = (&card.photo_mime, &card.photo_data) {
        // vCard 4 PHOTO as data URI — single property, no params,
        // line-folded so it stays under 75 octets per physical line.
        let b64 = BASE64.encode(bytes);
        push_line(&mut out, &format!("PHOTO:data:{mime};base64,{b64}"));
    }
    // KIND + MEMBER for group cards (#133 / #113).  We emit both
    // the RFC 6350 spelling and Apple's `X-ADDRESSBOOKSERVER-…`
    // legacy prefixed form so older clients (Apple Contacts up
    // through 14.x, NC's mobile companion) still recognise the
    // group on round-trip.  Non-group cards skip both lines.
    if !card.kind.is_empty() {
        push_line(&mut out, &format!("KIND:{}", escape_value(&card.kind)));
        if card.kind.eq_ignore_ascii_case("group") {
            push_line(
                &mut out,
                &format!("X-ADDRESSBOOKSERVER-KIND:{}", escape_value(&card.kind)),
            );
            for m in &card.members {
                push_line(&mut out, &format!("MEMBER:{}", escape_value(m)));
                push_line(
                    &mut out,
                    &format!("X-ADDRESSBOOKSERVER-MEMBER:{}", escape_value(m)),
                );
            }
        }
    }
    out.push_str("END:VCARD\r\n");
    out
}

/// Escape a vCard value per RFC 6350 §3.4. Order matters — backslash
/// first so it doesn't double-escape the others.
fn escape_value(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "")
        .replace(',', "\\,")
        .replace(';', "\\;")
}

/// Append a content line, folding it if it exceeds the 75-octet
/// limit. Folding is "CRLF + single space" — the receiver strips
/// that pair to reconstruct the logical line.
fn push_line(out: &mut String, line: &str) {
    const MAX: usize = 75;
    let bytes = line.as_bytes();
    if bytes.len() <= MAX {
        out.push_str(line);
        out.push_str("\r\n");
        return;
    }
    // Fold on byte boundaries that fall on UTF-8 char boundaries —
    // base64/data-URI content is ASCII so this is trivially safe in
    // practice; the find_char_boundary loop handles any future
    // non-ASCII text we hand it (e.g. non-Latin display names).
    let mut start = 0;
    let mut first = true;
    while start < bytes.len() {
        let take = if first { MAX } else { MAX - 1 };
        let mut end = (start + take).min(bytes.len());
        while end < bytes.len() && !line.is_char_boundary(end) {
            end -= 1;
        }
        if !first {
            out.push(' ');
        }
        out.push_str(&line[start..end]);
        out.push_str("\r\n");
        start = end;
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_vcard() {
        let raw = "BEGIN:VCARD\r\n\
                   VERSION:3.0\r\n\
                   UID:abc-123\r\n\
                   FN:Alice Example\r\n\
                   EMAIL;TYPE=INTERNET:alice@example.com\r\n\
                   TEL;TYPE=CELL:+1 555 0100\r\n\
                   ORG:Example Corp;Engineering\r\n\
                   END:VCARD\r\n";
        let p = parse_vcard(raw).unwrap();
        assert_eq!(p.uid, "abc-123");
        assert_eq!(p.display_name, "Alice Example");
        assert_eq!(p.emails.len(), 1);
        // INTERNET collapses to "other" — vCard 3 puts INTERNET on
        // every email and it carries no useful info.
        assert_eq!(p.emails[0].kind, "other");
        assert_eq!(p.emails[0].value, "alice@example.com");
        assert_eq!(p.phones.len(), 1);
        assert_eq!(p.phones[0].kind, "cell");
        assert_eq!(p.phones[0].value, "+1 555 0100");
        assert_eq!(p.organization.as_deref(), Some("Example Corp"));
        assert!(p.photo_data.is_none());
    }

    #[test]
    fn falls_back_to_n_when_fn_absent() {
        let raw = "BEGIN:VCARD\r\n\
                   VERSION:3.0\r\n\
                   UID:nofn\r\n\
                   N:Smith;Bob;;;\r\n\
                   END:VCARD\r\n";
        let p = parse_vcard(raw).unwrap();
        assert_eq!(p.display_name, "Bob Smith");
    }

    #[test]
    fn missing_uid_is_an_error() {
        let raw = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:X\r\nEND:VCARD\r\n";
        assert!(parse_vcard(raw).is_err());
    }

    #[test]
    fn build_then_parse_round_trips_unescaped_fields() {
        // Test the common case (no characters that need escaping) end
        // to end. `escape_value` is exercised separately below since
        // the `ical` parser doesn't un-escape `\;` / `\,` on read.
        let original = ParsedVcard {
            uid: "abc-123".into(),
            display_name: "Alice Example".into(),
            emails: vec![
                VcardEmail {
                    kind: "home".into(),
                    value: "alice@example.com".into(),
                },
                VcardEmail {
                    kind: "work".into(),
                    value: "alice@work.com".into(),
                },
            ],
            phones: vec![
                VcardPhone {
                    kind: "cell".into(),
                    value: "+1 555 0100".into(),
                },
                VcardPhone {
                    kind: "work".into(),
                    value: "+1 555 0200".into(),
                },
            ],
            organization: Some("Example Corp".into()),
            ..Default::default()
        };
        let raw = build_vcard(&original);
        assert!(raw.starts_with("BEGIN:VCARD\r\nVERSION:4.0\r\n"));

        let parsed = parse_vcard(&raw).expect("re-parse");
        assert_eq!(parsed.uid, "abc-123");
        assert_eq!(parsed.display_name, "Alice Example");
        assert_eq!(parsed.emails.len(), 2);
        assert_eq!(parsed.emails[0].kind, "home");
        assert_eq!(parsed.emails[0].value, "alice@example.com");
        assert_eq!(parsed.emails[1].kind, "work");
        assert_eq!(parsed.emails[1].value, "alice@work.com");
        assert_eq!(parsed.phones.len(), 2);
        assert_eq!(parsed.phones[0].kind, "cell");
        assert_eq!(parsed.phones[0].value, "+1 555 0100");
        assert_eq!(parsed.phones[1].kind, "work");
        assert_eq!(parsed.phones[1].value, "+1 555 0200");
        assert_eq!(parsed.organization.as_deref(), Some("Example Corp"));
    }

    #[test]
    fn build_escapes_special_characters() {
        // Build-side correctness: special chars in input must end up
        // as escape sequences in the wire format. (Round-trip on read
        // is limited by the ical parser, which is a separate concern.)
        let card = ParsedVcard {
            uid: "u".into(),
            display_name: "Smith; Jr., \"Bob\"".into(),
            organization: Some("A, B; C".into()),
            ..Default::default()
        };
        let raw = build_vcard(&card);
        // Quotes are not vCard-special; only `;` and `,` are escaped.
        assert!(raw.contains("FN:Smith\\; Jr.\\, \"Bob\""));
        assert!(raw.contains("ORG:A\\, B\\; C"));
    }

    #[test]
    fn build_folds_long_photo_line() {
        // A 200-byte payload is well past one line — make sure the
        // output has no physical line longer than 75 octets and that
        // continuation lines start with a single space.
        let card = ParsedVcard {
            uid: "p".into(),
            display_name: "Big Photo".into(),
            photo_mime: Some("image/png".into()),
            photo_data: Some(vec![0u8; 200]),
            ..Default::default()
        };
        let raw = build_vcard(&card);
        for line in raw.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(
                line.len() <= 75,
                "line longer than 75 bytes: {} ({line:?})",
                line.len()
            );
        }
        // At least one folded continuation line present.
        assert!(raw.contains("\r\n "));
    }

    #[test]
    fn parses_extended_fields() {
        let raw = "BEGIN:VCARD\r\n\
                   VERSION:4.0\r\n\
                   UID:ext-1\r\n\
                   FN:Erika Mustermann\r\n\
                   TITLE:CTO\r\n\
                   BDAY:1985-10-31\r\n\
                   URL:https://example.com\r\n\
                   URL:https://example.com/blog\r\n\
                   NOTE:Met at the conference\r\n\
                   ADR;TYPE=work:;;Hauptstr. 1;Berlin;BE;10115;DE\r\n\
                   ADR;TYPE=home:;;Side St 7;Munich;BY;80331;DE\r\n\
                   END:VCARD\r\n";
        let p = parse_vcard(raw).unwrap();
        assert_eq!(p.title.as_deref(), Some("CTO"));
        assert_eq!(p.birthday.as_deref(), Some("1985-10-31"));
        assert_eq!(p.urls.len(), 2);
        assert_eq!(p.note.as_deref(), Some("Met at the conference"));
        assert_eq!(p.addresses.len(), 2);
        assert_eq!(p.addresses[0].kind, "work");
        assert_eq!(p.addresses[0].street, "Hauptstr. 1");
        assert_eq!(p.addresses[0].locality, "Berlin");
        assert_eq!(p.addresses[1].kind, "home");
    }

    /// Regression: Apple-style group-qualified property names
    /// (`item1.ADR`, `item2.TEL`, …) were silently dropped because
    /// the dispatch matched the literal property name and couldn't
    /// see past the `<group>.` prefix.  Strip it before the match.
    #[test]
    fn parses_apple_group_qualified_properties() {
        let raw = "BEGIN:VCARD\r\n\
                   VERSION:3.0\r\n\
                   UID:grp-1\r\n\
                   FN:Test User\r\n\
                   item1.ADR;type=HOME;type=pref:;;Teststraße 6;Nenningen;;;Deutschland\r\n\
                   item2.TEL;type=CELL:+49 555 0100\r\n\
                   item3.EMAIL;type=INTERNET:user@example.com\r\n\
                   item4.URL:https://example.com\r\n\
                   END:VCARD\r\n";
        let p = parse_vcard(raw).unwrap();
        assert_eq!(p.addresses.len(), 1, "ADR should parse despite group prefix");
        assert_eq!(p.addresses[0].street, "Teststraße 6");
        assert_eq!(p.addresses[0].locality, "Nenningen");
        assert_eq!(p.addresses[0].country, "Deutschland");
        assert_eq!(p.addresses[0].kind, "home");
        assert_eq!(p.phones.len(), 1, "TEL should parse despite group prefix");
        assert_eq!(p.phones[0].value, "+49 555 0100");
        assert_eq!(p.emails.len(), 1, "EMAIL should parse despite group prefix");
        assert_eq!(p.emails[0].value, "user@example.com");
        assert_eq!(p.urls.len(), 1, "URL should parse despite group prefix");
    }

    #[test]
    fn extended_fields_round_trip() {
        let original = ParsedVcard {
            uid: "rt-1".into(),
            display_name: "Erika Mustermann".into(),
            title: Some("CTO".into()),
            birthday: Some("1985-10-31".into()),
            urls: vec!["https://example.com".into()],
            note: Some("hi".into()),
            addresses: vec![VcardAddress {
                kind: "work".into(),
                street: "Hauptstr. 1".into(),
                locality: "Berlin".into(),
                region: "BE".into(),
                postal_code: "10115".into(),
                country: "DE".into(),
            }],
            ..Default::default()
        };
        let raw = build_vcard(&original);
        let parsed = parse_vcard(&raw).expect("re-parse");
        assert_eq!(parsed.title.as_deref(), Some("CTO"));
        assert_eq!(parsed.birthday.as_deref(), Some("1985-10-31"));
        assert_eq!(parsed.urls, vec!["https://example.com"]);
        assert_eq!(parsed.note.as_deref(), Some("hi"));
        assert_eq!(parsed.addresses.len(), 1);
        assert_eq!(parsed.addresses[0].kind, "work");
        assert_eq!(parsed.addresses[0].street, "Hauptstr. 1");
        assert_eq!(parsed.addresses[0].country, "DE");
    }

    #[test]
    fn decodes_data_uri_photo() {
        // 1x1 GIF, base64.
        let raw = "BEGIN:VCARD\r\n\
                   VERSION:4.0\r\n\
                   UID:p1\r\n\
                   FN:With Photo\r\n\
                   PHOTO:data:image/gif;base64,R0lGODlhAQABAAAAACw=\r\n\
                   END:VCARD\r\n";
        let p = parse_vcard(raw).unwrap();
        assert_eq!(p.photo_mime.as_deref(), Some("image/gif"));
        assert!(!p.photo_data.unwrap().is_empty());
    }
}
