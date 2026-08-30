# Regenerates the Windows installer artwork from the storm app icon.
#
# Run from anywhere:  powershell -File src-tauri/installer/generate-installer-images.ps1
#
# Outputs (all 24-bit BMP, the format NSIS/WiX dialogs expect — no alpha channel):
#   sidebar.bmp     164x314  NSIS welcome/finish page sidebar
#   header.bmp      150x57   NSIS inner-page header (docked right via MUI_HEADERIMAGE_RIGHT)
#   wix-banner.bmp  493x58   WiX MSI top banner
#   wix-dialog.bmp  493x312  WiX MSI welcome/finish dialog background
#
# The exact pixel sizes are contracts with the dialog layouts — don't change them.
# BRAND_DARK (#1E2A40) must stay in sync with MUI_BGCOLOR in hooks.nsh: the sidebar
# and header bitmaps fade/blend into that colour so the installer pages read as one
# continuous surface instead of a bitmap pasted onto a dialog.

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$installerDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$iconPath = Join-Path $installerDir '..\icons\icon.png'

# Storm gradient stops from logos/unkai-logo/svg/storm/
$BRAND_LIGHT = [System.Drawing.Color]::FromArgb(74, 91, 122)    # #4A5B7A
$BRAND_DARK  = [System.Drawing.Color]::FromArgb(30, 42, 64)     # #1E2A40
$TEXT_WHITE  = [System.Drawing.Color]::FromArgb(245, 247, 250)
$TEXT_MUTED  = [System.Drawing.Color]::FromArgb(169, 182, 204)  # #A9B6CC
$ACCENT      = [System.Drawing.Color]::FromArgb(86, 104, 138)   # #56688A

$icon = [System.Drawing.Image]::FromFile((Resolve-Path $iconPath))

function New-Canvas([int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap($w, $h, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
    return @($bmp, $g)
}

# Soft radial brand glow on a dark ground; the glow's rim equals the ground colour,
# so the bitmap blends seamlessly into the MUI_BGCOLOR page around it.
function Fill-Glow([System.Drawing.Graphics]$g, [int]$w, [int]$h, [int]$cx, [int]$cy, [int]$r) {
    $bg = New-Object System.Drawing.SolidBrush($BRAND_DARK)
    $g.FillRectangle($bg, 0, 0, $w, $h)
    $bg.Dispose()

    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddEllipse($cx - $r, $cy - $r, 2 * $r, 2 * $r)
    $glow = New-Object System.Drawing.Drawing2D.PathGradientBrush($path)
    $glow.CenterColor = $BRAND_LIGHT
    $glow.SurroundColors = @($BRAND_DARK)
    $glow.CenterPoint = New-Object System.Drawing.PointF($cx, $cy)
    $g.FillEllipse($glow, $cx - $r, $cy - $r, 2 * $r, 2 * $r)
    $glow.Dispose()
    $path.Dispose()
}

function Draw-CenteredText([System.Drawing.Graphics]$g, [string]$text, [string]$family, [single]$size, [System.Drawing.FontStyle]$style, [System.Drawing.Color]$color, [single]$cx, [single]$y) {
    $font = New-Object System.Drawing.Font($family, $size, $style, [System.Drawing.GraphicsUnit]::Pixel)
    $brush = New-Object System.Drawing.SolidBrush($color)
    $fmt = New-Object System.Drawing.StringFormat
    $fmt.Alignment = [System.Drawing.StringAlignment]::Center
    $g.DrawString($text, $font, $brush, $cx, $y, $fmt)
    $fmt.Dispose(); $brush.Dispose(); $font.Dispose()
}

function Save-Bmp([System.Drawing.Bitmap]$bmp, [System.Drawing.Graphics]$g, [string]$name) {
    $g.Dispose()
    $out = Join-Path $installerDir $name
    $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Bmp)
    $bmp.Dispose()
    Write-Host "wrote $out"
}

# --- NSIS sidebar: 164x314, welcome/finish pages -------------------------------
$bmp, $g = New-Canvas 164 314
Fill-Glow $g 164 314 82 104 190
$g.DrawImage($icon, 34, 56, 96, 96)
Draw-CenteredText $g 'Unkai Mail' 'Segoe UI Semibold' 21 ([System.Drawing.FontStyle]::Regular) $TEXT_WHITE 82 182
$accentPen = New-Object System.Drawing.Pen($ACCENT, 2)
$g.DrawLine($accentPen, 68, 222, 96, 222)
$accentPen.Dispose()
Draw-CenteredText $g 'by Firn Labs' 'Segoe UI' 12 ([System.Drawing.FontStyle]::Regular) $TEXT_MUTED 82 236
Save-Bmp $bmp $g 'sidebar.bmp'

# --- NSIS header: 150x57, docked right on inner pages --------------------------
# Flat BRAND_DARK so it merges with the MUI_BGCOLOR header strip; icon sits right.
$bmp, $g = New-Canvas 150 57
$bg = New-Object System.Drawing.SolidBrush($BRAND_DARK)
$g.FillRectangle($bg, 0, 0, 150, 57)
$bg.Dispose()
$g.DrawImage($icon, 104, 12, 33, 33)
Save-Bmp $bmp $g 'header.bmp'

# --- WiX banner: 493x58, white with icon right + gradient underline ------------
$bmp, $g = New-Canvas 493 58
$g.Clear([System.Drawing.Color]::White)
$underlineRect = New-Object System.Drawing.Rectangle(-1, 55, 495, 3)
$grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush($underlineRect, $BRAND_LIGHT, $BRAND_DARK, [System.Drawing.Drawing2D.LinearGradientMode]::Horizontal)
$g.FillRectangle($grad, 0, 55, 493, 3)
$grad.Dispose()
$g.DrawImage($icon, 441, 7, 41, 41)
Save-Bmp $bmp $g 'wix-banner.bmp'

# --- WiX dialog: 493x312, storm panel left, white body right -------------------
# WiX draws the welcome/finish text in black over x > ~165, so that area stays white.
$bmp, $g = New-Canvas 493 312
$g.Clear([System.Drawing.Color]::White)
$g.SetClip((New-Object System.Drawing.Rectangle(0, 0, 164, 312)))
Fill-Glow $g 164 312 82 100 190
$g.DrawImage($icon, 42, 58, 80, 80)
Draw-CenteredText $g 'Unkai Mail' 'Segoe UI Semibold' 19 ([System.Drawing.FontStyle]::Regular) $TEXT_WHITE 82 168
Draw-CenteredText $g 'by Firn Labs' 'Segoe UI' 11 ([System.Drawing.FontStyle]::Regular) $TEXT_MUTED 82 200
$g.ResetClip()
Save-Bmp $bmp $g 'wix-dialog.bmp'

$icon.Dispose()
Write-Host 'done.'
