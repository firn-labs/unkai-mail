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

; Visual-styles-themed checkboxes ignore the text colour SetCtlColors assigns
; (NSIS bug #443), so the finish page's "Run Unkai-Mail" / "Create desktop
; shortcut" labels rendered in the theme's default near-black on the navy
; MUI_BGCOLOR. MUI2 only applies its workaround (drawing those controls
; unthemed, which makes them honour MUI_TEXTCOLOR) when Windows runs in
; high-contrast mode -- unless this define forces it unconditionally. In the
; NSIS 3.11 that tauri-bundler pins, the define is consumed only by the
; welcome/finish page code, so no other control changes appearance.
!define MUI_FORCECLASSICCONTROLS

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

; --- Legacy install-path registry migration (<= 0.3.0 upgrades) ---------------
; Until 0.3.0 no bundle.publisher was configured, so tauri-bundler derived the
; manufacturer "unkai" from the bundle identifier and installers recorded the
; install directory under HKCU\Software\unkai\Unkai-Mail. Setting publisher =
; "Firn Labs" (#556) moved that key to HKCU\Software\Firn Labs\Unkai-Mail. The
; template's upgrade flow reads ONLY the new location to build the previous
; uninstaller's "_?=<install dir>" argument; on machines that installed
; <= 0.3.0 the new key does not exist, the argument expands empty, and the old
; uninstaller aborts ("NSIS Error: Error launching installer", then the
; template's "Unable to uninstall!").
;
; This invisible first page copies the legacy value into the new location
; before the template's reinstall page reads it. A plain `Page custom` (not a
; MUI page) cannot disturb the template's MUI page chain, and the Abort in the
; creator function keeps the page from ever being displayed. It runs in GUI
; and passive installs; silent installs skip all pages, but they skip the
; uninstall-previous-version flow too, so nothing reads the key there. The
; legacy key itself is left alone: the <= 0.3.0 uninstaller deletes it when
; the upgrade runs it, exactly like a normal uninstall would.
;
; The two key paths must mirror the template's ${MANUPRODUCTKEY}
; ("Software\<publisher>\<productName>"), but that define only exists AFTER
; this include, so they are spelled out; revisit if bundle.publisher or
; productName ever changes in tauri.conf.json.
Var UnkaiLegacyInstallDir
Function UnkaiMigrateLegacyInstallDir
  ReadRegStr $UnkaiLegacyInstallDir SHCTX "Software\Firn Labs\Unkai-Mail" ""
  ${If} $UnkaiLegacyInstallDir == ""
    ReadRegStr $UnkaiLegacyInstallDir SHCTX "Software\unkai\Unkai-Mail" ""
    ${If} $UnkaiLegacyInstallDir != ""
      WriteRegStr SHCTX "Software\Firn Labs\Unkai-Mail" "" $UnkaiLegacyInstallDir
    ${EndIf}
  ${EndIf}
  Abort
FunctionEnd
Page custom UnkaiMigrateLegacyInstallDir
