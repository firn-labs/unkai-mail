# Unkai Mail Logo Assets

Five color variants + monochrome tray version + a Skeleton-themed SVG that follows your app's primary color automatically.

## What's in this bundle

```
svg/
  sky/         — Sky blue (classic mail)
  twilight/    — Twilight purple (premium, distinctive)
  storm/       — Storm gray-blue (sophisticated)
  dawn/        — Dawn coral (warm, friendly)
  mint/        — Mint green (fresh, modern)
  monochrome/  — Black + white tray/menubar versions
  skeleton-themed/ — Uses CSS variables, follows app theme

png/
  sky/         — Same colors, rendered as PNG at each size
  twilight/
  storm/
  dawn/
  mint/
  monochrome/
```

Each color folder contains the logo at: 16, 32, 64, 128, 256, 512, 1024 pixels.

## Which file to use where

| Location | File | Why |
|---|---|---|
| Tauri app icon (cross-platform) | `png/<color>/unkai-512.png` and `unkai-1024.png` | Tauri bundles these into platform-specific formats |
| macOS menu bar (tray) | `svg/monochrome/unkai-mono-black.svg` (template image) | macOS auto-tints monochrome icons to match light/dark mode |
| Windows system tray | `png/<color>/unkai-32.png` (or 16/24) | Windows tray accepts colored PNG; 32px is the modern default |
| Linux .desktop launcher | `png/<color>/unkai-256.png` | GNOME/KDE want PNG, 256px is the standard launcher size |
| In-app branding (sidebar header, splash, etc.) | `svg/skeleton-themed/unkai-themed-128.svg` | Inherits your app's Skeleton primary color via CSS vars |
| Marketing/website | `svg/<color>/unkai-512.svg` | SVG scales infinitely |

## The Skeleton-themed SVG

The file in `svg/skeleton-themed/` references Skeleton's CSS custom properties directly:

```svg
<stop offset="0%" stop-color="var(--color-primary-400, #5EA8FF)"/>
<stop offset="100%" stop-color="var(--color-primary-700, #2563EB)"/>
```

When this SVG is **inlined into your Svelte page** (not loaded as `<img src="...">` — browsers don't resolve CSS vars in external SVGs), it will automatically pick up your active Skeleton theme's primary color. Switch themes, the logo recolors. Switch to dark mode, the gradient adapts.

The hex values in `var(--color-primary-XXX, #fallback)` are fallbacks for when the SVG renders outside your app (e.g., if you preview the file standalone). Update them to match your actual Skeleton primary if you want better fallbacks.

### How to inline it in Svelte

```svelte
<script>
  import UnkaiLogo from '$lib/assets/unkai-themed-128.svg?raw';
</script>

<div class="logo">{@html UnkaiLogo}</div>
```

Vite's `?raw` import inlines the SVG content as a string, so it renders as part of the DOM and CSS variables resolve correctly.

## Tauri integration

In `src-tauri/tauri.conf.json`:

```json
{
  "bundle": {
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.icns",
      "icons/icon.ico"
    ]
  }
}
```

Tauri provides a CLI command to generate platform-specific bundles from a single source PNG:

```bash
npm run tauri icon path/to/unkai-1024.png
```

This produces `.icns` (macOS), `.ico` (Windows), and the various PNG sizes Linux needs — all from your single 1024px source. Run it on whichever color variant you ship as the primary brand.

## Tray icon (macOS template image)

For macOS menu bar, use the monochrome black SVG. macOS treats it as a "template image" and auto-tints based on dark/light mode and selection state. Set this in your Tauri tray:

```rust
// src-tauri/src/main.rs
use tauri::{Manager, SystemTray, SystemTrayMenu, SystemTrayMenuItem};

let tray = SystemTray::new()
    .with_icon(tauri::Icon::Raw(include_bytes!("../icons/unkai-mono-black.png").to_vec()));
```

Note: macOS template images need to be PNGs, not SVGs. Convert the monochrome SVG with any tool (or use the bundled monochrome PNGs in `png/monochrome/`).

For Windows and Linux, use a colored 16-32px PNG instead — they don't have the auto-tinting concept.

## Matching exactly to your Skeleton theme

The five color presets I generated approximate common Skeleton themes but aren't pixel-perfect matches to any specific one. To use your **exact** Skeleton primary:

1. Open `src/app.css` (or wherever your Skeleton theme is imported)
2. Find your primary color values — typically `--color-primary-400` and `--color-primary-700`
3. Open one of the colored SVGs (e.g., `svg/sky/unkai-512.svg`)
4. Replace the `<stop>` colors in the `<linearGradient>` with your hex values
5. Also replace the cutout stroke color (the line drawing the envelope fold) with your darker shade

Or — easier — just use the `skeleton-themed` SVG everywhere your app is running, and only use a static color variant for the bundled OS-level icons (which can't access your CSS).

## Design notes

- **Container shape:** Squircle with `rx ≈ 22.5%` of width — matches Apple's app icon proportions
- **Mark sizing:** The cloud-envelope fills 60% of the container, leaving comfortable padding
- **Gradient:** Subtle vertical (lighter top → darker bottom) for dimensionality without being garish
- **Mark color:** White on colored background (universal app icon convention)
- **Envelope fold cutout:** Uses the gradient's darker color so it reads as negative space carved into the cloud
