# Unkai Mail Logos — Color Set v2

Seven additional color variants in the same style as the first set. Combined with v1, you now have **12 logo colors** to choose from.

## What's in this bundle

```
svg/
  ocean/      — Tropical cyan-to-teal
  sunset/     — Amber-to-red, energetic
  forest/     — Lime-to-deep-green, natural
  rose/       — Pink-to-magenta, soft
  midnight/   — Deep indigo-to-near-black, moody
  copper/     — Orange-to-burnt-sienna, warm
  slate/      — Gray-to-dark-slate, neutral

png/
  ocean/  sunset/  forest/  rose/  midnight/  copper/  slate/
```

Each color folder contains: 16, 32, 64, 128, 256, 512, 1024 pixels.

## Color hex values

If you want to update your Skeleton theme to match (or drive both from one source), here are the gradient stops used:

| Color | Light stop (top) | Dark stop (bottom) |
|---|---|---|
| Ocean | `#22D3EE` | `#0E7490` |
| Sunset | `#FBBF24` | `#DC2626` |
| Forest | `#65A30D` | `#14532D` |
| Rose | `#F472B6` | `#BE185D` |
| Midnight | `#312E81` | `#0F0A2E` |
| Copper | `#FB923C` | `#9A3412` |
| Slate | `#94A3B8` | `#334155` |

These are all from Tailwind's color palette (which Skeleton builds on), so they should integrate cleanly into your design tokens. The light stop maps roughly to Tailwind's `400` shade and the dark stop to `700-900` depending on the color.

## Tradeoffs to know about

**Sunset** is the only multi-hue gradient (amber → red rather than two shades of one color). It's the most attention-grabbing of all 12 options. If you want it to feel more controlled, swap the top stop to a less-saturated orange (e.g., `#F59E0B` instead of `#FBBF24`).

**Midnight** has the lowest contrast on the envelope-fold cutout because the whole gradient is dark. The cutout is still readable at 16px (verified) but if you want it more pronounced, swap the cutout stroke from `#0F0A2E` to a lighter midnight tone like `#1E1B4B`. I kept the spec consistent across all 12 (cutout uses the bottom gradient stop) so they read as a family.

**Slate** is the only desaturated option — useful if your users complain that colorful app icons clash with their wallpapers. Reads as "professional/quiet" rather than "branded/loud."

## Picking one

Quick decision tree:

- **Mail app for serious work?** Storm, Forest, Slate, Midnight
- **Mail app with personality?** Twilight, Rose, Copper, Sunset
- **Cross-platform brand-forward?** Sky, Ocean, Mint
- **Mature/premium feel?** Midnight, Storm, Slate
- **Fun/casual feel?** Sunset, Rose, Dawn, Mint, Copper

If you genuinely can't decide, ship Twilight (from set v1) — it's the most distinctive without being polarizing, and purple has very few competitors in the mail-app category.

## Same usage as v1

Everything else (Tauri integration, monochrome tray, Skeleton theming) works identically to the first batch. See the original `README-logos.md` for those details.
