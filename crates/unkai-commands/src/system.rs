//! Local-filesystem and OS-adjacent affordances: attachment temp
//! files, printing, font enumeration, opening URLs.
//!
//! Mirrors `ui/src/lib/api/system.ts`.

use serde::Serialize;
use unkai_core::UnkaiError;
use unkai_core::models::NextcloudAccount;
use unkai_store::credentials;

use crate::state::SystemFontsCache;
use crate::support::load_nextcloud_account;

/// Open an arbitrary URL in the system's default browser.
///
/// Used by the Nextcloud login flow to hand the user off to their NC
/// server's login page, which happens outside our webview so the
/// browser can handle any SSO / IdP redirects the user's NC is wired
/// up with (Keycloak, OIDC, SAML, etc.).
pub fn open_url(url: String) -> Result<(), UnkaiError> {
    open::that(&url).map_err(|e| UnkaiError::Other(format!("failed to open '{url}': {e}")))
}

/// Write raw bytes to a local file.
///
/// Used by the attachment Download flow: the frontend opens a native
/// "Save As" dialog (via `tauri-plugin-dialog`), the user picks a
/// destination, and the chosen absolute path + the attachment bytes
/// come back here. We use `std::fs::write` which truncates any file
/// already at that path — the native save dialog already asked the
/// user about overwrites, so we don't need a second confirmation.
pub async fn save_bytes_to_path(path: String, data: Vec<u8>) -> Result<(), UnkaiError> {
    // `write` is synchronous and the payload is typically a few MB — the
    // Tauri command runtime already runs us on a worker thread, so we
    // don't need to spawn_blocking.
    std::fs::write(&path, &data)
        .map_err(|e| UnkaiError::Other(format!("Failed to write {path}: {e}")))
}

/// Read a small text file (a settings bundle, typically ~kilobytes)
/// from disk.  Used by the "Import settings" flow (#168): the
/// frontend opens a file picker via `plugin-dialog`, gets back an
/// absolute path, and hands it here for the actual read so we
/// don't need a separate filesystem plugin in `package.json`.
pub async fn read_text_from_path(path: String) -> Result<String, UnkaiError> {
    std::fs::read_to_string(&path)
        .map_err(|e| UnkaiError::Other(format!("Failed to read {path}: {e}")))
}

// ── Office viewer (issue #65) ────────────────────────────────
//
// Click an Office-compatible attachment in MailView → upload its
// bytes to a per-user temp folder in the user's Nextcloud → return
// the deep-link URL the frontend opens in a Tauri webview window.
// On close, the frontend fires `office_close_attachment` which
// expunges the temp file. A separate `office_sweep_temp` runs at
// connect-time to clean up anything left behind by a crash mid-edit.
//
// Folder layout:
//   /Unkai Mail/temp/<uuid>-<filename>
//
// The UUID prefix lets concurrent edits coexist without filename
// collisions and gives the sweeper an obvious "is-this-ours" gate
// (only delete files inside the temp folder).

/// Root path for Unkai's per-user temp area on the user's
/// Nextcloud. Files-app-visible (no leading dot) so the user can
/// recover anything we somehow lose track of, but tucked under our
/// app's branded folder so the home screen stays uncluttered.
pub const UNKAI_TEMP_ROOT: &str = "/Unkai Mail";

pub const UNKAI_TEMP_DIR: &str = "/Unkai Mail/temp";

/// Result of `office_open_attachment` — the URL the frontend opens
/// in a fresh webview window plus the temp path it should pass back
/// to `office_close_attachment` on close.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfficeOpenResult {
    /// Absolute URL into Nextcloud's Files app, which routes the
    /// file id to whichever app is registered as its handler —
    /// Collabora for Office docs, the PDF viewer for `.pdf`. Pasted
    /// directly into a `WebviewWindow` `url` arg.
    pub url: String,
    /// Path on the user's Nextcloud (relative to the user root).
    /// Round-trips back to `office_close_attachment` so the cleanup
    /// targets the file we just uploaded, not "all temp files".
    pub temp_path: String,
}

/// Best-effort `MKCOL` of `/Unkai Mail` and `/Unkai Mail/temp`.
/// Both are idempotent: `create_directory` returns "folder already
/// exists" as `UnkaiError::Nextcloud` which we swallow so a
/// pre-existing folder doesn't fail the open. Anything else
/// propagates so quota / 401 / network errors surface to the user.
pub async fn ensure_temp_dir(
    account: &NextcloudAccount,
    app_password: &str,
) -> Result<(), UnkaiError> {
    for dir in [UNKAI_TEMP_ROOT, UNKAI_TEMP_DIR] {
        match unkai_nextcloud::create_directory(
            &account.server_url,
            &account.username,
            app_password,
            dir,
            &account.trusted_certs,
        )
        .await
        {
            Ok(()) => {}
            Err(UnkaiError::Nextcloud(msg)) if msg.contains("already exists") => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Upload an attachment to the user's Nextcloud temp folder and
/// return the URL to open it in. Used by MailView when the user
/// clicks a `cid:` link or a tray button on an Office-compatible
/// attachment.
pub async fn office_open_attachment(
    nc_id: String,
    filename: String,
    data: Vec<u8>,
    content_type: Option<String>,
) -> Result<OfficeOpenResult, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    ensure_temp_dir(&account, &app_password).await?;

    // UUID prefix dodges filename collisions between concurrent
    // viewer windows, and gives the sweeper a way to recognise our
    // own files without a metadata round-trip.
    let safe_name = filename.replace(['/', '\\'], "_");
    let temp_path = format!("{}/{}-{}", UNKAI_TEMP_DIR, uuid::Uuid::new_v4(), safe_name);

    unkai_nextcloud::upload_file(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        data,
        content_type.as_deref(),
        &account.trusted_certs,
    )
    .await?;

    // Resolve the freshly-uploaded file's `oc:fileid` so we can
    // build the canonical `index.php/f/<id>` deep link. That URL
    // routes through Nextcloud's "open with default app" — Files
    // hands `.docx` etc. to Collabora, `.pdf` to the PDF viewer,
    // so the same code path works for both document types without
    // app-specific URL templating on our side.
    let file_id = unkai_nextcloud::propfind_fileid(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        &account.trusted_certs,
    )
    .await?;

    let server = account.server_url.trim_end_matches('/');
    let url = format!("{server}/index.php/f/{file_id}");

    Ok(OfficeOpenResult { url, temp_path })
}

/// Delete a temp file the frontend opened earlier. Best-effort:
/// 404 is swallowed by `delete_path`, network blips bubble up but
/// the frontend logs and moves on — leftover files get caught by
/// `office_sweep_temp` at next connect.
pub async fn office_close_attachment(nc_id: String, temp_path: String) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::delete_path(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        &account.trusted_certs,
    )
    .await
}

/// Result of `pdf_open_attachment`. Mirrors `OfficeOpenResult` so
/// the frontend can treat both viewers identically — same webview-
/// open + cleanup-on-close shape, the only difference is which URL
/// it points at.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfOpenResult {
    pub url: String,
    pub temp_path: String,
}

/// Open a PDF attachment in Nextcloud's built-in PDF viewer.
/// Same temp-upload + cleanup-on-close machinery as the Office flow:
///
///   - Bytes go to `/Unkai Mail/temp/<uuid>-<filename>` on the user's
///     Nextcloud.
///   - We use the same `index.php/f/<fileid>` deep link the Office
///     viewer uses; Files routes the fileid to its registered
///     handler, which for `.pdf` is Nextcloud's built-in PDF
///     viewer.
///
/// On `pdf_close_attachment` (or the startup sweep) the temp file
/// is DAV-DELETED so the viewer URL stops resolving once the
/// viewer window closes.
pub async fn pdf_open_attachment(
    nc_id: String,
    filename: String,
    data: Vec<u8>,
    content_type: Option<String>,
) -> Result<PdfOpenResult, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    ensure_temp_dir(&account, &app_password).await?;

    let safe_name = filename.replace(['/', '\\'], "_");
    let temp_path = format!("{}/{}-{}", UNKAI_TEMP_DIR, uuid::Uuid::new_v4(), safe_name);

    unkai_nextcloud::upload_file(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        data,
        content_type.as_deref(),
        &account.trusted_certs,
    )
    .await?;

    let file_id = unkai_nextcloud::propfind_fileid(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        &account.trusted_certs,
    )
    .await?;
    let server = account.server_url.trim_end_matches('/');
    let url = format!("{server}/index.php/f/{file_id}");

    Ok(PdfOpenResult { url, temp_path })
}

/// DELETE the temp PDF the frontend opened. Same cleanup path as
/// Office — kept as its own command so the frontend's per-viewer
/// dispatch stays straightforward.
pub async fn pdf_close_attachment(nc_id: String, temp_path: String) -> Result<(), UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;
    unkai_nextcloud::delete_path(
        &account.server_url,
        &account.username,
        &app_password,
        &temp_path,
        &account.trusted_certs,
    )
    .await
}

/// Clean up anything stuck in `/Unkai Mail/temp` from a previous
/// session — say the user closed Unkai mid-edit, or `office_close_
/// attachment` errored on the way out. We list the directory and
/// DELETE every entry whose `last_modified` is older than the cutoff,
/// so an in-flight viewer window in another Unkai instance doesn't
/// have its file pulled out from under it.
pub async fn office_sweep_temp(nc_id: String) -> Result<u32, UnkaiError> {
    let account = load_nextcloud_account(&nc_id)?;
    let app_password = credentials::get_nextcloud_password(&nc_id)?;

    // If the temp dir doesn't exist yet (fresh install / first
    // attachment click) treat that as "nothing to sweep". Anything
    // else propagates.
    let entries = match unkai_nextcloud::list_directory(
        &account.server_url,
        &account.username,
        &app_password,
        UNKAI_TEMP_DIR,
        &account.trusted_certs,
    )
    .await
    {
        Ok(e) => e,
        Err(UnkaiError::Nextcloud(msg)) if msg.contains("not found") => return Ok(0),
        Err(e) => return Err(e),
    };

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(1);
    let mut swept = 0u32;
    for entry in entries {
        let stale = entry.modified.map(|t| t < cutoff).unwrap_or(true);
        if !stale {
            continue;
        }
        let target = format!("{UNKAI_TEMP_DIR}/{}", entry.name);
        match unkai_nextcloud::delete_path(
            &account.server_url,
            &account.username,
            &app_password,
            &target,
            &account.trusted_certs,
        )
        .await
        {
            Ok(()) => swept += 1,
            Err(e) => tracing::warn!("office_sweep_temp: failed to delete {target}: {e}"),
        }
    }
    if swept > 0 {
        tracing::info!("office_sweep_temp: cleaned {swept} stale file(s)");
    }
    Ok(swept)
}

/// Open an attachment in its OS-default app so the user can print
/// it from there with the app's own print dialog. Used by the
/// "🖨 Open to print…" entry in the attachment dropdown.
///
/// Why this shape: the *generic* OS print dialog (Windows'
/// `PrintDialog`, the WinForms printer chooser) is just a printer
/// picker — it doesn't show the file, and it relies on each
/// file type's `PrintTo` verb being registered (Edge doesn't
/// register PrintTo for PDFs, so calling it for `.pdf` from a
/// fresh Windows install silently fails). The webview-rendered
/// Chromium print preview is brittle inside Tauri's sandbox.
///
/// What works reliably: open the file in its default handler
/// (Edge / Acrobat for PDF, Word for `.docx`, Photos for images,
/// Notepad for text, etc.) and let the user press **Ctrl/Cmd-P**.
/// Each app's own print dialog shows a real preview of the file
/// alongside the printer chooser — strictly better UX than the
/// generic OS dialog. The trade-off is one extra keystroke,
/// which the menu label calls out so the user expects it.
///
/// The temp file is kept for 10 minutes so the user has time
/// to actually print before we clean up.
pub async fn print_attachment(file_name: String, bytes: Vec<u8>) -> Result<(), UnkaiError> {
    // Per-call subdir name is a UUID v4 — not a predictable
    // path, which is exactly what the lint is meant to catch.
    // nosemgrep: rust.lang.security.temp-dir.temp-dir
    let mut dir = std::env::temp_dir();
    dir.push(format!("unkai-print-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| UnkaiError::Other(format!("create print temp dir: {e}")))?;

    // Strip path separators / NUL from the filename so the spooler
    // sees a flat name in our temp dir, not a path traversal.
    let safe_name: String = file_name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            _ => c,
        })
        .collect();
    let safe_name = if safe_name.trim().is_empty() {
        "attachment".to_string()
    } else {
        safe_name
    };
    let mut path = dir.clone();
    path.push(&safe_name);
    std::fs::write(&path, &bytes)
        .map_err(|e| UnkaiError::Other(format!("write print temp file: {e}")))?;

    // `open::that_detached` is the cross-platform "default verb"
    // launcher: ShellExecute open on Windows, `open` on macOS,
    // `xdg-open` (and friends) on Linux. `_detached` so we don't
    // hold a child handle the user could orphan by closing Unkai.
    if let Err(e) = open::that_detached(&path) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(UnkaiError::Other(format!(
            "failed to open '{}' for printing: {e}",
            path.display()
        )));
    }

    let cleanup_dir = dir;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        if let Err(e) = tokio::fs::remove_dir_all(&cleanup_dir).await {
            tracing::debug!(
                "print_attachment cleanup: failed to remove {}: {e}",
                cleanup_dir.display()
            );
        }
    });

    Ok(())
}

/// Walk the OS font catalogue and return the sorted, de-duped
/// family list.  Pure helper — used by both the startup warmer
/// and a manual refresh path.
pub fn enumerate_system_fonts() -> Vec<String> {
    let source = font_kit::source::SystemSource::new();
    let families = match source.all_families() {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("font enumeration failed: {e}");
            return Vec::new();
        }
    };
    let mut out: Vec<String> = families
        .into_iter()
        .filter(|f| !f.starts_with('.') && !f.trim().is_empty())
        .collect();
    out.sort_by_key(|a| a.to_lowercase());
    out.dedup();
    out
}

// ── On-disk font cache (#142 follow-up) ───────────────────────
//
// Even with the in-memory cache, a cold launch still pays the
// cost of font-kit's catalogue walk — slow on Linux's first-run
// fontconfig and visible enough that the user complained about
// "first compose" lag.  Persist the result to a JSON file in the
// OS cache dir, signed with a cheap fingerprint of the system
// font directories.  Subsequent launches read the JSON in
// microseconds; we only re-run font-kit when the fingerprint
// changes (i.e. the user actually installed or removed a font).
//
// The fingerprint is a SHA-256 of every font-directory mtime
// found by recursive walk.  Adding or removing a file inside any
// directory updates that directory's mtime on every common
// filesystem, so directory mtimes alone catch both additions and
// removals without us needing to stat every individual font file.

#[derive(serde::Serialize, serde::Deserialize)]
pub struct FontCacheFile {
    pub fingerprint: String,
    pub fonts: Vec<String>,
}

pub fn font_cache_path() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|d| d.join("unkai-mail").join("system_fonts.json"))
}

/// Standard system font directories per OS.  Used for the
/// fingerprint walk; font-kit itself looks at more places, but
/// these cover where additions / removals actually happen.
pub fn font_search_dirs() -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(w) = std::env::var_os("WINDIR") {
            out.push(std::path::PathBuf::from(w).join("Fonts"));
        }
        if let Some(d) = dirs::data_local_dir() {
            out.push(d.join("Microsoft").join("Windows").join("Fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        out.push(std::path::PathBuf::from("/System/Library/Fonts"));
        out.push(std::path::PathBuf::from("/Library/Fonts"));
        if let Some(h) = dirs::home_dir() {
            out.push(h.join("Library").join("Fonts"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        out.push(std::path::PathBuf::from("/usr/share/fonts"));
        out.push(std::path::PathBuf::from("/usr/local/share/fonts"));
        if let Some(h) = dirs::home_dir() {
            out.push(h.join(".fonts"));
            out.push(h.join(".local/share/fonts"));
        }
    }
    out
}

pub fn collect_dir_mtimes(dir: &std::path::Path, out: &mut Vec<(String, u64)>) {
    let Ok(meta) = std::fs::metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    out.push((dir.to_string_lossy().into_owned(), mtime));
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        if let Ok(m) = entry.metadata()
            && m.is_dir()
        {
            collect_dir_mtimes(&entry.path(), out);
        }
    }
}

pub fn compute_font_fingerprint() -> String {
    use sha2::{Digest, Sha256};
    let mut pairs: Vec<(String, u64)> = Vec::new();
    for d in font_search_dirs() {
        collect_dir_mtimes(&d, &mut pairs);
    }
    pairs.sort();
    let mut hasher = Sha256::new();
    for (p, m) in &pairs {
        hasher.update(p.as_bytes());
        hasher.update(b"|");
        hasher.update(m.to_string().as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

pub fn load_font_cache_file() -> Option<FontCacheFile> {
    let path = font_cache_path()?;
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save_font_cache_file(file: &FontCacheFile) {
    let Some(path) = font_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(file) {
        let _ = std::fs::write(&path, bytes);
    }
}

/// Return the cached font list to the frontend.  Reads from
/// the shared `SystemFontsCache` populated at startup; if the
/// cache is somehow empty (startup warmer failed or hasn't run
/// yet), runs the enumeration once on a blocking thread and
/// memoises the result before returning.
pub async fn list_system_fonts(cache: &SystemFontsCache) -> Result<Vec<String>, UnkaiError> {
    {
        let snap = cache.read().await;
        if !snap.is_empty() {
            return Ok(snap.clone());
        }
    }
    // Cold path: warm the cache synchronously this once.
    let fonts = tokio::task::spawn_blocking(enumerate_system_fonts)
        .await
        .map_err(|e| UnkaiError::Other(format!("font enumeration join: {e}")))?;
    *cache.write().await = fonts.clone();
    Ok(fonts)
}
