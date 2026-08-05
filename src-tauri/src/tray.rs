//! Tray icon assets and the base-bitmap state the badge renderer
//! composites onto (#476: split out of `main.rs`).
//!
//! Purely desktop chrome — nothing here is reachable from
//! `unkai-commands`, which reaches the tray through
//! [`UiNotifier`](unkai_commands::UiNotifier) instead.

use unkai_core::UnkaiError;

/// Raw RGBA of the *current* tray base icon — i.e. the icon the
/// badge renderer overlays the unread count onto.  Wrapped in a
/// `Mutex` so `set_logo_style` can hot-swap the bitmap when the
/// user picks a different style without restarting the app.
pub struct TrayBaseIcon(pub std::sync::Mutex<TrayBaseIconBitmap>);

#[derive(Clone)]
pub struct TrayBaseIconBitmap {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Bytes of every per-style logo PNG, baked into the binary at
/// compile time so the picker doesn't depend on runtime resources.
/// 256 px is the right pick: large enough that downscales for the
/// 32 px tray and the 16/32 px Windows window icon stay sharp,
/// small enough that all 7 styles together add < 100 KB to the
/// binary.
pub mod logo_assets {
    pub const STORM: &[u8] = include_bytes!("../../logos/unkai-logo/png/storm/unkai-256.png");
    pub const DAWN: &[u8] = include_bytes!("../../logos/unkai-logo/png/dawn/unkai-256.png");
    pub const MINT: &[u8] = include_bytes!("../../logos/unkai-logo/png/mint/unkai-256.png");
    pub const SKY: &[u8] = include_bytes!("../../logos/unkai-logo/png/sky/unkai-256.png");
    pub const TWILIGHT: &[u8] = include_bytes!("../../logos/unkai-logo/png/twilight/unkai-256.png");
    pub const MONO_BLACK: &[u8] =
        include_bytes!("../../logos/unkai-logo/png/monochrome/unkai-mono-black.png");
    pub const MONO_WHITE: &[u8] =
        include_bytes!("../../logos/unkai-logo/png/monochrome/unkai-mono-white.png");

    // ── v2 logo set (added in #197 follow-up) ────────────────────
    // Same 256 px naming convention as v1; lives under the
    // separate `unkai-logo-v2` folder so the original art and
    // the new pack stay independently swappable.
    pub const COPPER: &[u8] = include_bytes!("../../logos/unkai-logo-v2/png/copper/unkai-256.png");
    pub const FOREST: &[u8] = include_bytes!("../../logos/unkai-logo-v2/png/forest/unkai-256.png");
    pub const MIDNIGHT: &[u8] =
        include_bytes!("../../logos/unkai-logo-v2/png/midnight/unkai-256.png");
    pub const OCEAN: &[u8] = include_bytes!("../../logos/unkai-logo-v2/png/ocean/unkai-256.png");
    pub const ROSE: &[u8] = include_bytes!("../../logos/unkai-logo-v2/png/rose/unkai-256.png");
    pub const SLATE: &[u8] = include_bytes!("../../logos/unkai-logo-v2/png/slate/unkai-256.png");
    pub const SUNSET: &[u8] = include_bytes!("../../logos/unkai-logo-v2/png/sunset/unkai-256.png");
}

/// Map a style slug to the embedded PNG bytes.  Unknown slug →
/// fall back to storm so a stray value (mistyped settings file,
/// future-renamed style) can never leave the tray with no icon.
pub fn logo_bytes_for(style: &str) -> &'static [u8] {
    match style {
        // v1 styles (atmospheric set)
        "dawn" => logo_assets::DAWN,
        "mint" => logo_assets::MINT,
        "sky" => logo_assets::SKY,
        "twilight" => logo_assets::TWILIGHT,
        "monochrome-black" => logo_assets::MONO_BLACK,
        "monochrome-white" => logo_assets::MONO_WHITE,
        // v2 styles (elemental set)
        "copper" => logo_assets::COPPER,
        "forest" => logo_assets::FOREST,
        "midnight" => logo_assets::MIDNIGHT,
        "ocean" => logo_assets::OCEAN,
        "rose" => logo_assets::ROSE,
        "slate" => logo_assets::SLATE,
        "sunset" => logo_assets::SUNSET,
        _ => logo_assets::STORM,
    }
}

/// Decode a PNG into the raw RGBA + dims that Tauri's
/// `tauri::image::Image::new` and our badge compositor both want.
/// Reuses Tauri's bundled PNG decoder so we don't pull a separate
/// `image` crate just for this.
pub fn decode_logo_png(bytes: &[u8]) -> Result<TrayBaseIconBitmap, UnkaiError> {
    let img = tauri::image::Image::from_bytes(bytes)
        .map_err(|e| UnkaiError::Other(format!("failed to decode logo PNG: {e}")))?;
    Ok(TrayBaseIconBitmap {
        rgba: img.rgba().to_vec(),
        width: img.width(),
        height: img.height(),
    })
}
