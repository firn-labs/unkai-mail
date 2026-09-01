# Unkai UI Conventions — the hard rules

**Audience:** AI agents (Claude, GPT, etc.) and human contributors touching anything under `ui/src/`.
**Status:** Authoritative. When this document and existing code disagree, this document wins — the code is drift (see the ledger at the bottom). When this document and `CLAUDE.md` disagree, this document wins for visual/design questions; `CLAUDE.md` § "UI Conventions" and § "Sidebar-routed integration view shell" remain authoritative for the *behavioral* idioms (menu anchoring, outside-click dismissal, filter-reset, `on…ref` patterns, etc.).

Read this before writing or editing any Svelte component. The companion spec for icons is [`icons/ICON_DESIGN_REFERENCE.md`](../icons/ICON_DESIGN_REFERENCE.md).

## Why this document exists

Unkai's design language is deliberate: a dense, quiet, native-feeling mail client — frosted-glass chrome over Skeleton theme tokens, hand-drawn 1.6-stroke icons, small type, one accent. The failure mode is not bad taste; it is **drift**: the same primitive (a popover, a badge, a destructive button, a close affordance) implemented four slightly different ways across tabs. Drift is what makes a UI read as machine-assembled. The cure is mechanical:

> **Rule 0 — Consistency beats improvement.** Before styling anything, find the nearest existing implementation of the same primitive and copy its class string exactly. Never "improve" a shape locally. If a genuinely new pattern is needed, edit this document first, then implement it — a pattern that isn't written here doesn't ship.

## The design language in one paragraph

Persistent chrome is `.glass-panel`; floating layers are `.glass-float`; text on glass uses the `.text-on-glass*` contrast tokens. All color routes through Skeleton semantic tokens (`primary / surface / success / warning / error` in oklch), so every stock and user-imported theme keeps working. Density is productivity-app density: `text-sm` body, `text-xs` meta, `px-3 py-2` rows, hairline borders. Hover and selection are translucent primary tints. Motion is `transition-colors duration-150 ease-out` and nothing else. Icons come from the in-repo registry at one stroke width. The app should feel closer to Apple Mail than to a SaaS landing page.

## Hard rules

### 1. Color: semantic tokens only

- App chrome uses **only** Skeleton token families: `surface-*`, `primary-*`, `success-*`, `warning-*`, `error-*` (plus `secondary-*`/`tertiary-*` where `Avatar.svelte`'s hash palette already uses them).
- **Raw Tailwind palette names (`red-500`, `amber-400`, `violet-500`, `slate-*`, …) are banned in app chrome.** They don't follow the theme; in `crimson` or `nosh` they visibly diverge from the theme's own hues.
- **Destructive is `error-*`, not `red-*`.** The destructive hover overlay is:
  `hover:bg-error-500/15 hover:text-error-500 hover:border-error-500/40`
  (This supersedes the older raw-red idiom. Migration is lazy — swap `red-*` → `error-*` whenever you're already touching a file; never add new `red-*`.)
- Semantic colors mean semantics: `error` = destructive/failed, `warning` = caution, `success` = confirmed. Never decorative.
- **No gradients in app chrome.** No `bg-gradient-*`, no `linearGradient`, no gradient text (`bg-clip-text` + `text-transparent`). Documented exemptions: `inviteHtml.ts` (email cards live in foreign clients), functional `repeating-linear-gradient` hatch/dash patterns (calendar tentative-events, thread connector), the resize-cursor image.
- Documented raw-color exemptions: `.email-html-body` renderer in `app.css` (email content must not follow the app theme), `AccountSettings` app-icon colorway swatches (literal previews), `FileTypeIcon.svelte` (extension-hash hues — under review, see Open decisions).

### 2. Surfaces: two glass utilities, nothing hand-rolled

- Persistent chrome (rails, sidebars, headers, toolbars, nav columns) = `.glass-panel` + the structural border side you need.
- **Every floating layer (modal, dropdown, popover, context menu, autocomplete panel) = `.glass-float` + its radius tier. Never re-add `border`, `shadow-*`, or a `bg-*` next to it.** The hand-rolled `bg-surface-50 dark:bg-surface-900 border … shadow-lg` recipe is banned; it is the single biggest source of "why does this popover look different from that one."
- Exemption: a popover rendered *inside* a glass layer stays opaque (never stack blur) — `EmojiPicker` is the canonical case. If you claim this exemption, leave a comment saying so.
- Text sitting on glass uses `.text-on-glass` / `.text-on-glass-muted`, never raw `text-surface-*` greys. Muted is the contrast floor — don't go weaker.
- Shadows exist **only** via `--glass-shadow` inside `.glass-float`, plus `shadow-sm` on the opaque hover quick-action cluster (MailList/Notes/Tasks). No other `shadow-*`, no colored shadows, no glows.

### 3. Radius: four tiers, nothing else

| Tier | Class | Used for |
|---|---|---|
| Panel | `rounded-2xl` | modal cards, settings section cards |
| Menu | `rounded-xl` | compact anchored menus / dropdowns / popovers |
| Control | `rounded-lg` | buttons, inputs, list rows, selection highlights |
| Pill | `rounded-full` | badges, chips, avatars |

Bare `rounded`, `rounded-sm`, `rounded-md`, `rounded-3xl` are **banned** in UI chrome (bare `rounded`/`rounded-sm` occurrences in the tree are drift — migrate lazily to the nearest tier).

### 4. Type: small, uniform, weight-driven

- Body `text-sm`; metadata `text-xs`; view titles exactly `<h2 class="text-xl font-semibold truncate">`. Nothing larger than `text-xl` outside the first-run/lock surfaces (`AccountSetup`, `LockScreen`, `FullscreenState`).
- Hierarchy comes from weight (`font-medium` / `font-semibold`), not size. `font-bold` is reserved for the setup wizard.
- The eyebrow label (small uppercase section header) is **one** token: `text-[10px] uppercase tracking-wider text-surface-500` (on glass: `text-on-glass-muted`). The `text-[8px]`/`text-[9px]`/`text-[11px]`/`text-xs` variants in the tree are drift — converge on `text-[10px]`.
- No custom fonts, no gradient text, no wide tracking outside the eyebrow token.

### 5. Buttons: three shapes plus one overlay

1. **Icon-only neutral** (the workhorse): `btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center` + `<Icon size={14} />` + `title=` + `aria-label=`.
2. **Primary CTA**: `btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center`.
3. **Secondary** (Refresh/Sync): `btn btn-sm preset-tonal-surface inline-flex items-center justify-center`.

- **Destructive** = shape 1 + the `error-*` hover overlay from rule 1. Neutral at rest, colored only on hover.
- No fourth shape, ever. `btn-danger` is dead — remove on sight. `preset-filled-error-500` is allowed only as a modal's final destructive confirm button, not for row actions.
- Labeled buttons (footers, wizards) are the same `btn-sm` shape with the label after the icon. Loading swaps the leading icon to `loading`, never the label.

### 6. Icons: registry or nothing

- Every glyph goes through [`ui/src/lib/Icon.svelte`](../ui/src/lib/Icon.svelte). Adding a new SVG requires checking the registry first and following `icons/ICON_DESIGN_REFERENCE.md` (24×24, stroke 1.6, round caps, `currentColor`).
- **Emoji are banned as UI icons.** 🔒⚠️💾✉ etc. all have registered equivalents. Exemptions: user-settable folder/account/list emoji (a data feature, via `EmojiPicker`), OS-native notification text, email invite cards.
- **Raw text glyphs are banned as affordances.** `✕` close buttons → `<Icon name="close" size={18} />`. `▸`/`▾` disclosure carets → registered caret icons (register them if missing). `⋯` overflow → `<Icon name="more" />`.

### 7. Motion: functional only

- The house transition is `transition-colors duration-150 ease-out`. Scope other transitions to the single property that changes (`transition-transform` on Toggle, `transition-[width]` on progress).
- **Banned:** `transition-all`, `hover:scale-*`, entrance/exit animations on routine UI, animated gradients.
- Continuous animation only for genuine in-flight state (`animate-spin` sync, `animate-pulse` progress), gated `motion-safe:` where it's decorative, with `prefers-reduced-motion` handling for custom keyframes.

### 8. Density: this is a mail client

- Padding stays on the established scale: rows `px-3 py-2` (or tighter), headers `px-6 py-3`, modal/settings bodies `p-4`–`p-6`. `p-8`+, `py-12`+, hero spacing, and centered feature-card layouts are banned outside `FullscreenState`/`LockScreen`.
- Layout is flat panes separated by 1px borders. The `card` class appears only as the modal card or the settings section card. **Never a card inside a card**, never a card grid.

### 9. Badges: one component

All pill badges (PGP/Signed/High/Low, email-kind chips, status chips) share one shape: `rounded-full px-2 py-[1px] text-[10px] font-semibold uppercase tracking-wide leading-tight`, colored with semantic-token tints (`bg-error-500/15 text-error-600 dark:text-error-400` style). Extract/extend a shared `Badge`/chip component rather than inlining a new variant (see ledger).

### 10. Empty states: plain and two-state

Plain muted text (`text-surface-500`), no illustration stack, no emoji, no exclamation marks. Always distinguish **genuine empty** ("No notes yet — create one with the + button") from **filter narrowed to zero** ("No notes match this search") — two strings, two branches. All new strings go through `ui/messages/en.json`.

## Grep-able ban list

If any of these matches inside `ui/src/` (outside the documented exemptions above), it's a violation:

```
bg-gradient- | bg-clip-text | transition-all | hover:scale- | shadow-2xl | drop-shadow
rounded-3xl | rounded-md | btn-danger
(bg|text|border|ring)-(red|blue|green|purple|violet|indigo|pink|rose|cyan|sky|slate|emerald|orange|yellow)-
```

## Drift ledger (audit 2026-09-01)

Known violations at the time this document was written. Migration is lazy unless an item gets its own issue — fix when touching the file, never regress, tick off here when a file is clean.

1. **Raw `red-*` → `error-*`** (~161 occurrences; heaviest: `CalendarView`, `Sidebar`, `MailList`, `ContactsView`, `ProfilesSettings`, `SharesView`, `AiSettings`; also the `.quick-action-btn-danger` rule in `app.css` and the old idiom text in `CLAUDE.md` — the latter two are updated with this doc).
2. **Hand-rolled popover recipe → `.glass-float`** (~14 files: `Select`, `DateField`, `TimeField`, `LocationField`, `AddressSuggestField`, `AddressAutocomplete`, `EventEditor`, `ContactsView` ×2, `RichTextEditor` ×5, `SearchBar`, `AccountSettings`). One-line class swaps; biggest single visual win.
3. **`✕`/`▸`/`▾` raw glyphs → registry icons** (9 close buttons: `Compose` ×2, `CreateTalkRoomModal`, `EventEditor`, `EventPlanner`, `MoveFolderPicker`, `NextcloudFilePicker`, `NotesView`, `SearchInput`; carets in `NotesView`, `TalkView`, `RichTextEditor`, `app.css`). Needs `caret-right`/`caret-down` icons registered.
4. **Emoji-as-icon** (5 spots: `LockScreen` ×3 → `lock`/`passphrase`/`security-key`, `MailView` ⚠️ → `warning`, `NextcloudFilePicker` 💾 → `save-draft`, `NotesMentionPicker` ✉ → `email-envelope`).
5. **Settings shell off-vocabulary** (`AccountSettings` header: plain border instead of `glass-panel`, `font-bold` `<h1>` instead of `font-semibold` `<h2>`, text-label back button, `bg-primary-500/15` selection instead of `/12` + inset ring).
6. **`.text-on-glass*` under-adopted** (~22 files put raw `text-surface-*` on glass surfaces).
7. **Badge shapes ×4 → one component** (`EmailKindChip`, `MailList` inline pills ×4, `NextcloudSettings` variant, Skeleton `chip` in `SearchBar`).
8. **Undocumented radius tiers** (34 bare `rounded`, 15 `rounded-sm`).
9. **`MailView` undocumented primary-hover button overlay** (`hover:bg-primary-500/15 …` at the reader action bar) — either register it here as a legal "active-tool" overlay or remove.
10. **`FileTypeIcon` 9-hue rainbow** — see Open decisions.
11. **Hand-written `border-surface-N dark:border-surface-M` pairs** (~121, three variants) where a glass utility would supply the border.

## Open decisions

- **`FileTypeIcon` palette:** the violet/purple/pink/cyan extension-hash rainbow is the one genuinely "auto-generated-looking" palette in the app. Options: curate down to ~5 hues derived from semantic tokens, or keep (distinct file-type hues are an established desktop convention — Finder, Nextcloud Files do it). Undecided; don't touch until decided.
