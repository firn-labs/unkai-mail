//! Geocoding via Nominatim — the canonical OSM-backed geocoder
//! (#280).
//!
//! Fetches forward-geocoding suggestions for the EventEditor's
//! Location autocomplete.  Routes through the local
//! `geocode_cache` SQLite table (see `nimbus-store::cache::geocode`)
//! so the same query typed twice doesn't burn two upstream calls,
//! and so the user honours Nominatim's posted "1 req/sec
//! absolute" rate limit naturally.
//!
//! # Privacy
//!
//! Each network-bound request emits the user's typed query and a
//! best-effort UA string identifying this app + version.  No
//! stable identifier is sent — Nominatim sees an anonymous
//! desktop client.  Results are cached locally so a returning
//! user typing the same place doesn't repeat the round-trip.
//!
//! # Future: NC Maps proxy
//!
//! The Nextcloud Maps app (when installed) is primarily a Leaflet
//! frontend that calls Nominatim from the browser side; it
//! doesn't currently expose a usable server-side proxy.  This
//! module deliberately stays Nominatim-direct so the offline /
//! NC-absent flow keeps working; a future PR can add an
//! NC-proxied tier if/when Maps grows one.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use nimbus_core::NimbusError;

/// One forward-geocoding suggestion.  Mirrors the Nominatim
/// response we care about, plus a normalised `(lat, lon)` pair —
/// Nominatim emits both as JSON strings, never numbers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeocodeResult {
    /// Server-side row id — we don't use it but caching the raw
    /// JSON shape means rebuilding the struct from cache returns
    /// the same value as a fresh fetch.
    #[serde(default)]
    pub place_id: u64,
    /// Human-readable address ("Berlin Hauptbahnhof, …, Berlin,
    /// 10557, Deutschland").  This is what we drop into the
    /// `LOCATION:` field.
    pub display_name: String,
    /// Latitude in decimal degrees (WGS-84).
    pub lat: f64,
    /// Longitude in decimal degrees (WGS-84).
    pub lon: f64,
    /// Coarse OSM type ("node", "way", "relation"); we surface
    /// it in the autocomplete tooltip when present.
    #[serde(default)]
    pub osm_type: Option<String>,
    /// Class of place ("amenity", "highway", "tourism", …) —
    /// used by the UI to pick a small icon if we add per-type
    /// markers later.
    #[serde(default)]
    pub class: Option<String>,
    /// Sub-type within the class ("cafe", "restaurant", …).
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Structured address parts from Nominatim's
    /// `addressdetails=1` response.  Used by #259's contact-form
    /// suggestion picker to fill street / locality / region /
    /// postcode / country in one shot.  `None` for pre-#259
    /// cache rows whose serialised JSON didn't carry the field —
    /// the cache is forward-compatible because every component
    /// is `#[serde(default)]`.
    #[serde(default)]
    pub address: Option<GeocodeAddress>,
}

/// Structured address components Nominatim returns under the
/// `address` key when `addressdetails=1` is requested (#259).
/// Field names follow Nominatim's snake_case keys; we expose
/// them via the parent's `rename_all = "camelCase"` to the
/// frontend so they fit the existing IPC convention.
///
/// Every field is optional because Nominatim only emits the
/// keys it has — a forest-only POI returns `country` and
/// nothing else, while a postal address returns the full set.
/// We keep the "any of {city, town, village, hamlet, …}"
/// fallback list in the frontend because picking the right
/// locality field is a presentation decision, not a data one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GeocodeAddress {
    pub road: Option<String>,
    pub house_number: Option<String>,
    pub neighbourhood: Option<String>,
    pub suburb: Option<String>,
    pub city: Option<String>,
    pub town: Option<String>,
    pub village: Option<String>,
    pub hamlet: Option<String>,
    pub municipality: Option<String>,
    pub county: Option<String>,
    pub state: Option<String>,
    pub state_district: Option<String>,
    pub region: Option<String>,
    pub postcode: Option<String>,
    pub country: Option<String>,
    pub country_code: Option<String>,
}

/// Raw Nominatim hit before lat/lon get coerced to floats.
/// Nominatim emits both as strings, so we deserialise into a
/// shadow struct and then convert.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct NominatimHit {
    #[serde(default)]
    place_id: u64,
    display_name: String,
    lat: String,
    lon: String,
    #[serde(default)]
    osm_type: Option<String>,
    #[serde(default)]
    class: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    address: Option<GeocodeAddress>,
}

/// Public-Nominatim base URL — used when the user hasn't
/// configured a custom one.  Exported so the settings UI can
/// show it to the user as the "default" placeholder text.
pub const DEFAULT_NOMINATIM_BASE_URL: &str = "https://nominatim.openstreetmap.org";

/// Resolve which Nominatim base URL to actually hit, given the
/// user's setting.  Empty / whitespace-only → public default;
/// otherwise the user-supplied value with any trailing slashes
/// trimmed (so `https://x/`, `https://x//`, and `https://x` all
/// produce the same result).  We do this in one place instead
/// of at the call site so a typo in the setting can never
/// produce a malformed `https://x//search` URL.
pub fn resolve_nominatim_base_url(user_setting: &str) -> &str {
    let trimmed = user_setting.trim();
    if trimmed.is_empty() {
        return DEFAULT_NOMINATIM_BASE_URL;
    }
    trimmed.trim_end_matches('/')
}

/// Issue a forward-geocoding request to Nominatim.
///
/// `base_url` is the resolved endpoint root — typically the
/// public Nominatim, but a self-hosted instance is supported via
/// the `nominatim_base_url` user setting.  See
/// `resolve_nominatim_base_url`.
///
/// `lang` is the IETF tag whose translations Nominatim should
/// prefer (Accept-Language header).  Empty string means "let
/// Nominatim default to local-language names", which is fine for
/// most use cases.
///
/// The function does **not** consult the local cache — that's the
/// caller's job.  Pulling cache logic into this layer would
/// couple the protocol crate to `nimbus-store`, and the cache
/// adds a small amount of canonicalisation we want visible in
/// `main.rs` where the Tauri command flows.
pub async fn nominatim_search(
    query: &str,
    lang: &str,
    base_url: &str,
) -> Result<Vec<GeocodeResult>, NimbusError> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }

    let base = resolve_nominatim_base_url(base_url);
    let url = format!(
        "{base}/search?format=jsonv2&addressdetails=1&limit=8&q={}",
        urlencoding(q),
    );

    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        // Nominatim's usage policy requires an honest User-Agent
        // identifying the application + version + a contact path.
        // Repo URL satisfies the contact requirement.
        .user_agent(concat!(
            "NimbusMail/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/Videothek/nimbus-mail)"
        ))
        .build()
        .map_err(|e| NimbusError::Network(format!("geocode HTTP client: {e}")))?;

    let mut req = client.get(&url);
    if !lang.trim().is_empty() {
        req = req.header(reqwest::header::ACCEPT_LANGUAGE, lang);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| NimbusError::Network(format!("geocode request: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(NimbusError::Other(format!(
            "Nominatim returned HTTP {status}"
        )));
    }
    let hits: Vec<NominatimHit> = resp
        .json()
        .await
        .map_err(|e| NimbusError::Protocol(format!("geocode JSON: {e}")))?;

    Ok(hits
        .into_iter()
        .filter_map(|h| {
            let lat: f64 = h.lat.parse().ok()?;
            let lon: f64 = h.lon.parse().ok()?;
            // Drop hits outside WGS-84 ranges — defensive against
            // server-side wobbles, never seen in practice.
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                return None;
            }
            Some(GeocodeResult {
                place_id: h.place_id,
                display_name: h.display_name,
                lat,
                lon,
                osm_type: h.osm_type,
                class: h.class,
                kind: h.kind,
                address: h.address,
            })
        })
        .collect())
}

/// Minimal percent-encoding for query-string values.  Mirrors the
/// helper in `nimbus-nextcloud::user` so we don't pull in a fresh
/// dep just for one path.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_base_url_falls_back_to_default_on_empty() {
        assert_eq!(resolve_nominatim_base_url(""), DEFAULT_NOMINATIM_BASE_URL);
        assert_eq!(
            resolve_nominatim_base_url("   "),
            DEFAULT_NOMINATIM_BASE_URL
        );
    }

    #[test]
    fn resolve_base_url_trims_trailing_slashes() {
        assert_eq!(
            resolve_nominatim_base_url("https://nominatim.example.com"),
            "https://nominatim.example.com"
        );
        assert_eq!(
            resolve_nominatim_base_url("https://nominatim.example.com/"),
            "https://nominatim.example.com"
        );
        assert_eq!(
            resolve_nominatim_base_url("https://nominatim.example.com////"),
            "https://nominatim.example.com"
        );
    }

    #[test]
    fn urlencoding_escapes_spaces_and_diacritics() {
        assert_eq!(urlencoding("Café Berlin"), "Caf%C3%A9%20Berlin");
        assert_eq!(urlencoding("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn deserialises_a_real_response_shape() {
        // Subset of an actual Nominatim hit.  The field set is
        // representative of what we surface in the UI.
        let body = r#"[
          {
            "place_id": 12345,
            "lat": "52.520008",
            "lon": "13.404954",
            "display_name": "Berlin, 10117, Deutschland",
            "osm_type": "node",
            "class": "place",
            "type": "city"
          }
        ]"#;
        let hits: Vec<NominatimHit> = serde_json::from_str(body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display_name, "Berlin, 10117, Deutschland");
        assert_eq!(hits[0].kind.as_deref(), Some("city"));
        // Pre-#259 hits don't carry an `address` object — make
        // sure the optional default keeps working.
        assert!(hits[0].address.is_none());
    }

    #[test]
    fn deserialises_addressdetails_response() {
        // Real-shape Nominatim hit for a German postal address
        // with `addressdetails=1`.  All the parts the contact
        // form needs sit under `address` and we should surface
        // them.
        let body = r#"[
          {
            "place_id": 67890,
            "lat": "52.520008",
            "lon": "13.404954",
            "display_name": "Schillerstraße 12, Charlottenburg, Berlin, 10625, Deutschland",
            "address": {
              "road": "Schillerstraße",
              "house_number": "12",
              "suburb": "Charlottenburg",
              "city": "Berlin",
              "state": "Berlin",
              "postcode": "10625",
              "country": "Deutschland",
              "country_code": "de"
            }
          }
        ]"#;
        let hits: Vec<NominatimHit> = serde_json::from_str(body).unwrap();
        assert_eq!(hits.len(), 1);
        let addr = hits[0].address.as_ref().expect("address present");
        assert_eq!(addr.road.as_deref(), Some("Schillerstraße"));
        assert_eq!(addr.house_number.as_deref(), Some("12"));
        assert_eq!(addr.city.as_deref(), Some("Berlin"));
        assert_eq!(addr.postcode.as_deref(), Some("10625"));
        assert_eq!(addr.country.as_deref(), Some("Deutschland"));
        assert_eq!(addr.country_code.as_deref(), Some("de"));
    }
}
