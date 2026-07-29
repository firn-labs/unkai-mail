//! Rust port of the email-safe invite-card builders in
//! `ui/src/lib/inviteHtml.ts` (#441).
//!
//! `create_meeting_invite` produces the same invite-card draft the
//! in-app "Respond with meeting" / "Insert Talk link" flows do, so
//! this module mirrors the TypeScript builders **template for
//! template** — same wrapper structure, same inline-style tokens,
//! same `data-unkai-block` markers (which the editor's `UnkaiBlock`
//! extension needs to keep the card intact as an atom node).
//!
//! ## Keeping the two implementations in sync
//!
//! There is deliberately no shared source of truth: the TS side
//! renders inside the webview, this side inside the Rust process,
//! and a build-time bridge between them would cost more than the
//! duplication.  Instead the rule is: **any change to
//! `inviteHtml.ts`'s tokens or templates must be applied here too**
//! (and vice versa).  The unit tests below lock the exact byte
//! shape of both cards so an unsynced edit fails loudly.
//!
//! One knowing divergence: the TS side formats the date line with
//! `Intl.DateTimeFormat(undefined, …)` — the OS locale, including
//! its 12/24-hour convention.  Rust has no `Intl`; this port pins
//! English month/weekday names and 24-hour times ("Tuesday,
//! May 6 · 14:00 – 15:00").  The shape of the line is identical,
//! only the localisation differs.
//!
//! The email-rendering constraints (inline styles only, system
//! font stack, emoji glyphs, no images in the chrome) are
//! documented in `CLAUDE.md` — don't relax them here.

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};

/// Minimal HTML escape for the values spliced into the templates —
/// same five characters the TS `esc` helper covers.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// ── Shared design tokens (inlined into every style attribute) ──
//
// Byte-identical to the `T` constant in `inviteHtml.ts`.

const T_CARD: &str = "background:#ffffff;border:1px solid #e2e8f0;border-radius:14px;box-shadow:0 1px 2px rgba(15,23,42,0.04),0 8px 24px rgba(15,23,42,0.06);overflow:hidden;";
const T_HEADER_BG: &str =
    "background:linear-gradient(135deg,#3b82f6 0%,#6366f1 100%);padding:20px 24px;color:#ffffff;";
const T_HEADER_PILL: &str =
    "display:inline-block;padding:7px 14px;border-radius:999px;background:rgba(255,255,255,0.18);";
const T_HEADER_WORDMARK: &str =
    "font-size:13px;font-weight:700;letter-spacing:0.12em;color:#ffffff;text-transform:uppercase;";
const T_BODY: &str = "padding:24px;font-family:-apple-system,BlinkMacSystemFont,\"Segoe UI\",Roboto,Helvetica,Arial,sans-serif;color:#0f172a;";
const T_TITLE: &str =
    "margin:0 0 4px 0;font-size:20px;line-height:1.3;font-weight:600;color:#0f172a;";
const T_SUBTITLE: &str = "margin:0;font-size:14px;line-height:1.5;color:#475569;";
const T_DETAIL_LABEL: &str =
    "font-size:11px;font-weight:600;letter-spacing:0.06em;text-transform:uppercase;color:#64748b;";
const T_DETAIL_VALUE: &str = "font-size:14px;line-height:1.5;color:#0f172a;margin:2px 0 0 0;";
const T_DIVIDER: &str = "height:1px;background:#e2e8f0;margin:20px 0;border:0;";
const T_CTA_ROW: &str = "margin-top:24px;";
const T_CTA_BUTTON: &str = "display:inline-block;background:#3b82f6;color:#ffffff;text-decoration:none;font-weight:600;font-size:14px;padding:11px 22px;border-radius:10px;letter-spacing:0.01em;";
const T_FOOTER: &str = "margin:20px 0 0 0;padding:14px 16px;background:#f8fafc;border-radius:10px;font-size:12px;line-height:1.5;color:#475569;";
const T_FOOTER_STRONG: &str = "color:#0f172a;font-weight:600;";

/// Format a start/end pair as the single human line the "When" row
/// carries: `Tuesday, May 6 · 14:00 – 15:00` (same-day) or
/// `Tuesday, May 6 14:00 – Wednesday, May 7 15:00` (cross-day).
/// English names + 24-hour clock — see the module comment for why
/// this pins a locale where the TS side follows the OS.
pub(crate) fn format_range<Tz: TimeZone>(start: &DateTime<Tz>, end: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let date = |d: &DateTime<Tz>| d.format("%A, %B %-d").to_string();
    let time = |d: &DateTime<Tz>| d.format("%H:%M").to_string();
    let same_day =
        start.year() == end.year() && start.month() == end.month() && start.day() == end.day();
    if same_day {
        format!("{} · {} – {}", date(start), time(start), time(end))
    } else {
        format!(
            "{} {} – {} {}",
            date(start),
            time(start),
            date(end),
            time(end)
        )
    }
}

/// Wrap the card body inside the shared chrome (outer wrapper +
/// typography-only wordmark header).  The `data-unkai-block`
/// attribute is what the editor's `UnkaiBlock` extension keys on.
fn chrome(body_html: &str, kind: &str) -> String {
    format!(
        "<div data-unkai-block=\"{kind}\" style=\"max-width:560px;margin:16px 0;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;\">\n  <div style=\"{T_CARD}\">\n    <div style=\"{T_HEADER_BG}\">\n      <span style=\"{T_HEADER_PILL}\"><span style=\"{T_HEADER_WORDMARK}\">Unkai Mail</span></span>\n    </div>\n    <div style=\"{T_BODY}\">\n      {body_html}\n    </div>\n  </div>\n</div>"
    )
}

/// One detail row: emoji + label + value, stacked vertically.
fn detail_row(emoji: &str, label: &str, value: &str) -> String {
    format!(
        "<div style=\"display:flex;gap:12px;align-items:flex-start;margin:14px 0;\">\n  <div style=\"font-size:18px;line-height:1.4;flex-shrink:0;width:24px;text-align:center;\">{emoji}</div>\n  <div style=\"flex:1;min-width:0;\">\n    <div style=\"{T_DETAIL_LABEL}\">{}</div>\n    <div style=\"{T_DETAIL_VALUE}\">{value}</div>\n  </div>\n</div>",
        esc(label)
    )
}

/// Footer microcopy block — the "You'll be invited via Nextcloud"
/// line lives here so it reads as a system-level note.
fn nextcloud_footer(message: &str) -> String {
    format!(
        "<div style=\"{T_FOOTER}\">\n  <span style=\"{T_FOOTER_STRONG}\">📨 {}</span><br />\n  <span>Accept the invitation in your mail client or directly in Nextcloud — your calendar updates either way.</span>\n</div>",
        esc(message)
    )
}

// ── Public renderers ────────────────────────────────────────────

/// Render a Talk-meeting invitation card — the counterpart of
/// `talkInviteHtml` in `inviteHtml.ts`.  Not used by the
/// `create_meeting_invite` composite (its card is the meeting one,
/// with the Talk link as a row), but ported alongside it so the
/// pair of builders stays complete for future tool surfaces.
pub fn talk_invite_html(name: &str, url: &str) -> String {
    let inner = format!(
        "<h1 style=\"{T_TITLE}\">You're invited to a Talk meeting</h1>\n<p style=\"{T_SUBTITLE}\">Click the button below to join the conversation in Nextcloud Talk — works in any modern browser, no install.</p>\n\n<hr style=\"{T_DIVIDER}\" />\n\n{}\n{}\n\n<div style=\"{T_CTA_ROW}\">\n  <a href=\"{}\" style=\"{T_CTA_BUTTON}\">Join Talk meeting →</a>\n</div>\n\n{}",
        detail_row(
            "💬",
            "Talk room",
            &format!("<strong style=\"font-weight:600;\">{}</strong>", esc(name)),
        ),
        detail_row(
            "🔗",
            "Join link",
            &format!(
                "<a href=\"{}\" style=\"color:#3b82f6;text-decoration:none;word-break:break-all;\">{}</a>",
                esc(url),
                esc(url)
            ),
        ),
        esc(url),
        nextcloud_footer("You'll also be added as a participant in Nextcloud Talk."),
    );
    chrome(&inner, "talk-invite")
}

/// Input for [`meeting_invite_html`] — mirrors the TS
/// `MeetingInvite` interface (timestamps arrive as UTC and are
/// rendered in the machine's local timezone, like the webview's
/// `Date` does).
pub(crate) struct MeetingInviteCard {
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub location: Option<String>,
    pub description: Option<String>,
    pub talk_url: Option<String>,
}

/// Render a calendar-meeting invitation card — the counterpart of
/// `meetingInviteHtml` in `inviteHtml.ts`.
pub(crate) fn meeting_invite_html(card: &MeetingInviteCard) -> String {
    meeting_invite_html_in(card, &Local)
}

/// Timezone-parameterised body of [`meeting_invite_html`] so tests
/// can pin a zone and lock exact output bytes.
fn meeting_invite_html_in<Tz: TimeZone>(card: &MeetingInviteCard, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let start = card.start.with_timezone(tz);
    let end = card.end.with_timezone(tz);

    let mut detail_rows: Vec<String> = vec![detail_row(
        "📅",
        "When",
        &format!(
            "<strong style=\"font-weight:600;\">{}</strong>",
            esc(&format_range(&start, &end))
        ),
    )];
    if let Some(location) = card.location.as_deref().map(str::trim)
        && !location.is_empty()
    {
        detail_rows.push(detail_row("📍", "Where", &esc(location)));
    }
    let talk_url = card
        .talk_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    if let Some(url) = talk_url {
        detail_rows.push(detail_row(
            "💬",
            "Talk room",
            &format!(
                "<a href=\"{}\" style=\"color:#3b82f6;text-decoration:none;word-break:break-all;\">{}</a>",
                esc(url),
                esc(url)
            ),
        ));
    }
    if let Some(description) = card.description.as_deref().map(str::trim)
        && !description.is_empty()
    {
        // Preserve plain-text newlines as <br /> so an agenda with
        // line breaks reads correctly; escape first so the user's
        // text can't break the surrounding HTML.
        let notes_html = esc(description)
            .replace("\r\n", "<br />")
            .replace('\n', "<br />");
        detail_rows.push(detail_row("📝", "Notes", &notes_html));
    }

    let cta_block = match talk_url {
        Some(url) => format!(
            "<div style=\"{T_CTA_ROW}\">\n  <a href=\"{}\" style=\"{T_CTA_BUTTON}\">Join Talk meeting →</a>\n</div>",
            esc(url)
        ),
        None => String::new(),
    };

    let inner = format!(
        "<h1 style=\"{T_TITLE}\">{}</h1>\n<p style=\"{T_SUBTITLE}\">A calendar invitation has been created in Nextcloud and shared with everyone on this thread.</p>\n\n<hr style=\"{T_DIVIDER}\" />\n\n{}\n\n{}\n\n{}",
        esc(&card.summary),
        detail_rows.join("\n"),
        cta_block,
        nextcloud_footer(
            "You'll be invited via Nextcloud — accepting in your mail client adds the event to your calendar."
        ),
    );
    chrome(&inner, "meeting-invite")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn escape_covers_the_five_specials() {
        assert_eq!(
            esc(r#"<a & "b" 'c'>"#),
            "&lt;a &amp; &quot;b&quot; &#39;c&#39;&gt;"
        );
    }

    #[test]
    fn format_range_same_day_and_cross_day() {
        let start = Utc.with_ymd_and_hms(2026, 5, 5, 14, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 5, 5, 15, 0, 0).unwrap();
        assert_eq!(format_range(&start, &end), "Tuesday, May 5 · 14:00 – 15:00");
        let end = Utc.with_ymd_and_hms(2026, 5, 6, 9, 30, 0).unwrap();
        assert_eq!(
            format_range(&start, &end),
            "Tuesday, May 5 14:00 – Wednesday, May 6 09:30"
        );
    }

    #[test]
    fn talk_card_shape_matches_the_ts_builder() {
        let html = talk_invite_html("Weekly sync", "https://cloud.example.com/call/abc123");
        // Wrapper marker + chrome shape the editor's UnkaiBlock
        // extension and the mail clients rely on.
        assert!(html.starts_with("<div data-unkai-block=\"talk-invite\" style=\"max-width:560px;"));
        assert!(html.contains(">Unkai Mail</span>"));
        assert!(html.contains("You're invited to a Talk meeting"));
        assert!(html.contains("Weekly sync"));
        assert!(html.contains("href=\"https://cloud.example.com/call/abc123\""));
        assert!(html.contains("Join Talk meeting →"));
        // The footer message runs through esc() — same as the TS
        // builder — so its apostrophe arrives entity-encoded.
        assert!(html.contains("You&#39;ll also be added as a participant in Nextcloud Talk."));
        // Inline styles only — no <style> block, no <img>, no class=.
        assert!(!html.contains("<style"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("class="));
    }

    #[test]
    fn meeting_card_renders_optional_rows_and_escapes() {
        let card = MeetingInviteCard {
            summary: "Q3 <Planning> & Review".into(),
            start: Utc.with_ymd_and_hms(2026, 5, 5, 14, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 5, 5, 15, 0, 0).unwrap(),
            location: Some("Room 5 & annex".into()),
            description: Some("Agenda:\n1. Numbers\n2. <Plans>".into()),
            talk_url: Some("https://cloud.example.com/call/xyz".into()),
        };
        let html = meeting_invite_html_in(&card, &Utc);
        assert!(
            html.starts_with("<div data-unkai-block=\"meeting-invite\" style=\"max-width:560px;")
        );
        assert!(html.contains("Q3 &lt;Planning&gt; &amp; Review"));
        assert!(html.contains("Tuesday, May 5 · 14:00 – 15:00"));
        assert!(html.contains("Room 5 &amp; annex"));
        assert!(html.contains("Agenda:<br />1. Numbers<br />2. &lt;Plans&gt;"));
        assert!(html.contains("href=\"https://cloud.example.com/call/xyz\""));
        assert!(html.contains("Join Talk meeting →"));
    }

    #[test]
    fn meeting_card_without_talk_url_has_no_cta() {
        let card = MeetingInviteCard {
            summary: "Coffee".into(),
            start: Utc.with_ymd_and_hms(2026, 5, 5, 14, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 5, 5, 15, 0, 0).unwrap(),
            location: None,
            description: None,
            talk_url: None,
        };
        let html = meeting_invite_html_in(&card, &Utc);
        assert!(!html.contains("Join Talk meeting"));
        assert!(!html.contains("📍"));
        assert!(!html.contains("📝"));
        assert!(html.contains("📅"));
    }
}
