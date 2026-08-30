; Unkai Mail installer chrome.
;
; Included by Tauri's NSIS template (bundle.windows.nsis.installerHooks) right
; after its stock !includes and BEFORE any MUI page macro is inserted, so the
; MUI2 defines below restyle the whole installer without forking the template.
;
; Constraints to keep in mind when editing:
; - Pure ASCII only. makensis reads BOM-less scripts as ANSI, so non-ASCII
;   characters here would come out garbled at runtime.
; - "1E2A40" must match BRAND_DARK in generate-installer-images.ps1: header.bmp
;   and sidebar.bmp are painted on that colour so they blend seamlessly into
;   the MUI_BGCOLOR dialog surface around them.
; - ${INSTALLERICON} / ${HEADERIMAGE} / ${SIDEBARIMAGE} are defined by the
;   template AFTER this include; that works because !define values expand
;   lazily where MUI2 uses them. It also means the three matching
;   tauri.conf.json entries (installerIcon, headerImage, sidebarImage) must
;   stay set, or the uninstaller defines below expand to "" and break the
;   compile.

; --- Brand surface: storm navy with near-white text ---------------------------
; Applies to the header strip on every page and to the welcome/finish pages.
!define MUI_BGCOLOR "1E2A40"
!define MUI_TEXTCOLOR "F5F7FA"

; Dock the header bitmap right so page titles keep their usual left position.
!define MUI_HEADERIMAGE_RIGHT

; --- Uninstaller carries the same identity ------------------------------------
; The template only sets MUI_ICON; without these the uninstaller falls back to
; the stock NSIS icon and artwork.
!define MUI_UNICON "${INSTALLERICON}"
!define MUI_HEADERIMAGE_UNBITMAP "${HEADERIMAGE}"
!define MUI_UNWELCOMEFINISHPAGE_BITMAP "${SIDEBARIMAGE}"

; --- Welcome / finish copy ----------------------------------------------------
; English only on purpose: the bundler currently builds an English-only
; installer (bundle.windows.nsis.languages is unset, defaulting to English).
; If installer languages are ever added, this copy must move to LangStrings.
!define MUI_WELCOMEPAGE_TITLE "Welcome to Unkai Mail"
!define MUI_WELCOMEPAGE_TEXT "Unkai is the sea of clouds that gathers beneath mountain peaks at dawn.$\r$\n$\r$\nThis wizard installs Unkai Mail, a fast, private mail client with deep Nextcloud integration.$\r$\n$\r$\nClick Next to continue."

!define MUI_FINISHPAGE_TITLE "Unkai Mail is ready"
!define MUI_FINISHPAGE_TEXT "Unkai Mail has been installed on your computer.$\r$\n$\r$\nAdd your mail account on first launch - Nextcloud, IMAP and JMAP accounts are set up in minutes."
!define MUI_FINISHPAGE_LINK "Unkai Mail on GitHub"
!define MUI_FINISHPAGE_LINK_LOCATION "https://github.com/firn-labs/unkai-mail"
!define MUI_FINISHPAGE_LINK_COLOR "8FB3E8"
