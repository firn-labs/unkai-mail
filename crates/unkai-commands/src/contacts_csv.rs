//! CSV → contact mapping for the import-from-file flow (#484).
//!
//! Address-book tools all export a different CSV dialect, but they
//! share one shape: a header row naming the columns, one contact per
//! record. We hand-roll the RFC 4180 parsing (quoted fields, `""`
//! escapes, embedded newlines, CRLF/LF) rather than take a dependency
//! — the format is small and the workspace posture is to keep the
//! SBOM lean for things this size (see `unkai-carddav`'s vCard
//! builder for the same call).
//!
//! Output is [`ParsedVcard`] rather than a CSV-specific struct so the
//! import command funnels both file formats through the exact same
//! write path (`build_vcard` → DAV PUT / local outcome → cache row).
//! The `uid` is left empty — the import command mints a fresh
//! `urn:uuid:` per row, same as the create-contact form does.

use unkai_carddav::{ParsedVcard, VcardAddress, VcardEmail, VcardPhone, VcardStructuredName};
use unkai_core::UnkaiError;

/// One parsed row's outcome: a card, or a reason this row was
/// unusable (carried into the import report so the user can see
/// which lines were dropped and why).
pub enum CsvRowOutcome {
    Card(Box<ParsedVcard>),
    Skipped { line: usize, reason: String },
}

/// Parse a contacts CSV export into cards.
///
/// Errors only when the file as a whole is unusable (no header, no
/// recognisable columns); individual unusable rows come back as
/// `Skipped` entries instead of failing the file — one blank line in
/// a 500-row export shouldn't kill the other 499.
pub fn parse_csv_contacts(text: &str) -> Result<Vec<CsvRowOutcome>, UnkaiError> {
    let delimiter = sniff_delimiter(text);
    let records = parse_records(text, delimiter);
    let mut iter = records.into_iter();

    let header = iter
        .next()
        .ok_or_else(|| UnkaiError::Protocol("CSV file is empty".to_string()))?;
    let columns: Vec<ColumnRole> = header.fields.iter().map(|h| classify_header(h)).collect();

    if !columns.iter().any(|c| !matches!(c, ColumnRole::Ignore)) {
        return Err(UnkaiError::Protocol(
            "no recognisable contact columns in the CSV header — expected \
             headers like \"Name\", \"First Name\", \"E-mail Address\", \"Phone\""
                .to_string(),
        ));
    }

    let mut out = Vec::new();
    for record in iter {
        // Fully empty records are formatting noise (trailing newline,
        // spreadsheet padding rows) — drop them silently rather than
        // reporting a skip for something the user can't see.
        if record.fields.iter().all(|f| f.trim().is_empty()) {
            continue;
        }
        match row_to_card(&columns, &record.fields) {
            Some(card) => out.push(CsvRowOutcome::Card(Box::new(card))),
            None => out.push(CsvRowOutcome::Skipped {
                line: record.line,
                reason: "row has neither a name nor an email address".to_string(),
            }),
        }
    }
    Ok(out)
}

// ── RFC 4180 record parsing ─────────────────────────────────────

struct Record {
    /// 1-based line number the record *started* on (quoted fields can
    /// span lines, so this is the number to show a user).
    line: usize,
    fields: Vec<String>,
}

/// Spreadsheet exports in `;`-locales (German Excel among them) use a
/// semicolon delimiter; a few tools emit tabs. Sniff by counting
/// candidate delimiters *outside quotes* on the header line and pick
/// the most frequent — comma wins ties as the format's default.
fn sniff_delimiter(text: &str) -> char {
    let mut counts = [0usize; 3]; // comma, semicolon, tab
    let mut in_quotes = false;
    for c in text.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\n' if !in_quotes => break,
            ',' if !in_quotes => counts[0] += 1,
            ';' if !in_quotes => counts[1] += 1,
            '\t' if !in_quotes => counts[2] += 1,
            _ => {}
        }
    }
    if counts[1] > counts[0] && counts[1] >= counts[2] {
        ';'
    } else if counts[2] > counts[0] {
        '\t'
    } else {
        ','
    }
}

/// Split the file into records per RFC 4180: fields separated by the
/// delimiter, `"`-quoted fields may contain delimiters/newlines, a
/// doubled `""` inside quotes is a literal quote. Tolerates a UTF-8
/// BOM and both CRLF and LF line endings.
fn parse_records(text: &str, delimiter: char) -> Vec<Record> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut line = 1usize;
    let mut record_start_line = 1usize;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                '\n' => {
                    line += 1;
                    field.push('\n');
                }
                '\r' => {} // normalise CRLF inside quoted fields to \n
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' => in_quotes = true,
            '\r' => {} // CR outside quotes only ever precedes LF
            '\n' => {
                line += 1;
                fields.push(std::mem::take(&mut field));
                records.push(Record {
                    line: record_start_line,
                    fields: std::mem::take(&mut fields),
                });
                record_start_line = line;
            }
            c if c == delimiter => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    // Final record when the file doesn't end in a newline.
    if !field.is_empty() || !fields.is_empty() {
        fields.push(field);
        records.push(Record {
            line: record_start_line,
            fields,
        });
    }
    records
}

// ── Header classification ───────────────────────────────────────

/// What a CSV column means for the contact we build. Classified once
/// from the header row, then applied to every record.
enum ColumnRole {
    DisplayName,
    GivenName,
    FamilyName,
    AdditionalName,
    NamePrefix,
    NameSuffix,
    Nickname,
    /// `kind` is the vCard TYPE hint ("home" / "work" / "cell" / …)
    /// inferred from the header text.
    Email {
        kind: String,
    },
    Phone {
        kind: String,
    },
    Organization,
    JobTitle,
    Birthday,
    Note,
    Url,
    /// One part of a postal address. `bucket` groups sibling columns
    /// back into a single address — "Home Street" + "Home City" share
    /// a bucket, separate from "Business Street" + "Business City",
    /// and the numbered dialect's "Address 1 - …" / "Address 2 - …"
    /// stay two addresses via the digit in the header. `kind` is the
    /// vCard TYPE hint emitted on the ADR.
    Addr {
        part: AddrPart,
        bucket: String,
        kind: String,
    },
    Ignore,
}

enum AddrPart {
    Street,
    Locality,
    Region,
    PostalCode,
    Country,
}

/// Map a header cell to a role. Matching is case-insensitive and
/// covers the two dominant export dialects — the spelled-out style
/// ("First Name" / "E-mail Address" / "Home Street") and the
/// numbered style ("Given Name" / "E-mail 1 - Value" / "Phone 1 -
/// Value") — plus their German equivalents, since localised
/// spreadsheet tools translate the headers too.
fn classify_header(header: &str) -> ColumnRole {
    let h = header.trim().to_lowercase();
    if h.is_empty() {
        return ColumnRole::Ignore;
    }

    // The numbered dialect pairs every value column with a "- Type" /
    // "- Label" sibling; those carry no value and would otherwise
    // false-positive the keyword matching below.
    if h.ends_with("type") || h.ends_with("label") {
        return ColumnRole::Ignore;
    }

    // Emails before names: "e-mail display name" (a real header in
    // the spelled-out dialect) contains "name" but is not one.
    if h.contains("e-mail") || h.contains("email") {
        return if h.contains("display") {
            ColumnRole::Ignore
        } else {
            ColumnRole::Email {
                kind: kind_hint(&h),
            }
        };
    }
    if h.contains("phone")
        || h.contains("telefon")
        || h.contains("mobile")
        || h.contains("mobil")
        || h.contains("handy")
        || h.contains("fax")
    {
        return ColumnRole::Phone {
            kind: kind_hint(&h),
        };
    }

    // Address parts, bucketed so sibling columns reassemble into
    // whole addresses (see `ColumnRole::Addr`).
    let addr = |part: AddrPart| {
        let kind = kind_hint(&h);
        // Digits in the header separate "Address 1 - Street" from
        // "Address 2 - Street"; without any, the kind alone buckets.
        let index: String = h.chars().filter(char::is_ascii_digit).collect();
        ColumnRole::Addr {
            part,
            bucket: format!("{kind}#{index}"),
            kind,
        }
    };
    if h.contains("street") || h.contains("straße") || h.contains("strasse") {
        return addr(AddrPart::Street);
    }
    if h.contains("city") || (h.contains("ort") && !h.contains("sort") && !h.contains("geburtsort"))
    {
        return addr(AddrPart::Locality);
    }
    if h.contains("state") || h.contains("region") || h.contains("bundesland") {
        return addr(AddrPart::Region);
    }
    if h.contains("zip")
        || h.contains("postal code")
        || h.contains("plz")
        || h.contains("postleitzahl")
    {
        return addr(AddrPart::PostalCode);
    }
    if h.contains("country") || (h.contains("land") && !h.contains("bundesland")) {
        return addr(AddrPart::Country);
    }

    match h.as_str() {
        "name" | "display name" | "full name" | "anzeigename" => return ColumnRole::DisplayName,
        // Bare "title" is the courtesy title (name prefix) in the
        // spelled-out dialect; the job title spells itself out.
        "title" | "anrede" => return ColumnRole::NamePrefix,
        "suffix" | "name suffix" | "namenssuffix" => return ColumnRole::NameSuffix,
        _ => {}
    }
    // Middle-name headers before given-name: "Weitere Vornamen"
    // contains "vorname" and must not classify as the given name.
    if h.contains("middle name") || h.contains("additional name") || h.contains("weitere vornamen")
    {
        return ColumnRole::AdditionalName;
    }
    if h.contains("first name") || h.contains("given name") || h.contains("vorname") {
        return ColumnRole::GivenName;
    }
    if h.contains("last name")
        || h.contains("family name")
        || h.contains("surname")
        || h.contains("nachname")
    {
        return ColumnRole::FamilyName;
    }
    if h.contains("name prefix") || h.contains("präfix") {
        return ColumnRole::NamePrefix;
    }
    if h.contains("name suffix") || h.contains("suffix") {
        return ColumnRole::NameSuffix;
    }
    if h.contains("nickname") || h.contains("spitzname") {
        return ColumnRole::Nickname;
    }
    if h.contains("job title") || h.contains("position") || h.contains("beruf") {
        return ColumnRole::JobTitle;
    }
    if h.contains("organization")
        || h.contains("organisation")
        || h.contains("company")
        || h.contains("firma")
    {
        // "Organization 1 - Name" carries the value; department /
        // title siblings of the numbered dialect fall through to
        // Ignore via the type/label rule or simply don't match.
        return if h.contains("department") || h.contains("abteilung") {
            ColumnRole::Ignore
        } else {
            ColumnRole::Organization
        };
    }
    if h.contains("birthday") || h.contains("geburtstag") {
        return ColumnRole::Birthday;
    }
    if h.contains("note") || h.contains("notiz") {
        return ColumnRole::Note;
    }
    if h.contains("web page") || h.contains("website") || h.contains("webseite") || h == "url" {
        return ColumnRole::Url;
    }
    ColumnRole::Ignore
}

/// Pull a vCard TYPE hint out of a header's wording.
fn kind_hint(h: &str) -> String {
    if h.contains("mobile") || h.contains("mobil") || h.contains("handy") || h.contains("cell") {
        "cell".to_string()
    } else if h.contains("fax") {
        "fax".to_string()
    } else if h.contains("home") || h.contains("privat") {
        "home".to_string()
    } else if h.contains("business") || h.contains("work") || h.contains("geschäftlich") {
        "work".to_string()
    } else {
        "other".to_string()
    }
}

// ── Row assembly ────────────────────────────────────────────────

/// Build one card from a record. Returns `None` when the row carries
/// neither any name part nor an email — nothing to key a contact on.
fn row_to_card(columns: &[ColumnRole], fields: &[String]) -> Option<ParsedVcard> {
    let mut card = ParsedVcard::default();
    let mut name = VcardStructuredName::default();
    // bucket → partial address, in first-seen order so the emitted
    // ADR order matches the file's column order.
    let mut addresses: Vec<(String, VcardAddress)> = Vec::new();

    fn addr_for<'a>(
        list: &'a mut Vec<(String, VcardAddress)>,
        bucket: &str,
        kind: &str,
    ) -> &'a mut VcardAddress {
        if let Some(i) = list.iter().position(|(k, _)| k == bucket) {
            return &mut list[i].1;
        }
        list.push((
            bucket.to_string(),
            VcardAddress {
                kind: kind.to_string(),
                ..Default::default()
            },
        ));
        &mut list.last_mut().unwrap().1
    }

    for (role, raw) in columns.iter().zip(fields.iter()) {
        let value = raw.trim();
        if value.is_empty() {
            continue;
        }
        match role {
            ColumnRole::DisplayName => card.display_name = value.to_string(),
            ColumnRole::GivenName => name.given = value.to_string(),
            ColumnRole::FamilyName => name.family = value.to_string(),
            ColumnRole::AdditionalName => name.additional = value.to_string(),
            ColumnRole::NamePrefix => name.prefix = value.to_string(),
            ColumnRole::NameSuffix => name.suffix = value.to_string(),
            ColumnRole::Nickname => card.nickname = Some(value.to_string()),
            ColumnRole::Email { kind } => {
                // The numbered dialect packs multiple addresses into
                // one cell separated by " ::: ".
                for part in value.split(" ::: ") {
                    let v = part.trim();
                    if !v.is_empty() && !card.emails.iter().any(|e| e.value == v) {
                        card.emails.push(VcardEmail {
                            kind: kind.clone(),
                            value: v.to_string(),
                        });
                    }
                }
            }
            ColumnRole::Phone { kind } => {
                for part in value.split(" ::: ") {
                    let v = part.trim();
                    if !v.is_empty() && !card.phones.iter().any(|p| p.value == v) {
                        card.phones.push(VcardPhone {
                            kind: kind.clone(),
                            value: v.to_string(),
                        });
                    }
                }
            }
            ColumnRole::Organization => card.organization = Some(value.to_string()),
            ColumnRole::JobTitle => card.title = Some(value.to_string()),
            ColumnRole::Birthday => card.birthday = Some(value.to_string()),
            ColumnRole::Note => card.note = Some(value.to_string()),
            ColumnRole::Url => {
                if !card.urls.iter().any(|u| u == value) {
                    card.urls.push(value.to_string());
                }
            }
            ColumnRole::Addr { part, bucket, kind } => {
                let a = addr_for(&mut addresses, bucket, kind);
                match part {
                    AddrPart::Street => a.street = value.to_string(),
                    AddrPart::Locality => a.locality = value.to_string(),
                    AddrPart::Region => a.region = value.to_string(),
                    AddrPart::PostalCode => a.postal_code = value.to_string(),
                    AddrPart::Country => a.country = value.to_string(),
                }
            }
            ColumnRole::Ignore => {}
        }
    }

    card.addresses = addresses.into_iter().map(|(_, a)| a).collect();

    // FN fallback chain, mirroring the create-form convention:
    // explicit display-name column → assembled structured name →
    // first email. A row with none of those isn't a contact.
    if card.display_name.trim().is_empty() {
        let assembled = [
            name.prefix.as_str(),
            name.given.as_str(),
            name.additional.as_str(),
            name.family.as_str(),
            name.suffix.as_str(),
        ]
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<&str>>()
        .join(" ");
        if !assembled.is_empty() {
            card.display_name = assembled;
        } else if let Some(first) = card.emails.first() {
            card.display_name = first.value.clone();
        } else {
            return None;
        }
    }
    card.structured_name = name;
    Some(card)
}

// ── Legacy-encoding fallback ────────────────────────────────────

/// Decode file bytes to text: UTF-8 when valid, otherwise Windows-1252
/// — the encoding legacy spreadsheet exports on Windows actually use
/// (a superset of Latin-1 in the 0x80–0x9F range). Without this, an
/// umlaut in an older export makes the whole import fail with an
/// unhelpful "invalid UTF-8" error.
pub fn decode_import_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252_char(b)).collect(),
    }
}

/// One byte of Windows-1252 → char. Identical to Latin-1 except the
/// 0x80–0x9F block, which maps to printable punctuation/letters.
fn cp1252_char(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9f => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cards(text: &str) -> Vec<ParsedVcard> {
        parse_csv_contacts(text)
            .unwrap()
            .into_iter()
            .filter_map(|o| match o {
                CsvRowOutcome::Card(c) => Some(*c),
                CsvRowOutcome::Skipped { .. } => None,
            })
            .collect()
    }

    #[test]
    fn parses_spelled_out_dialect() {
        let text = "First Name,Last Name,E-mail Address,Mobile Phone,Company,Job Title\r\n\
                    Alex,Morgan,alex@example.com,+1 555 0100,Example Corp,Engineer\r\n";
        let c = cards(text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].display_name, "Alex Morgan");
        assert_eq!(c[0].structured_name.given, "Alex");
        assert_eq!(c[0].structured_name.family, "Morgan");
        assert_eq!(c[0].emails[0].value, "alex@example.com");
        assert_eq!(c[0].phones[0].kind, "cell");
        assert_eq!(c[0].organization.as_deref(), Some("Example Corp"));
        assert_eq!(c[0].title.as_deref(), Some("Engineer"));
    }

    #[test]
    fn parses_numbered_dialect_with_type_columns() {
        let text = "Name,Given Name,Family Name,E-mail 1 - Type,E-mail 1 - Value,Phone 1 - Type,Phone 1 - Value\n\
                    Jane Smith,Jane,Smith,Home,jane@example.com,Mobile,+1 555 0200\n";
        let c = cards(text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].display_name, "Jane Smith");
        // The "- Type" columns are metadata, not values — they must
        // not become extra emails/phones.
        assert_eq!(c[0].emails.len(), 1);
        assert_eq!(c[0].emails[0].value, "jane@example.com");
        assert_eq!(c[0].phones.len(), 1);
    }

    #[test]
    fn quoted_fields_keep_delimiters_and_newlines() {
        let text = "Name,E-mail Address,Notes\n\
                    \"Smith, Jane\",jane@example.com,\"line one\nline two, with comma\"\n";
        let c = cards(text);
        assert_eq!(c[0].display_name, "Smith, Jane");
        assert_eq!(c[0].note.as_deref(), Some("line one\nline two, with comma"));
    }

    #[test]
    fn doubled_quotes_are_literal() {
        let text = "Name,E-mail Address\n\"Jane \"\"JJ\"\" Smith\",jj@example.com\n";
        let c = cards(text);
        assert_eq!(c[0].display_name, "Jane \"JJ\" Smith");
    }

    #[test]
    fn semicolon_delimiter_is_sniffed() {
        let text = "Vorname;Nachname;E-Mail-Adresse\nJürgen;Müller;jm@example.com\n";
        let c = cards(text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].display_name, "Jürgen Müller");
        assert_eq!(c[0].emails[0].value, "jm@example.com");
    }

    #[test]
    fn home_and_business_addresses_stay_separate() {
        let text = "Name,Home Street,Home City,Home Postal Code,Business Street,Business City\n\
                    Alex Morgan,Elm St 1,Springfield,12345,Main St 9,Shelbyville\n";
        let c = cards(text);
        assert_eq!(c[0].addresses.len(), 2);
        assert_eq!(c[0].addresses[0].kind, "home");
        assert_eq!(c[0].addresses[0].street, "Elm St 1");
        assert_eq!(c[0].addresses[0].postal_code, "12345");
        assert_eq!(c[0].addresses[1].kind, "work");
        assert_eq!(c[0].addresses[1].locality, "Shelbyville");
    }

    #[test]
    fn nameless_email_row_uses_email_as_display_name() {
        let text = "Name,E-mail Address\n,solo@example.com\n";
        let c = cards(text);
        assert_eq!(c[0].display_name, "solo@example.com");
    }

    #[test]
    fn unusable_rows_are_reported_not_fatal() {
        let text = "Name,E-mail Address,Notes\n,,just a note\nJane,jane@example.com,\n";
        let outcomes = parse_csv_contacts(text).unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(matches!(
            outcomes[0],
            CsvRowOutcome::Skipped { line: 2, .. }
        ));
        assert!(matches!(outcomes[1], CsvRowOutcome::Card(_)));
    }

    #[test]
    fn empty_and_headerless_files_error() {
        assert!(parse_csv_contacts("").is_err());
        assert!(parse_csv_contacts("foo,bar\n1,2\n").is_err());
    }

    #[test]
    fn blank_lines_are_dropped_silently() {
        let text = "Name,E-mail Address\nJane,jane@example.com\n\n\n";
        let outcomes = parse_csv_contacts(text).unwrap();
        assert_eq!(outcomes.len(), 1);
    }

    #[test]
    fn cp1252_fallback_decodes_umlauts() {
        // "Jürgen" with a Latin-1/CP-1252 ü (0xFC) — invalid UTF-8.
        let bytes = b"Name,E-mail Address\nJ\xfcrgen,ju@example.com\n";
        #[allow(invalid_from_utf8)]
        let premise = std::str::from_utf8(bytes);
        assert!(premise.is_err());
        let text = decode_import_text(bytes);
        let c = cards(&text);
        assert_eq!(c[0].display_name, "Jürgen");
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let text = "\u{feff}Name,E-mail Address\nJane,jane@example.com\n";
        let c = cards(text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].display_name, "Jane");
    }
}
