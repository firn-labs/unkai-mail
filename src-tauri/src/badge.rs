//! Unread-mail attention dot for the tray icon and the Windows taskbar overlay.
//!
//! When there's any unread mail we paint a small soft-red disc in
//! the bottom-right corner of the icon — no count, no digits.
//! The standard tray-icon attention pattern: the user sees "you
//! have something to read" at a glance, and reaches for the actual
//! count from inside the app where typography is legible.
//!
//! The disc uses a 2×2 supersampled edge: each pixel along the
//! circumference samples 4 sub-pixel positions; the fraction
//! inside the circle becomes the pixel's coverage. Smooth
//! boundary with no FFT-grade AA stack.
//!
//! The tray icon is the base PNG composited with the dot in the
//! bottom-right corner. The Windows taskbar overlay is the dot
//! alone at 16×16, sized for `WebviewWindow::set_overlay_icon`.

use tauri::image::Image;

/// Tailwind red-500, fully opaque.
const BADGE_RGBA: [u8; 4] = [239, 68, 68, 255];

/// Composite the unread dot onto a copy of `base_pixels` and
/// return it as an owned Tauri image. When `unread == 0` the
/// base image is returned unchanged so the tray relaxes to the
/// plain icon.
pub fn render_tray_icon(
    base_pixels: &[u8],
    width: u32,
    height: u32,
    unread: u32,
) -> Image<'static> {
    let pixels = if unread == 0 {
        base_pixels.to_vec()
    } else {
        let mut p = base_pixels.to_vec();
        let dim = width.min(height);
        // ~22% of the icon's short side, inset from the corner.
        // Reads as an indicator at the system-tray render size
        // without dominating the icon. Floor of 8 px so a tiny
        // base icon still produces a visible disc.
        let badge_size = ((dim * 22) / 100).max(8);
        let inset = (dim / 16).max(2);

        // Top-right corner of the icon canvas.
        let bx = width.saturating_sub(badge_size + inset);
        let by = inset;

        draw_filled_circle(&mut p, width, height, bx, by, badge_size, BADGE_RGBA);
        p
    };
    Image::new_owned(pixels, width, height)
}

/// Standalone unread dot sized for a Windows taskbar overlay
/// (16×16). Returns `None` when there's nothing to show so the
/// caller can clear the overlay with `set_overlay_icon(None)`.
///
/// Windows places the entire 16×16 overlay at the bottom-right of
/// the taskbar entry, then scales it for the user's DPI. Drawing
/// the disc at the full 16×16 produced a chunky red ball; instead
/// we paint a smaller disc inside transparent padding so the
/// visible badge in the taskbar reads as an indicator, not a
/// sticker. Same halo trick the tray badge uses for separation
/// against a busy underlying icon.
///
/// Called from the `#[cfg(windows)]` branch in `main.rs`; on other
/// platforms the call site is compiled away.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn render_taskbar_overlay(unread: u32) -> Option<Image<'static>> {
    if unread == 0 {
        return None;
    }
    const W: u32 = 16;
    const H: u32 = 16;
    // 10×10 disc centered in the 16×16 canvas (3 px transparent
    // padding all around). Leaves the visible badge ~half the
    // taskbar overlay slot — modern dock-indicator proportions.
    const BADGE_SIZE: u32 = 10;
    let badge_pos = (W - BADGE_SIZE) / 2;

    let mut pixels = vec![0u8; (W * H * 4) as usize];
    draw_filled_circle(
        &mut pixels,
        W,
        H,
        badge_pos,
        badge_pos,
        BADGE_SIZE,
        BADGE_RGBA,
    );
    Some(Image::new_owned(pixels, W, H))
}

/// Standard "src over dst" alpha compositing for one RGBA pixel.
///
/// `coverage` (0..=255) scales the source's alpha — used by the
/// supersampled circle to express partial pixel coverage at the
/// boundary. coverage=255 means "full pixel inside the shape",
/// coverage=0 means "fully outside" (early-exit).
fn blend_pixel(dst: &mut [u8], src: [u8; 4], coverage: u32) {
    let src_a = src[3] as u32 * coverage / 255;
    if src_a == 0 {
        return;
    }
    let inv = 255 - src_a;
    dst[0] = ((src[0] as u32 * src_a + dst[0] as u32 * inv) / 255) as u8;
    dst[1] = ((src[1] as u32 * src_a + dst[1] as u32 * inv) / 255) as u8;
    dst[2] = ((src[2] as u32 * src_a + dst[2] as u32 * inv) / 255) as u8;
    let dst_a = dst[3] as u32;
    dst[3] = (src_a + dst_a * inv / 255).min(255) as u8;
}

/// Filled circle with a 2×2 supersampled edge. For each output
/// pixel we sample 4 sub-pixel positions on a half-pixel grid;
/// the fraction of samples inside the circle becomes the pixel's
/// coverage. Centre pixels stay at full opacity, boundary pixels
/// fade to 0 over a one-pixel band.
fn draw_filled_circle(
    pixels: &mut [u8],
    img_w: u32,
    img_h: u32,
    x: u32,
    y: u32,
    size: u32,
    color: [u8; 4],
) {
    // 4×-precision integer space so we never hit floats. Each
    // unit step in absolute pixel coordinates is 4 units in this
    // space; one sub-sample step is 2 units; a radius of `size/2`
    // becomes `size*2`.
    let cx = x as i32 * 4 + size as i32 * 2;
    let cy = y as i32 * 4 + size as i32 * 2;
    let r = size as i32 * 2;
    let r2 = r * r;
    const SUBPIXELS: [(i32, i32); 4] = [(1, 1), (3, 1), (1, 3), (3, 3)];

    for py in 0..size {
        let abs_y = y + py;
        if abs_y >= img_h {
            break;
        }
        for px in 0..size {
            let abs_x = x + px;
            if abs_x >= img_w {
                break;
            }
            let base_x = abs_x as i32 * 4;
            let base_y = abs_y as i32 * 4;
            let mut hits = 0u32;
            for (sx, sy) in SUBPIXELS {
                let dx = base_x + sx - cx;
                let dy = base_y + sy - cy;
                if dx * dx + dy * dy <= r2 {
                    hits += 1;
                }
            }
            if hits == 0 {
                continue;
            }
            let coverage = hits * 255 / 4;
            let idx = ((abs_y * img_w + abs_x) * 4) as usize;
            blend_pixel(&mut pixels[idx..idx + 4], color, coverage);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_unread_returns_unchanged_pixels() {
        let base = vec![10u8; 32 * 32 * 4];
        let img = render_tray_icon(&base, 32, 32, 0);
        assert_eq!(img.rgba(), base.as_slice());
    }

    #[test]
    fn nonzero_unread_paints_reddish() {
        let base = vec![0u8; 32 * 32 * 4];
        let img = render_tray_icon(&base, 32, 32, 5);
        // We just want to see "this pixel is dominated by red"
        // somewhere in the top-right quadrant where the dot
        // lives.
        let pixels = img.rgba();
        let mut found_red = false;
        for y in 0..16 {
            for x in 16..32 {
                let idx = (y * 32 + x) * 4;
                let (r, g, b, a) = (
                    pixels[idx] as u32,
                    pixels[idx + 1] as u32,
                    pixels[idx + 2] as u32,
                    pixels[idx + 3] as u32,
                );
                if r > 150 && r > g + 50 && r > b + 50 && a > 0 {
                    found_red = true;
                    break;
                }
            }
        }
        assert!(found_red, "expected at least one red badge pixel");
    }

    #[test]
    fn taskbar_overlay_none_when_zero() {
        assert!(render_taskbar_overlay(0).is_none());
    }

    #[test]
    fn taskbar_overlay_some_when_nonzero() {
        let img = render_taskbar_overlay(7).expect("expected overlay");
        assert_eq!(img.width(), 16);
        assert_eq!(img.height(), 16);
    }

    #[test]
    fn circle_edge_is_antialiased() {
        // Render an overlay onto a transparent canvas and look for
        // at least one partially-covered (non-zero, non-fully-opaque)
        // pixel — proof the supersampled edge produces intermediate
        // alpha values, not a binary inside/outside mask.
        let img = render_taskbar_overlay(1).expect("expected overlay");
        let pixels = img.rgba();
        let (rgba, _) = pixels.as_chunks::<4>();
        let has_partial = rgba.iter().any(|p| (1..230).contains(&p[3]));
        assert!(
            has_partial,
            "expected at least one partially-covered AA edge pixel"
        );
    }
}
