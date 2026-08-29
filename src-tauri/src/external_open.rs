//! External-open routing (#536) — the single place that decides
//! which profile window receives a payload the OS handed us:
//! `mailto:` URLs (#294) and `.eml` / `.ics` file paths (#254).
//!
//! Payloads arrive on three roads:
//!
//!   * **Cold start** — the OS spawns us with the payload in argv
//!     (`capture_launch_args`, called before the Tauri builder
//!     runs) or, for URL schemes, via the deep-link plugin's
//!     `get_current()` (drained in [`setup_deep_links`]).  Nothing
//!     can be delivered yet — the payload goes into the cold-start
//!     buffers and the first window's shell drains them on mount
//!     via the `take_pending_*` commands.
//!   * **Live URL** — the deep-link plugin's `on_open_url` fires
//!     while we're running (also for second-launch argv, which the
//!     single-instance plugin forwards through deep-link's own
//!     dispatcher).
//!   * **Second launch** — the single-instance callback hands us
//!     the new process's argv ([`handle_second_launch`]).  This is
//!     the only live road file paths travel: deep-link forwards
//!     URLs, not bare paths.
//!
//! Every live road funnels into [`route_external_open`], which
//! owns the "which profile gets this?" decision (#536): the most
//! recently focused primary window — via
//! [`crate::windows::show_primary_window`], whose fallback chain
//! ends at (re)creating a window for the startup profile when
//! nothing is open.  The payload is always buffered *and* emitted
//! to that window; the frontend drains the buffer alongside the
//! event with a short-TTL dedupe, so neither a not-yet-listening
//! fresh window nor a double delivery can lose or replay it.

use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter};

use crate::windows;

/// What the OS handed us.  Decides the event channel and buffer a
/// payload rides in; the frontend decides what to *do* with it
/// (Compose for mailto, the standalone reader / event editor for
/// files).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalOpenKind {
    /// An RFC 6068 `mailto:` URL.
    Mailto,
    /// A path to an existing `.eml` or `.ics` file.
    File,
}

impl ExternalOpenKind {
    /// The window-targeted event the payload is delivered on.
    /// `unkai://mailto` predates this module (#294) and is matched
    /// literally by the frontend registry — keep it stable.
    fn event_name(self) -> &'static str {
        match self {
            ExternalOpenKind::Mailto => "unkai://mailto",
            ExternalOpenKind::File => "unkai://open-file",
        }
    }
}

/// Cold-start buffers, one per kind.  Always `Vec`s: on a cold
/// start it's plausible for multiple roads to deliver the same
/// payload (argv scan + deep-link `get_current()`), and a second
/// launch can carry several files at once — the frontend dedups
/// by draining the whole list.
static PENDING_MAILTO_URLS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static PENDING_FILE_OPENS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn buffer_for(kind: ExternalOpenKind) -> &'static Mutex<Vec<String>> {
    match kind {
        ExternalOpenKind::Mailto => PENDING_MAILTO_URLS.get_or_init(|| Mutex::new(Vec::new())),
        ExternalOpenKind::File => PENDING_FILE_OPENS.get_or_init(|| Mutex::new(Vec::new())),
    }
}

/// Stash a payload in its cold-start buffer.  No-op if the mutex
/// is poisoned — losing one payload to a worker-thread panic is
/// strictly better than panicking the main thread on lock
/// recovery.
fn buffer_payload(kind: ExternalOpenKind, payload: &str) {
    if let Ok(mut slot) = buffer_for(kind).lock() {
        slot.push(payload.to_string());
    }
}

/// Classify one launch / second-launch argument as an external
/// open, or `None` for anything we don't handle (`--flag` style
/// argv, the binary's own path, unrelated arguments).  File
/// candidates are validated as *existing* `.eml` / `.ics` files so
/// a stray argument can't be treated as a path.
pub fn classify_arg(arg: &str) -> Option<(ExternalOpenKind, String)> {
    if arg.starts_with('-') {
        return None;
    }
    let lower = arg.to_lowercase();
    if lower.starts_with("mailto:") {
        return Some((ExternalOpenKind::Mailto, arg.to_string()));
    }
    if (lower.ends_with(".eml") || lower.ends_with(".ics")) && std::path::Path::new(arg).is_file() {
        return Some((ExternalOpenKind::File, arg.to_string()));
    }
    None
}

/// Scan argv once at process start and buffer anything we handle.
/// Runs before the Tauri builder, so this is capture-only — there
/// is no window to deliver to yet.
///
/// On Windows the OS hands protocol URLs and associated-file paths
/// as argv; on macOS URLs come via the deep-link plugin's Apple
/// Event handler instead; on Linux it depends on the desktop
/// file's `Exec=` substitution (typically `%u` / `%U` → argv).
/// Scanning *all* of argv (not just argv[1]) handles wrappers that
/// prepend flags.
pub fn capture_launch_args() {
    // argv is read only to discover payloads to *open*, never for
    // an access-control decision — mailto URLs are parsed as URLs
    // by the frontend, and file candidates are re-validated as
    // existing `.eml`/`.ics` files in `classify_arg`.
    // nosemgrep: rust.lang.security.args.args
    for arg in std::env::args().skip(1) {
        if let Some((kind, payload)) = classify_arg(&arg) {
            buffer_payload(kind, &payload);
        }
    }
}

/// Deliver one external payload to the right profile window — THE
/// routing decision (#536): the most recently focused primary
/// window; when nothing is open, a (re)created window for the
/// startup profile.
///
/// Buffer + targeted emit, always both: the buffer covers a window
/// whose listeners aren't up yet (a freshly created one skips the
/// emit entirely — it drains the buffer on mount), and the
/// frontend's short-TTL dedupe collapses the double delivery when
/// both roads arrive.
pub fn route_external_open(app: &AppHandle, kind: ExternalOpenKind, payload: &str) {
    buffer_payload(kind, payload);
    match windows::show_primary_window(app) {
        Ok(raised) if !raised.created => {
            if let Err(e) = app.emit_to(
                raised.window.label(),
                kind.event_name(),
                payload.to_string(),
            ) {
                tracing::warn!("emit {} failed: {e}", kind.event_name());
            }
        }
        // A freshly created window drains the buffer on mount —
        // no emit needed (and none possible: no listeners yet).
        Ok(_) => {}
        Err(e) => tracing::warn!("external-open window raise failed: {e}"),
    }
}

/// Single-instance callback body: route every payload in the
/// second launch's argv, and raise the app even when there is
/// none — a bare second launch is the user asking for the window.
///
/// `mailto:` URLs are *also* forwarded through the deep-link
/// plugin's dispatcher (the `deep-link` feature on the
/// single-instance plugin), so they can arrive here and via
/// `on_open_url` — two `route_external_open` calls, collapsed by
/// the frontend's dedupe.  File paths only travel this road.
pub fn handle_second_launch(app: &AppHandle, argv: &[String]) {
    let mut routed_any = false;
    // Skip argv[0] — the second process's binary path.
    for arg in argv.iter().skip(1) {
        if let Some((kind, payload)) = classify_arg(arg) {
            routed_any = true;
            route_external_open(app, kind, &payload);
        }
    }
    if !routed_any && let Err(e) = windows::show_primary_window(app) {
        tracing::warn!("single-instance window raise failed: {e}");
    }
}

/// Deep-link wiring, called once from `.setup()` (#294):
///
///   1. Register `mailto` as a handled URI scheme at runtime.  The
///      bundle config registers it at install time, but
///      `register()` is what writes the per-user registry keys on
///      Windows (and the per-user `.desktop` association on Linux)
///      for dev / portable launches that never run an installer.
///      Idempotent — safe to call every boot.
///   2. Drain `get_current()` into the cold-start buffer.  Tauri
///      exposes the URL the OS used to spawn us here; without
///      this, a fresh launch from a mailto link delivers the URL
///      before the frontend has registered a listener and we'd
///      silently drop it.  Capture-only — the config-declared main
///      window doesn't exist yet at `.setup()` time.
///   3. Subscribe to `on_open_url` for live URLs, routing each
///      through [`route_external_open`].
pub fn setup_deep_links(app: &AppHandle) {
    use tauri_plugin_deep_link::DeepLinkExt;
    let dl = app.deep_link();
    if let Err(e) = dl.register("mailto") {
        tracing::warn!(
            "deep-link mailto registration failed (OS will not route mailto links here \
             until next launch / installer run): {e}"
        );
    }
    match dl.get_current() {
        Ok(Some(urls)) => {
            for u in urls {
                let s = u.to_string();
                if s.to_lowercase().starts_with("mailto:") {
                    buffer_payload(ExternalOpenKind::Mailto, &s);
                }
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("deep-link get_current failed: {e}"),
    }
    let handle_for_links = app.clone();
    dl.on_open_url(move |event| {
        for url in event.urls() {
            let s = url.to_string();
            if s.to_lowercase().starts_with("mailto:") {
                route_external_open(&handle_for_links, ExternalOpenKind::Mailto, &s);
            }
        }
    });
}

/// Frontend hook: drain the cold-start `mailto:` buffer.  Returns
/// the URLs collected so far and clears the buffer — a window
/// refresh won't re-open them.
pub fn take_pending_mailto_urls() -> Vec<String> {
    take_buffer(ExternalOpenKind::Mailto)
}

/// Frontend hook: drain the cold-start file-open buffer (`.eml` /
/// `.ics` paths).  Same one-shot semantics as the mailto drain.
pub fn take_pending_files_to_open() -> Vec<String> {
    take_buffer(ExternalOpenKind::File)
}

fn take_buffer(kind: ExternalOpenKind) -> Vec<String> {
    buffer_for(kind)
        .lock()
        .map(|mut slot| std::mem::take(&mut *slot))
        .unwrap_or_default()
}
