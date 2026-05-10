//! Local cache for geocoding results (#280).
//!
//! Powers the EventEditor's location autocomplete — every search
//! that hits a geocoder (Nominatim today, possibly NC Maps later)
//! also lands in this table so the same query typed twice doesn't
//! spend two upstream API calls.  Critical for Nominatim in
//! particular, whose usage policy caps clients at ~1 req/sec.
//!
//! # Schema
//!
//! Defined in `schema.rs` migration v29:
//!
//! ```sql
//! CREATE TABLE geocode_cache (
//!   query        TEXT NOT NULL,
//!   lang         TEXT NOT NULL DEFAULT '',
//!   results_json TEXT NOT NULL,
//!   cached_at    INTEGER NOT NULL,
//!   PRIMARY KEY (query, lang)
//! );
//! ```
//!
//! Cache rows are immutable for a TTL — see [`CACHE_TTL_SECS`] —
//! after which a hit triggers a refresh.  No background eviction;
//! the table is small (one row per distinct query) and rusqlite
//! handles the `INSERT OR REPLACE` cheaply.

use chrono::Utc;
use rusqlite::{OptionalExtension, params};

use super::{Cache, CacheError};

/// Hours a geocode-cache row is considered fresh.  Conservative on
/// purpose — geocoding addresses don't change often and a stale
/// `display_name` is harmless for the autocomplete UX.
pub const CACHE_TTL_SECS: i64 = 7 * 24 * 60 * 60; // 7 days

impl Cache {
    /// Look up a previous geocode result for `(query, lang)`.
    /// Returns `Ok(Some(json))` if a fresh row exists, `Ok(None)`
    /// if the row is missing or older than [`CACHE_TTL_SECS`].
    /// The returned blob is the raw JSON the caller stored — we
    /// don't deserialise here so the cache layer stays decoupled
    /// from the geocoding-result shape.
    pub fn get_geocode_cache(&self, query: &str, lang: &str) -> Result<Option<String>, CacheError> {
        let conn = self.conn()?;
        let cutoff = Utc::now().timestamp() - CACHE_TTL_SECS;
        let key_query = canonicalise_query(query);
        let key_lang = lang.trim().to_ascii_lowercase();
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT results_json, cached_at FROM geocode_cache
                 WHERE query = ?1 AND lang = ?2",
                params![key_query, key_lang],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        match row {
            Some((json, cached_at)) if cached_at >= cutoff => Ok(Some(json)),
            _ => Ok(None),
        }
    }

    /// Persist a geocode result.  `results_json` is whatever the
    /// caller wants to round-trip through the cache — normally a
    /// `Vec<GeocodeResult>` serialised with `serde_json`.  Replaces
    /// any existing row for the same `(query, lang)` so a refresh
    /// after the TTL doesn't leave stale data behind.
    pub fn put_geocode_cache(
        &self,
        query: &str,
        lang: &str,
        results_json: &str,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let key_query = canonicalise_query(query);
        let key_lang = lang.trim().to_ascii_lowercase();
        conn.execute(
            "INSERT OR REPLACE INTO geocode_cache
                (query, lang, results_json, cached_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![key_query, key_lang, results_json, Utc::now().timestamp()],
        )?;
        Ok(())
    }
}

/// Normalise a query for cache lookup: trim, collapse interior
/// whitespace, lower-case.  Two visually-equivalent searches
/// (`Café Hartmann` vs `café  hartmann`) collapse to the same
/// row so we don't burn an API call on stylistic typos.
fn canonicalise_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.extend(c.to_lowercase());
            last_was_space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalise_collapses_whitespace_and_lowercases() {
        assert_eq!(canonicalise_query("  Café   Hartmann "), "café hartmann");
        assert_eq!(canonicalise_query("CAFÉ"), "café");
    }

    #[test]
    fn cache_roundtrip_within_ttl() {
        let cache = Cache::open_in_memory().expect("open in-memory cache");
        cache
            .put_geocode_cache("Café Hartmann", "en", r#"[{"display_name":"x"}]"#)
            .expect("put");
        let got = cache
            .get_geocode_cache("café   hartmann", "EN")
            .expect("get")
            .expect("hit");
        assert!(got.contains("display_name"));
    }

    #[test]
    fn cache_miss_for_different_lang() {
        let cache = Cache::open_in_memory().expect("open in-memory cache");
        cache.put_geocode_cache("Café", "en", r#"[]"#).expect("put");
        let got = cache.get_geocode_cache("Café", "de").expect("get");
        assert!(got.is_none());
    }
}
