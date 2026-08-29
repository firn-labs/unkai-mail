//! URLhaus-backed link-safety check (#165).
//!
//! Stores a periodically-refreshed snapshot of abuse.ch's URLhaus
//! "online malicious URLs" feed inside the machine-level
//! [`SharedCache`] (`shared.db`, #532 — one snapshot and one hourly
//! download per machine, regardless of how many profiles exist) and
//! exposes a small lookup API.  Two questions matter:
//!
//!   - **Is this exact URL on URLhaus?** — strongest signal; the
//!     URL has been observed serving malware or phishing.
//!   - **Has this host been on URLhaus?** — weaker signal; the
//!     domain has hosted malicious content in the past.  v1 of
//!     the UI collapses both into a single "unsafe" verdict;
//!     keeping the distinction in the API leaves room for a
//!     future "caution" tier without a schema change.
//!
//! Lookups are case-insensitive on host and trim a single
//! trailing slash on the path so cosmetic differences between
//! `https://evil.example.com/foo` and
//! `https://evil.example.com/foo/` don't desynchronise.  The
//! match isn't a security boundary — a determined attacker can
//! always permute the path to evade an exact-string list — but
//! it's the contract of the upstream feed and it's the right
//! tier of paranoia for a "warn the user before they click"
//! affordance.

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::SharedCache;
use crate::cache::CacheError;

/// One URLhaus row matched against a candidate URL.  Returned
/// from `lookup` so callers know *which* dimension matched and
/// can render a different pill colour / hover hint per tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlhausMatch {
    /// `true` if the full URL was on the list; `false` if only
    /// the host matched (i.e. the same domain has hosted other
    /// malicious URLs but this exact URL isn't currently flagged).
    pub exact: bool,
    /// URLhaus' `threat` column — typically `"malware_download"`
    /// or `"phishing"`.  Plain-text passthrough.
    pub threat: String,
    /// URLhaus' `tags` column — comma-separated list of tags
    /// like `"emotet,exe"`.  Plain-text passthrough; the UI
    /// renders this verbatim in a tooltip.
    pub tags: String,
}

/// Snapshot summary for the Settings UI — how many URLs we
/// currently know about and when the local copy was last
/// refreshed.  `None` for `last_refreshed_at` means "we've
/// never successfully refreshed", which the UI surfaces as
/// "Never" rather than the unix epoch zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlhausStatus {
    pub total_urls: u32,
    pub last_refreshed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Look up a URL in the local URLhaus snapshot.  Returns `None`
/// for "no known threat indicators" (i.e. the safe path).
///
/// Two lookups in sequence:
///   1. Exact-URL match against the primary key.
///   2. Host-only match against the `host` index.
///
/// Both are O(1) / O(log n) and the index is small enough
/// (typically a few thousand URLs) that the full-table size
/// stays well within SQLite's page cache.
pub fn lookup(shared: &SharedCache, url: &str) -> Result<Option<UrlhausMatch>, CacheError> {
    let trimmed = trim_url(url);
    let host = match extract_host(&trimmed) {
        Some(h) => h,
        // Couldn't parse a host — nothing to match against, so
        // we report "safe" and let the UI render the green pill.
        // A weird URL is much more likely to be a `mailto:` /
        // `tel:` / `javascript:` than something URLhaus would
        // flag anyway.
        None => return Ok(None),
    };

    let conn = shared.conn()?;

    // Exact URL match first — it's the stronger signal and
    // costs nothing to check.
    if let Some((threat, tags)) = conn
        .query_row(
            "SELECT threat, tags FROM urlhaus_urls WHERE url = ?1",
            params![&trimmed],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        return Ok(Some(UrlhausMatch {
            exact: true,
            threat,
            tags,
        }));
    }

    // Fall back to host-only match.  We pick the most recent
    // entry by `date_added` so the surfaced threat label
    // reflects current activity rather than ancient history.
    let host_match = conn
        .query_row(
            "SELECT threat, tags
             FROM urlhaus_urls
             WHERE host = ?1
             ORDER BY date_added DESC
             LIMIT 1",
            params![&host],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;

    Ok(host_match.map(|(threat, tags)| UrlhausMatch {
        exact: false,
        threat,
        tags,
    }))
}

/// Replace the entire URLhaus snapshot in one transaction.
/// Called by the background refresh worker after a successful
/// CSV download — the safest atomic-update story for "wholesale
/// list replacement" data.
///
/// `entries` is the parsed CSV; rows with an unparseable URL or
/// missing host are silently skipped (the upstream CSV
/// occasionally carries malformed lines).
pub fn replace_all(shared: &SharedCache, entries: &[UrlhausCsvRow]) -> Result<u32, CacheError> {
    let now = Utc::now().timestamp();
    let mut conn = shared.conn()?;
    let tx = conn.transaction()?;

    tx.execute("DELETE FROM urlhaus_urls", [])?;

    let mut inserted: u32 = 0;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO urlhaus_urls
                    (url, host, threat, tags, date_added, last_refreshed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;

        for entry in entries {
            let trimmed = trim_url(&entry.url);
            let Some(host) = extract_host(&trimmed) else {
                continue;
            };
            stmt.execute(params![
                &trimmed,
                &host,
                &entry.threat,
                &entry.tags,
                entry.date_added,
                now,
            ])?;
            inserted += 1;
        }
    }

    tx.execute(
        "INSERT INTO urlhaus_meta (key, value) VALUES ('last_refreshed_at', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![now.to_string()],
    )?;

    tx.commit()?;

    debug!("URLhaus snapshot replaced: {} URL(s)", inserted);
    Ok(inserted)
}

/// Count rows in the snapshot whose `host` column matches a
/// candidate URL's host.  Used by the diagnostic IPC (#165) to
/// tell "URL not in URLhaus" apart from "host known but exact +
/// fallback both missed".  Returns 0 when the URL has no
/// extractable host.
pub fn host_count_for_url(shared: &SharedCache, url: &str) -> Result<u32, CacheError> {
    let Some(host) = extract_host(&trim_url(url)) else {
        return Ok(0);
    };
    let conn = shared.conn()?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM urlhaus_urls WHERE host = ?1",
        params![&host],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

/// Read the snapshot status — used by the Settings UI to render
/// "{n} URLs, last refreshed N hours ago".
pub fn status(shared: &SharedCache) -> Result<UrlhausStatus, CacheError> {
    let conn = shared.conn()?;
    let total: u32 = conn.query_row("SELECT COUNT(*) FROM urlhaus_urls", [], |r| {
        r.get::<_, i64>(0).map(|n| n as u32)
    })?;

    let last_refreshed_epoch: Option<i64> = conn
        .query_row(
            "SELECT value FROM urlhaus_meta WHERE key = 'last_refreshed_at'",
            [],
            |r| {
                let s: String = r.get(0)?;
                Ok(s.parse::<i64>().ok())
            },
        )
        .optional()?
        .flatten();

    Ok(UrlhausStatus {
        total_urls: total,
        last_refreshed_at: last_refreshed_epoch
            .and_then(|epoch| chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0)),
    })
}

/// One row parsed from the URLhaus CSV.  Public so the
/// http-fetcher (in `unkai-app`, where the http client lives)
/// can build these and hand them to `replace_all`.
#[derive(Debug, Clone)]
pub struct UrlhausCsvRow {
    pub url: String,
    pub threat: String,
    pub tags: String,
    /// `dateadded` column from URLhaus, parsed to unix epoch
    /// seconds.  Used as the tiebreaker in host-only matches.
    pub date_added: i64,
}

fn trim_url(url: &str) -> String {
    let trimmed = url.trim();
    // Drop a single trailing `/` so two URLs that differ only in
    // that position land on the same row.  Not aggressive about
    // case-folding the path — URLhaus entries are typically
    // case-sensitive (an attacker may serve different content
    // from `/Pay` vs `/pay`).
    if trimmed.ends_with('/') && trimmed.len() > 1 {
        trimmed[..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn extract_host(url: &str) -> Option<String> {
    // We don't need a full URL parser here — a hand-rolled
    // scheme-strip + host-up-to-(`:` or `/` or `?` or `#`) is
    // enough for the URLhaus format and saves us a `url` crate
    // dependency for one function.
    let after_scheme = if let Some(idx) = url.find("://") {
        &url[idx + 3..]
    } else {
        url
    };
    if after_scheme.is_empty() {
        return None;
    }
    let end = after_scheme
        .find(['/', '?', '#', ':'])
        .unwrap_or(after_scheme.len());
    let host = &after_scheme[..end];
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_host_strips_scheme_and_trailing_path() {
        assert_eq!(
            extract_host("https://evil.example.com/foo/bar?q=1"),
            Some("evil.example.com".into())
        );
        assert_eq!(
            extract_host("http://Evil.Example.COM:8080/x"),
            Some("evil.example.com".into())
        );
        assert_eq!(extract_host("//no-scheme.example.com/x"), None);
        assert_eq!(extract_host(""), None);
    }

    #[test]
    fn trim_url_drops_single_trailing_slash() {
        assert_eq!(
            trim_url("https://x.example.com/foo/"),
            "https://x.example.com/foo"
        );
        assert_eq!(
            trim_url("https://x.example.com/foo"),
            "https://x.example.com/foo"
        );
        // Don't strip the lone `/` that's the path itself —
        // that would conflate two distinct URLs.
        assert_eq!(trim_url("/"), "/");
    }

    fn open_test_cache() -> SharedCache {
        SharedCache::open_in_memory().expect("in-memory shared cache")
    }

    #[test]
    fn lookup_finds_exact_url() {
        let cache = open_test_cache();
        replace_all(
            &cache,
            &[UrlhausCsvRow {
                url: "https://evil.example.com/payload.exe".into(),
                threat: "malware_download".into(),
                tags: "emotet,exe".into(),
                date_added: 1_700_000_000,
            }],
        )
        .expect("replace");
        let m = lookup(&cache, "https://evil.example.com/payload.exe")
            .expect("lookup")
            .expect("hit");
        assert!(m.exact);
        assert_eq!(m.threat, "malware_download");
    }

    #[test]
    fn lookup_falls_back_to_host_match() {
        let cache = open_test_cache();
        replace_all(
            &cache,
            &[UrlhausCsvRow {
                url: "https://evil.example.com/payload.exe".into(),
                threat: "malware_download".into(),
                tags: "emotet".into(),
                date_added: 1_700_000_000,
            }],
        )
        .expect("replace");
        let m = lookup(&cache, "https://evil.example.com/different-path")
            .expect("lookup")
            .expect("host hit");
        assert!(!m.exact);
        assert_eq!(m.threat, "malware_download");
    }

    #[test]
    fn lookup_returns_none_for_safe_url() {
        let cache = open_test_cache();
        replace_all(&cache, &[]).expect("replace");
        let m = lookup(&cache, "https://example.com/").expect("lookup");
        assert!(m.is_none());
    }

    #[test]
    fn status_reflects_replace_all() {
        let cache = open_test_cache();
        replace_all(
            &cache,
            &[
                UrlhausCsvRow {
                    url: "https://a.example.com/x".into(),
                    threat: "malware_download".into(),
                    tags: String::new(),
                    date_added: 1_700_000_000,
                },
                UrlhausCsvRow {
                    url: "https://b.example.com/y".into(),
                    threat: "phishing".into(),
                    tags: String::new(),
                    date_added: 1_700_000_001,
                },
            ],
        )
        .expect("replace");
        let s = status(&cache).expect("status");
        assert_eq!(s.total_urls, 2);
        assert!(s.last_refreshed_at.is_some());
    }
}
