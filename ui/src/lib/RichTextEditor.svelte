<script lang="ts" module>
  // Module-scope cache for the system font list (#142).  Shared
  // across every RichTextEditor instance in the session so
  // re-opening compose doesn't re-pay the IPC cost.  Filled by
  // the first instance that resolves `list_system_fonts`; the
  // backend cache itself is warmed at app startup, so this ends
  // up being a single ~1ms IPC for the lifetime of the app.
  let systemFontsCache = $state<string[]>([])
</script>

<script lang="ts">
  /**
   * RichTextEditor — Tiptap-based WYSIWYG editor for composing emails.
   *
   * Provides a ribbon-style toolbar with formatting controls: text
   * styles (bold, italic, underline, strikethrough), headings, lists,
   * alignment, links, tables, colors, horizontal rules, and images.
   *
   * The editor exposes its HTML content reactively via `onchange` so the
   * parent (Compose) can read it at send time.
   */

  import { anchorRect, layoutViewport, visualDeltaToLayout, visualToLayoutRatio } from './coords'
  import { onDestroy } from 'svelte'
  import { createEditor, EditorContent } from 'svelte-tiptap'
  import { Node as TiptapNode, mergeAttributes } from '@tiptap/core'
  import StarterKit from '@tiptap/starter-kit'
  import Underline from '@tiptap/extension-underline'
  import Link from '@tiptap/extension-link'
  import Image from '@tiptap/extension-image'
  import TextAlign from '@tiptap/extension-text-align'
  import Placeholder from '@tiptap/extension-placeholder'
  import { TextStyle } from '@tiptap/extension-text-style'
  import { FontFamily } from '@tiptap/extension-font-family'
  import Color from '@tiptap/extension-color'
  import Highlight from '@tiptap/extension-highlight'
  import { Table } from '@tiptap/extension-table'
  import TableRow from '@tiptap/extension-table-row'
  import TableCell from '@tiptap/extension-table-cell'
  import TableHeader from '@tiptap/extension-table-header'
  import Mention from '@tiptap/extension-mention'
  import type { Range } from '@tiptap/core'
  import EmojiPicker from './EmojiPicker.svelte'
  import FileTypeIcon from './FileTypeIcon.svelte'
  import Icon from './Icon.svelte'
  import { thumbUrlSync } from './AttachmentThumb.svelte'
  import * as api from './api'

  /**
   * Imperative handle the parent gets via `onready`. Tiptap is
   * stateful and managed inside this component, so the parent can't
   * just reassign `content` and expect the editor to follow. Instead
   * we hand back targeted operations the parent might need.
   */
  export interface EditorApi {
    /** Append raw HTML to the end of the document. Used by Compose
     *  to drop in Nextcloud share links without disturbing the user's
     *  cursor or undo history. */
    appendHtml: (html: string) => void
    /** Replace the entire document with the given HTML. Used by
     *  Compose to swap the active signature when the user picks a
     *  different From: account — caller is responsible for passing
     *  the *full* new body, not a diff. */
    setHtml: (html: string) => void
    /** Insert an image at the current selection. Used by the "insert
     *  from Nextcloud" flow: the parent opens the file picker,
     *  downloads bytes, converts to a data URL, and calls this. */
    insertImage: (src: string) => void
    /** Insert HTML at the **top** of the user's sign-off region.
     *
     *  The signoff region is everything between the lead-spacer
     *  typing area and the signature (a `<p>` whose text starts
     *  with the RFC 3676 `-- ` separator).  New blocks stack
     *  *above* any existing `unkaiBlock` cards already sitting in
     *  that region, so a freshly inserted share-link paragraph
     *  isn't buried beneath a previously inserted meeting card
     *  (#320 follow-up).  Falls back to just-above-signature when
     *  the region is empty, then to just-above the first
     *  `unkaiBlock` overall (signature absent → quoted-history is
     *  the only landmark), then to appending at the end (fresh
     *  compose).  Used by Compose's mid-compose meeting / Talk /
     *  Nextcloud-share block injection — all three want the same
     *  landing zone. */
    insertAboveSignature: (html: string) => void
  }

  interface Props {
    /** Initial HTML content (e.g. quoted reply body). */
    content?: string
    /** Placeholder text shown when the editor is empty. */
    placeholder?: string
    /** Fires on every content change with the current HTML. */
    onchange?: (html: string) => void
    /** Fires once the editor instance is ready, handing over a small
     *  imperative API for operations the parent can't drive via props. */
    onready?: (api: EditorApi) => void
    /** If set, the "insert image from Nextcloud" toolbar button calls
     *  this instead of prompting for a URL. The parent is expected to
     *  mount the `NextcloudFilePicker` and hand the picked image's
     *  data URL back via `editorApi.insertImage`. When not provided
     *  the button falls back to the plain URL prompt so embedders
     *  without Nextcloud (tests, future standalone usage) still work. */
    onrequestncimage?: () => void
    /** Async query for the `@` contact picker. Returns two parallel
     *  lists — `participants` (currently in To/Cc/Bcc) shown above the
     *  divider, `others` (rest of the address book matching `query`)
     *  below. The popup wires the keyboard / click → `oncontactpicked`. */
    oncontactquery?: (query: string) => Promise<{
      participants: ContactSuggestion[]
      others: ContactSuggestion[]
    }>
    /** Fires after a `@` contact mention has been inserted. Compose
     *  uses this to add the contact to Cc when the email isn't
     *  already on To/Cc/Bcc — keeps the recipient list and the
     *  body's mentions in sync. */
    oncontactpicked?: (contact: ContactSuggestion) => void
    /** Live attachment list for the `/` reference picker. Each entry
     *  needs `content_id` (the cid: target) and `filename`. The
     *  editor reads this snapshot at every keystroke of the picker —
     *  no separate event needed when the parent's attachments
     *  change. */
    attachmentsForRef?: AttachmentRef[]
    /** Caller-provided actions appended to the right side of the
     *  toolbar (#103).  Compose uses this to colocate the Save /
     *  Discard / Send buttons with the rich-text controls so the
     *  user has one toolbar instead of a top-row + bottom-footer
     *  split.  When omitted the tab strip ends at the trailing
     *  divider, which is what every future embedder without send-
     *  style actions gets by default. */
    actionsTrailing?: import('svelte').Snippet
    /** When true the trailing-actions block is rendered without
     *  the heavier vertical divider, so the caller's buttons sit
     *  flush against undo / redo with only the `gap-1` between
     *  them — used by surfaces like the signature editor (#314)
     *  where the trailing action is a single small icon, not a
     *  weight-bearing Save / Send group like in Compose. */
    actionsTrailingCompact?: boolean
    /** Extra tabs the embedder wants to add to the toolbar (#103
     *  follow-up).  Compose contributes a single "Attach" tab so
     *  Attach / NC Files / Talk / Event live in the same ribbon as
     *  the rich-text controls.  Each entry is `{ id, label, icon,
     *  content }` — `content` is rendered as the panel below the
     *  tab strip when the tab is active.  Empty / omitted → no
     *  extra tabs. */
    extraTabs?: ExtraTab[]
    /** Initial active tab — built-in `'format' | 'insert' | 'layout'`
     *  or one of the `extraTabs[].id` values the embedder contributed.
     *  Used by Compose's auto-encrypt-on-reply flow (#341) so the
     *  passphrase input is the first thing the user sees rather than
     *  hidden behind a tab switch.  Defaults to `'format'`. */
    defaultActiveTab?: string
  }

  /** Embedder-provided tab spec.  Mirrors the ribbon tabs the
   *  editor renders by default but lets a parent like Compose add
   *  Compose-only actions (Attach / Files / Talk / Event) into the
   *  same tab strip as Format / Insert / Layout. */
  export interface ExtraTab {
    /** Stable id used as the `activeTab` value when this tab is
     *  selected.  Avoid collisions with the built-in tab ids
     *  (`'format' | 'insert' | 'layout'`). */
    id: string
    /** Tab strip label, e.g. "Attach". */
    label: string
    /** Optional emoji / text glyph shown left of the label.
     *  Prefer `iconName` for consistency with the rest of the UI;
     *  this field stays for callers that want a literal glyph. */
    icon?: string
    /** Stroke-icon name shown left of the label.  Wins over
     *  `icon` when both are set. */
    iconName?: import('./Icon.svelte').IconName
    /** Panel contents — rendered below the tab strip when this tab
     *  is the active one. */
    content: import('svelte').Snippet
  }

  /** A row in the `@` contact picker. */
  export interface ContactSuggestion {
    /** Stable key — typically the email. */
    id: string
    /** Display name shown in the chip and the popup. */
    label: string
    /** Email address used in the mailto: href and plain-text
     *  serialization. */
    email: string
    /** Optional avatar URL (e.g. `api.platform.assetUrl(id, 'contact-photo')`). */
    photoUrl?: string | null
    /** Optional secondary line in the popup row (e.g. organization). */
    hint?: string | null
  }

  /** A row in the `/` attachment picker. */
  export interface AttachmentRef {
    /** RFC 2392 Content-ID, used as the `cid:` target on the
     *  inserted link. Stamped at attachment-pick time in Compose. */
    content_id: string
    filename: string
    /** MIME type — drives the per-format icon in the `/` picker
     *  popup.  Optional so older callers (and the picker's
     *  parseHTML round-trip) keep working without it. */
    content_type?: string
    /** Raw bytes of the attachment, when available — Compose
     *  always has them in memory so we can render an inline
     *  image / video thumbnail in the picker dropdown.  Omitted
     *  for non-Compose callers (the picker then falls back to
     *  the typed icon). */
    data?: number[] | Uint8Array
  }
  let {
    content = '',
    placeholder = 'Write your message\u2026',
    onchange,
    onready,
    onrequestncimage,
    oncontactquery,
    oncontactpicked,
    attachmentsForRef = [],
    actionsTrailing,
    actionsTrailingCompact = false,
    extraTabs = [],
    defaultActiveTab,
  }: Props = $props()

  // \u2500\u2500 Inline `@` and `/` picker state \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500
  // Tiptap's suggestion plugin owns the trigger detection and
  // emits lifecycle hooks; we mirror the relevant bits into Svelte
  // state so the popup renders declaratively below the editor.
  // Two independent slots so `@` and `/` can't both be open at once
  // wins out by sharing `pickerKey` \u2014 only one is mounted at a time.
  interface PickerPosition {
    left: number
    top: number
  }
  interface MentionPickerState {
    visible: boolean
    items: ContactSuggestion[]
    /** Number of `participants` items at the head of `items`. The
     *  popup draws a divider after `participantsCount - 1` so the
     *  user can tell "already on this mail" from "rest of the
     *  address book". Zero = no divider. */
    participantsCount: number
    selectedIndex: number
    position: PickerPosition
    command: ((c: ContactSuggestion) => void) | null
  }
  let mentionPicker = $state<MentionPickerState>({
    visible: false,
    items: [],
    participantsCount: 0,
    selectedIndex: 0,
    position: { left: 0, top: 0 },
    command: null,
  })

  interface AttachmentPickerState {
    visible: boolean
    items: AttachmentRef[]
    selectedIndex: number
    position: PickerPosition
    command: ((a: AttachmentRef) => void) | null
  }
  let attachmentPicker = $state<AttachmentPickerState>({
    visible: false,
    items: [],
    selectedIndex: 0,
    position: { left: 0, top: 0 },
    command: null,
  })
  /** Always true — kept as a $state flag so the {#if} below
   *  doesn't fall back to constant folding (which Svelte would
   *  treat as unconditional anyway, but the named flag makes
   *  the intent obvious).  Mounting the popup DOM eagerly
   *  with the editor itself means even the very first `/` is
   *  a CSS flag flip, no DOM allocation cost on the critical
   *  path.  The hidden <ul> is one detached element — a
   *  rounding-error memory cost for the latency win. */
  const attachmentPickerEverShown = true

  /** Compute the popup anchor from the trigger char's bounding
   *  rect. Clamped to the viewport so a `@` typed near the right
   *  edge of the modal doesn't push the popup off-screen. */
  function anchorBelow(rect: DOMRect | null | undefined): PickerPosition {
    if (!rect) return { left: 8, top: 8 }
    const gap = 4
    // Tiptap hands us a visual-space DOMRect — convert into the
    // fixed-position (layout) space the popup is styled in.
    const ratio = visualToLayoutRatio()
    const vp = layoutViewport()
    return {
      left: Math.max(8, Math.min(rect.left * ratio, vp.width - 280)),
      top: rect.bottom * ratio + gap,
    }
  }

  /** Forward editor keystrokes to the visible picker (arrows /
   *  enter / tab / escape) and return whether we consumed them.
   *  Tiptap suggestion uses the boolean to decide whether to let
   *  the keystroke fall through to the editor. Each picker has
   *  its own handler because they own different state slots, but
   *  both share the same key-mapping. */
  function handleMentionKey(event: KeyboardEvent): boolean {
    const len = mentionPicker.items.length
    if (len === 0) return event.key === 'Escape'
    if (event.key === 'ArrowDown') {
      mentionPicker.selectedIndex = (mentionPicker.selectedIndex + 1) % len
      return true
    }
    if (event.key === 'ArrowUp') {
      mentionPicker.selectedIndex =
        (mentionPicker.selectedIndex - 1 + len) % len
      return true
    }
    if (event.key === 'Enter' || event.key === 'Tab') {
      const item = mentionPicker.items[mentionPicker.selectedIndex]
      if (item && mentionPicker.command) mentionPicker.command(item)
      return true
    }
    return event.key === 'Escape'
  }
  function handleAttachmentKey(event: KeyboardEvent): boolean {
    const len = attachmentPicker.items.length
    if (len === 0) return event.key === 'Escape'
    if (event.key === 'ArrowDown') {
      attachmentPicker.selectedIndex = (attachmentPicker.selectedIndex + 1) % len
      return true
    }
    if (event.key === 'ArrowUp') {
      attachmentPicker.selectedIndex =
        (attachmentPicker.selectedIndex - 1 + len) % len
      return true
    }
    if (event.key === 'Enter' || event.key === 'Tab') {
      const item = attachmentPicker.items[attachmentPicker.selectedIndex]
      if (item && attachmentPicker.command) attachmentPicker.command(item)
      return true
    }
    return event.key === 'Escape'
  }

  // ── UnkaiBlock — atom node for embedded styled HTML blocks ─
  //
  // Tiptap's StarterKit schema unwraps generic <div> wrappers and
  // strips inline styles, so the modern invite cards + the muted
  // "previous conversation" block (#195) couldn't survive a round
  // trip through the editor. UnkaiBlock recognises any element
  // carrying `data-unkai-block="<kind>"`, captures its full
  // outerHTML into the node attribute, and renders it as a
  // contentEditable=false NodeView. The editor displays the
  // styled card verbatim; `editor.getHTML()` re-emits the
  // original markup for sending.
  //
  // The atom flag means the user can select-and-delete the
  // entire block with one Backspace — no separate × button
  // needed. Replaces the previous "out-of-editor preview"
  // approach with true in-editor WYSIWYG.
  //
  // (No image-URL swap any more: the chrome doesn't carry an
  // `<img>` because mail clients ate both the remote URL and
  // the inline data URI versions, leaving recipients with a
  // broken-icon. Brand header is typography-only now, so the
  // editor and the recipient see exactly the same thing.)
  const UnkaiBlock = TiptapNode.create({
    name: 'unkaiBlock',
    group: 'block',
    atom: true,
    selectable: true,
    draggable: false,
    parseHTML() {
      return [
        {
          tag: 'div[data-unkai-block]',
          getAttrs: (el) => {
            const dom = el as HTMLElement
            return { html: dom.outerHTML, kind: dom.getAttribute('data-unkai-block') }
          },
        },
      ]
    },
    addAttributes() {
      return {
        html: { default: '' },
        kind: { default: '' },
      }
    },
    renderHTML({ node }) {
      // Rehydrate the original element from the stored HTML so
      // `getHTML()` round-trips the full styled markup back out
      // for the recipient. Falls back to a bare wrapper if the
      // attribute somehow got cleared (defensive).
      const html = (node.attrs as { html: string }).html
      if (html) {
        const tmpl = document.createElement('template')
        tmpl.innerHTML = html
        const root = tmpl.content.firstElementChild
        if (root instanceof HTMLElement) return root
      }
      return [
        'div',
        mergeAttributes({
          'data-unkai-block': (node.attrs as { kind: string }).kind || '',
        }),
      ]
    },
    addNodeView() {
      return ({ node }) => {
        const dom = document.createElement('div')
        dom.setAttribute('contenteditable', 'false')
        dom.style.userSelect = 'none'
        dom.innerHTML = (node.attrs as { html: string }).html
        return { dom }
      }
    },
  })

  // svelte-ignore state_referenced_locally
  const editor = createEditor({
    extensions: [
      UnkaiBlock,
      StarterKit.configure({
        heading: { levels: [1, 2, 3] },
        // Tiptap 3 renamed `History` to `UndoRedo`. `newGroupDelay`
        // groups consecutive keystrokes inside a 500ms window into
        // one undo unit — Ctrl-Z then undoes a word (not a single
        // character), which is what every modern editor does.
        undoRedo: { newGroupDelay: 500 },
        // StarterKit ships its own Link + Underline; disable
        // them so the explicit `Link.extend(...)` and the
        // standalone `Underline` import below don't register a
        // second copy ("Duplicate extension names found:
        // ['link', 'underline']" in the console).
        link: false,
        underline: false,
      }),
      Underline,
      // Extend the Link mark so every rendered <a> carries a `title`
      // attribute equal to its href. Browsers show `title` as a native
      // tooltip on hover, which lets the user preview where a link
      // actually leads — important both for reviewing what we just
      // pasted and for spotting phishing-style "click here" anchors
      // whose visible text differs from the real destination.
      Link.extend({
        renderHTML({ HTMLAttributes }) {
          const href = (HTMLAttributes as { href?: string }).href
          return [
            'a',
            mergeAttributes(
              this.options.HTMLAttributes,
              HTMLAttributes,
              href ? { title: href } : {},
            ),
            0,
          ]
        },
      }).configure({
        openOnClick: false,
        // Auto-promote pasted / typed URLs into real anchors so
        // the receiving side's URLhaus check (#165) sees them
        // as `<a href>` rather than plain text — and so the
        // sender benefits from the same hover-tooltip behaviour
        // we add for explicitly-inserted links.
        autolink: true,
        HTMLAttributes: { target: '_blank', rel: 'noopener noreferrer' },
      }),
      // Image extended with drag-to-resize. A plain `<img>` doesn't
      // expose a native resize affordance, so we:
      //   1. add optional `width` / `height` attributes that parse
      //      from and render into the HTML, so sizes round-trip
      //      through save/open/send,
      //   2. plug in a NodeView that renders a wrapper span around
      //      the `<img>` with a small bottom-right corner handle;
      //      dragging the handle resizes the image in real time
      //      and commits the final width as an attr on pointer-up.
      // The NodeView is plain DOM (no Svelte NodeView wrapper) so
      // it stays under 80 lines and doesn't drag Svelte runtime
      // into ProseMirror's view layer.
      Image.extend({
        addAttributes() {
          return {
            ...this.parent?.(),
            width: {
              default: null,
              parseHTML: (el) => {
                const w = el.getAttribute('width')
                if (w && /^\d+$/.test(w)) return parseInt(w, 10)
                return null
              },
              renderHTML: (attrs) =>
                attrs.width ? { width: String(attrs.width) } : {},
            },
            height: {
              default: null,
              parseHTML: (el) => {
                const h = el.getAttribute('height')
                if (h && /^\d+$/.test(h)) return parseInt(h, 10)
                return null
              },
              renderHTML: (attrs) =>
                attrs.height ? { height: String(attrs.height) } : {},
            },
          }
        },
        addNodeView() {
          return ({ node, editor: ed, getPos }) => {
            const wrap = document.createElement('span')
            wrap.className = 'ev-resizable-img'
            wrap.style.display = 'inline-block'
            wrap.style.position = 'relative'
            wrap.style.maxWidth = '100%'

            const img = document.createElement('img')
            img.src = node.attrs.src
            img.alt = node.attrs.alt ?? ''
            if (node.attrs.width) img.style.width = `${node.attrs.width}px`
            img.style.maxWidth = '100%'
            img.style.height = 'auto'
            img.style.display = 'block'
            wrap.appendChild(img)

            const handle = document.createElement('span')
            handle.className = 'ev-resize-handle'
            handle.setAttribute('aria-hidden', 'true')
            wrap.appendChild(handle)

            handle.addEventListener('pointerdown', (e) => {
              e.preventDefault()
              e.stopPropagation()
              const startX = e.clientX
              const startWidth = img.offsetWidth || img.naturalWidth || 200
              let latestWidth = startWidth
              const onMove = (ev: PointerEvent) => {
                // Delta in layout pixels — the width attribute is
                // layout-space, so a raw client-coordinate delta
                // would out- or under-run the cursor while the
                // UI-scale zoom (#191) is active.
                latestWidth = Math.max(50, startWidth + visualDeltaToLayout(ev.clientX - startX))
                img.style.width = `${latestWidth}px`
              }
              const onUp = () => {
                window.removeEventListener('pointermove', onMove)
                window.removeEventListener('pointerup', onUp)
                const pos = typeof getPos === 'function' ? getPos() : null
                if (pos == null) return
                // Commit the final width as an attribute so it
                // round-trips through save/send. `setNodeSelection`
                // puts the cursor on the node so `updateAttributes`
                // lands on the right one.
                ed.chain()
                  .setNodeSelection(pos)
                  .updateAttributes('image', { width: Math.round(latestWidth) })
                  .run()
              }
              window.addEventListener('pointermove', onMove)
              window.addEventListener('pointerup', onUp)
            })

            return {
              dom: wrap,
              update(updatedNode) {
                // Reject updates of a different type so ProseMirror
                // falls back to full re-render; accept same-type
                // updates and re-sync the width.
                if (updatedNode.type.name !== 'image') return false
                img.src = updatedNode.attrs.src
                img.alt = updatedNode.attrs.alt ?? ''
                if (updatedNode.attrs.width) {
                  img.style.width = `${updatedNode.attrs.width}px`
                } else {
                  img.style.width = ''
                }
                return true
              },
            }
          }
        },
      }).configure({
        inline: true,
        // #248 — without this, Tiptap's `parseHTML` strips
        // `<img src="data:…">` on re-mount, so any inline-encoded
        // image (signature builder, NC files attached as data URLs,
        // pasted screenshots) vanishes the second time the editor
        // opens against the same body.  In-memory Compose hides
        // this because the editor never re-parses its own
        // `getHTML()`; the signature panel exposes it because the
        // settings tab unmounts + remounts on every visit.
        allowBase64: true,
      }),
      TextAlign.configure({ types: ['heading', 'paragraph'] }),
      // svelte-ignore state_referenced_locally
      Placeholder.configure({ placeholder }),
      // TextStyle is the mark that FontFamily / Color attach to;
      // the extensions are cumulative — adding more marks here
      // doesn't invalidate existing content.
      TextStyle,
      FontFamily,
      Color,
      Highlight.configure({ multicolor: true }),
      Table.configure({ resizable: true }),
      TableRow,
      // Extend TableCell with a backgroundColor attribute so the
      // toolbar's cell-colour picker has somewhere to write.  The
      // attr round-trips via inline `style="background-color: …"`,
      // which every mail client renders correctly without needing
      // a class-based stylesheet on the recipient side.
      TableCell.extend({
        addAttributes() {
          return {
            ...this.parent?.(),
            backgroundColor: {
              default: null,
              parseHTML: (el: HTMLElement) =>
                el.style.backgroundColor || null,
              renderHTML: (attrs: Record<string, unknown>) => {
                const c = attrs.backgroundColor
                if (!c) return {}
                return { style: `background-color: ${c}` }
              },
            },
          }
        },
      }),
      TableHeader,
      // ── `@` contact mention ────────────────────────────────
      // Renamed from the default `mention` so it can coexist with
      // the `/` attachment-ref extension below (Tiptap's Mention is
      // a single Node type — to register two we extend twice with
      // different `name`s).
      //
      // Wire format (#93): mailto-anchor wrapping the pill, plus
      // *inline* styles that mirror our editor's Tailwind look.
      // Most mail clients strip `class=` and external CSS but
      // keep `style` attrs and `title` tooltips, so recipients
      // see a styled pill + can hover to read the full address.
      // The `data-contact-mention` marker survives the round-trip
      // so a Unkai reply re-parses our own pills.
      Mention.extend({
        name: 'contactMention',
        renderHTML({ node, HTMLAttributes }) {
          const email = node.attrs.id ?? ''
          const label = node.attrs.label ?? email
          return [
            'a',
            {
              ...HTMLAttributes,
              href: `mailto:${email}`,
              title: label && email && label !== email ? `${label} <${email}>` : email,
              style:
                'display:inline-block;padding:0 4px;border-radius:4px;' +
                'background:rgba(59,130,246,0.12);color:#1d4ed8;' +
                'text-decoration:none;font-weight:500;',
            },
            `@${label}`,
          ]
        },
        // Plain-text serialization: feed `body_text` an RFC-style
        // address rather than just the bare display name so the
        // text-only fallback still tells a recipient who Alice is.
        renderText({ node }) {
          const label = node.attrs.label ?? node.attrs.id ?? ''
          const email = node.attrs.id ?? ''
          if (label && email && label !== email) return `${label} <${email}>`
          return email || label
        },
        parseHTML() {
          return [
            {
              tag: 'a[data-contact-mention]',
              getAttrs: (el) => {
                const href = (el as HTMLElement).getAttribute('href') ?? ''
                const email = href.replace(/^mailto:/, '')
                const text = (el as HTMLElement).textContent ?? ''
                const label = text.replace(/^@/, '') || email
                return { id: email, label }
              },
            },
          ]
        },
      }).configure({
        // Backspace deletes the whole mention node without re-
        // inserting the trigger character — without this, a
        // user removing a contact pill ends up with a stray `@`
        // glyph after the deletion.
        deleteTriggerWithBackspace: true,
        HTMLAttributes: {
          'data-contact-mention': '',
          class:
            'inline-block px-1 rounded bg-primary-500/15 text-primary-700 dark:text-primary-300 no-underline',
        },
        suggestion: {
          char: '@',
          // `items` is called every time the query changes. We
          // delegate to the parent's `oncontactquery` (Compose hands
          // back the merged participants + address-book list) and
          // stash `participantsCount` on the array so the popup
          // hooks can read it back without changing Tiptap's
          // expected return shape.
          items: async ({ query }) => {
            if (!oncontactquery) return []
            const { participants, others } = await oncontactquery(query)
            const merged = [...participants, ...others]
            ;(merged as unknown as { __pcount: number }).__pcount = participants.length
            return merged
          },
          command: ({ editor, range, props }) => {
            const c = props as ContactSuggestion
            editor
              .chain()
              .focus()
              .insertContentAt(range, [
                { type: 'contactMention', attrs: { id: c.email, label: c.label } },
                { type: 'text', text: ' ' },
              ])
              .run()
            oncontactpicked?.(c)
          },
          render: () => ({
            onStart: (props) => {
              const items = props.items as ContactSuggestion[]
              const pcount =
                (items as unknown as { __pcount?: number }).__pcount ?? 0
              mentionPicker.items = items
              mentionPicker.participantsCount = pcount
              mentionPicker.selectedIndex = 0
              mentionPicker.command = (c) =>
                (props.command as (data: ContactSuggestion) => void)(c)
              mentionPicker.position = anchorBelow(props.clientRect?.())
              mentionPicker.visible = true
            },
            onUpdate: (props) => {
              const items = props.items as ContactSuggestion[]
              const pcount =
                (items as unknown as { __pcount?: number }).__pcount ?? 0
              mentionPicker.items = items
              mentionPicker.participantsCount = pcount
              mentionPicker.selectedIndex = 0
              mentionPicker.command = (c) =>
                (props.command as (data: ContactSuggestion) => void)(c)
              mentionPicker.position = anchorBelow(props.clientRect?.())
            },
            onKeyDown: ({ event }) => handleMentionKey(event),
            onExit: () => {
              mentionPicker.visible = false
              mentionPicker.items = []
              mentionPicker.command = null
            },
          }),
        },
      }),

      // ── `/` attachment reference ───────────────────────────
      // Same Mention machinery, different node + char + render.
      //
      // Wire format (#93): cid-anchor + inline styles + a title
      // tooltip identifying it as an attachment.  Clients that
      // resolve `cid:` (including our own renderer) get a direct
      // jump to the part; clients that strip `cid:` URLs still
      // show a styled "🖇️ filename (attached)" badge and the file
      // is in the message's attachment tray anyway.
      //
      // The plain-text fallback annotates the filename with
      // "(attached)" so a recipient reading the text/plain part
      // immediately knows the reference points at the attachment
      // tray, not a missing inline link.
      Mention.extend({
        name: 'attachmentRef',
        // Render as <a href="cid:..."> so Unkai's own reader
        // (MailView.processEmailHtml) picks it up the same way
        // it always did — by matching cid: anchors and routing
        // their click to the attachment via `data-unkai-cid`.
        // The data-* attrs (data-cid, data-filename,
        // data-attachment-ref) ride along for the new robust
        // click handler and as a cross-client identifier.
        // Cross-client trade-off: clients that strip `cid:` may
        // surface a "can't open cid:..." error if the user
        // *clicks* the link.  The visible badge + filename is
        // the part those recipients are meant to read; the
        // hyperlink is purely a Unkai affordance.
        renderHTML({ node, HTMLAttributes }) {
          const cid = node.attrs.id ?? ''
          const label = node.attrs.label ?? cid
          // Render an inline-styled file-type badge + filename
          // instead of a leading emoji.  A coloured rounded box
          // with the format code (PDF/DOC/XLS/PPT/CSV/ZIP) in
          // bold survives every major MUA's content sanitiser
          // because it's just <span style="..."> — no <svg>, no
          // class names, no external CSS.  Recipients on any
          // major mail client get a typed-icon look close to
          // what they'd see in a file explorer.
          //
          // `data-label` carries the filename verbatim so a
          // round-trip back into Tiptap recovers it without
          // having to slice the badge out of textContent.
          const ext = label.includes('.')
            ? label.slice(label.lastIndexOf('.') + 1).toLowerCase()
            : ''
          let badgeText = 'FILE'
          let badgeBg = '#64748b' // slate
          if (ext === 'pdf') {
            badgeText = 'PDF'
            badgeBg = '#e11d48' // rose
          } else if (
            ['doc', 'docx', 'docm', 'dot', 'dotx', 'dotm', 'odt', 'ott', 'rtf'].includes(ext)
          ) {
            badgeText = 'DOC'
            badgeBg = '#2563eb' // blue
          } else if (['xls', 'xlsx', 'xlsm', 'xlt', 'xltx', 'xltm', 'ods', 'ots'].includes(ext)) {
            badgeText = 'XLS'
            badgeBg = '#059669' // emerald-600
          } else if (['csv', 'tsv'].includes(ext)) {
            badgeText = 'CSV'
            badgeBg = '#10b981' // emerald-500
          } else if (['ppt', 'pptx', 'pptm', 'pot', 'potx', 'potm', 'odp', 'otp'].includes(ext)) {
            badgeText = 'PPT'
            badgeBg = '#f59e0b' // amber
          } else if (['zip', '7z', 'rar', 'tar', 'gz', 'xz', 'bz2', 'tgz'].includes(ext)) {
            badgeText = 'ZIP'
            badgeBg = '#7c3aed' // violet
          } else if (['md', 'markdown', 'mdx', 'mkd'].includes(ext)) {
            badgeText = 'MD'
            badgeBg = '#0ea5e9' // sky
          } else if (
            ['jpg', 'jpeg', 'png', 'gif', 'webp', 'avif', 'bmp', 'tif', 'tiff', 'svg', 'heic', 'heif', 'ico'].includes(ext)
          ) {
            badgeText = (ext === 'jpeg' ? 'JPG' : ext.toUpperCase()).slice(0, 4)
            badgeBg = '#06b6d4' // cyan
          } else if (
            ['mp4', 'mkv', 'mov', 'avi', 'wmv', 'flv', 'webm', 'm4v', 'mpg', 'mpeg', '3gp'].includes(ext)
          ) {
            badgeText = ext.toUpperCase().slice(0, 4)
            badgeBg = '#ec4899' // pink
          } else if (
            ['mp3', 'flac', 'wav', 'ogg', 'm4a', 'aac', 'opus', 'wma', 'aiff', 'alac'].includes(ext)
          ) {
            badgeText = ext.toUpperCase().slice(0, 4)
            badgeBg = '#a855f7' // purple
          }
          // Body badge (#158 final): plain coloured pill.  The
          // document-mark experiment was disrupting the body
          // line height regardless of how we sized it, so we
          // keep the FileTypeIcon document treatment on the
          // chip strip (which has its own row) and use a
          // simple inline pill in flowing body text.  Same
          // colour palette so the format is still recognised
          // at a glance; pure inline-styled <span> so it
          // survives every MUA sanitiser and renders
          // identically across clients.
          const badgeStyle =
            'display:inline-block;min-width:2.4em;padding:1px 6px;margin-right:6px;' +
            `background:${badgeBg};color:#ffffff;` +
            'border-radius:4px;font-size:0.68em;font-weight:800;' +
            'font-family:ui-sans-serif,system-ui,-apple-system,sans-serif;' +
            'letter-spacing:0.05em;text-align:center;vertical-align:middle;' +
            'line-height:1.4;'
          return [
            'a',
            {
              ...HTMLAttributes,
              href: `cid:${cid}`,
              title: `Attached file: ${label}`,
              'data-label': label,
              'data-cid': cid,
              'data-filename': label,
              style:
                'display:inline-flex;align-items:center;padding:1px 8px 1px 3px;' +
                'border-radius:999px;background:rgba(120,120,120,0.10);' +
                'color:inherit;text-decoration:none;font-weight:500;',
            },
            ['span', { style: badgeStyle }, badgeText],
            label,
          ]
        },
        renderText({ node }) {
          const label = node.attrs.label ?? ''
          return label ? `${label} (attached)` : ''
        },
        parseHTML() {
          return [
            {
              // Match both the new <span> and the legacy <a>
              // shape so messages composed before the cid-anchor
              // → span migration still re-import correctly.
              tag: '[data-attachment-ref]',
              getAttrs: (el) => {
                const e = el as HTMLElement
                // Prefer `data-cid` (new), fall back to parsing
                // the legacy href="cid:..." attribute.
                const dataCid = e.getAttribute('data-cid')
                const href = e.getAttribute('href') ?? ''
                const id = dataCid || href.replace(/^cid:/, '')
                // `data-label` is the canonical recovery path
                // (set by renderHTML).  Older messages that
                // pre-date the badge still parse via textContent
                // with the leading badge code or emoji stripped.
                const dataLabel = e.getAttribute('data-label') ?? e.getAttribute('data-filename')
                if (dataLabel) return { id, label: dataLabel }
                const text = e.textContent ?? ''
                const label =
                  text
                    // Strip any 2–4 letter ALL-CAPS leader
                    // (PDF / DOC / XLS / CSV / PPT / ZIP / MD /
                    // JPG / PNG / SVG / TIFF / FILE / …) the
                    // wire-format renderer might have prepended.
                    .replace(/^[A-Z]{2,4}\s+/, '')
                    .replace(/^(?:🖇️|📕|📘|📗|📙|🗜️|📎|🔗)\s*/, '') || id
                return { id, label }
              },
            },
          ]
        },
      }).configure({
        // Backspace removes the whole reference node without
        // dropping a stray `/` glyph — same reasoning as the
        // contactMention configure block above.
        deleteTriggerWithBackspace: true,
        // Use a truthy marker value rather than the empty
        // string — some HTML serialisers drop empty-valued
        // attributes during round-trip, which would silently
        // break the MailView click handler that looks for
        // `[data-attachment-ref]`.
        HTMLAttributes: {
          'data-attachment-ref': 'true',
          class: 'inline-block text-primary-600 dark:text-primary-400 underline',
        },
        suggestion: {
          char: '/',
          items: ({ query }) => {
            const q = query.trim().toLowerCase()
            return attachmentsForRef
              .filter((a) => a.content_id)
              .filter((a) => !q || a.filename.toLowerCase().includes(q))
              .slice(0, 8)
          },
          command: ({ editor, range, props }) => {
            // Tiptap types `props` as MentionNodeAttrs; we always
            // pass our own AttachmentRef shape from `items` above,
            // so the unknown-cast is the standard escape hatch.
            const a = props as unknown as AttachmentRef
            editor
              .chain()
              .focus()
              .insertContentAt(range, [
                { type: 'attachmentRef', attrs: { id: a.content_id, label: a.filename } },
                { type: 'text', text: ' ' },
              ])
              .run()
          },
          render: () => ({
            onStart: (props) => {
              attachmentPicker.items = props.items as unknown as AttachmentRef[]
              attachmentPicker.selectedIndex = 0
              attachmentPicker.command = props.command as unknown as (
                data: AttachmentRef,
              ) => void
              attachmentPicker.position = anchorBelow(props.clientRect?.())
              attachmentPicker.visible = true
            },
            onUpdate: (props) => {
              attachmentPicker.items = props.items as unknown as AttachmentRef[]
              attachmentPicker.selectedIndex = 0
              attachmentPicker.command = props.command as unknown as (
                data: AttachmentRef,
              ) => void
              attachmentPicker.position = anchorBelow(props.clientRect?.())
            },
            onKeyDown: ({ event }) => handleAttachmentKey(event),
            onExit: () => {
              attachmentPicker.visible = false
              attachmentPicker.items = []
              attachmentPicker.command = null
            },
          }),
        },
      }),
    ],
    // svelte-ignore state_referenced_locally
    content,
    onUpdate: ({ editor: e }) => {
      onchange?.(e.getHTML())
    },
  })

  onDestroy(() => {
    $editor?.destroy()
  })

  // Hand the parent a small imperative API once the editor is live.
  // Tiptap's createEditor returns a Readable store that publishes the
  // instance asynchronously (after the DOM mounts), so we wait for the
  // first non-null value before firing onready.
  $effect(() => {
    const ed = $editor
    if (ed && onready) {
      onready({
        appendHtml: (html: string) => {
          // `insertContentAt(end)` keeps the user's selection where it
          // is — appending a paragraph at the document end is the
          // expected gesture for "append" rather than "insert here".
          ed.chain().insertContentAt(ed.state.doc.content.size, html).run()
        },
        setHtml: (html: string) => {
          // `emitUpdate: false` skips the `onUpdate` callback — the
          // caller already knows the new content (they passed it) and
          // we don't want to round-trip back through `onchange` and
          // re-trigger reactive effects watching `bodyHtml`.
          ed.commands.setContent(html, { emitUpdate: false })
        },
        insertImage: (src: string) => {
          // Run through `chain().focus()` so the editor takes focus
          // even if the insert was triggered from a modal in the
          // parent — otherwise Tiptap's selection can sit outside
          // the document and the image lands in the wrong place.
          ed.chain().focus().setImage({ src }).run()
        },
        insertAboveSignature: (html: string) => {
          // Single doc walk collects the two landmark positions
          // we need: the topmost `unkaiBlock` (any kind —
          // meeting / Talk / share-card or quoted-history) and
          // the signature paragraph.  RFC 3676 separator is
          // exactly `-- ` (dash-dash-space) on its own line;
          // Tiptap may strip the trailing space on parse so we
          // accept both `-- ` and a bare `--` paragraph as the
          // signature marker.
          let firstBlockPos: number | null = null
          let sigPos: number | null = null
          ed.state.doc.descendants((node, p) => {
            if (firstBlockPos === null && node.type.name === 'unkaiBlock') {
              firstBlockPos = p
            }
            if (sigPos === null && node.type.name === 'paragraph') {
              const text = node.textContent
              if (text === '--' || text.startsWith('-- ')) {
                sigPos = p
              }
            }
            return true
          })

          // Land at the top of the signoff region so new blocks
          // stack ABOVE previously inserted cards (a tiny share
          // link otherwise disappears beneath a big meeting card
          // — #320).  Cases:
          //
          //   - unkaiBlock sitting above sig (or sig absent) →
          //     insert at that block's position so the new
          //     content reads first.
          //   - sig present, no block above it → insert at sig
          //     position (first card in the signoff region).
          //   - neither found → append at end (fresh compose
          //     with no signature and no cards yet — the block
          //     lands below the lead-spacer typing area).
          let pos: number
          if (firstBlockPos !== null && (sigPos === null || firstBlockPos < sigPos)) {
            pos = firstBlockPos
          } else if (sigPos !== null) {
            pos = sigPos
          } else {
            pos = ed.state.doc.content.size
          }
          ed.chain().insertContentAt(pos, html).run()
        },
      })
    }
  })

  // ── Toolbar helpers ─────────────────────────────────────────

  // ── Toolbar helpers ─────────────────────────────────────────
  // Each helper grabs the editor from the store at call time (not
  // capture time) so it's always the live instance.

  function cmd() {
    return $editor!.chain().focus()
  }

  function toggleHeading(level: 1 | 2 | 3) {
    cmd().toggleHeading({ level }).run()
  }

  function doUndo() { cmd().undo().run() }
  function doRedo() { cmd().redo().run() }

  // ── Link popover ────────────────────────────────────────────
  // #317 — `window.prompt` is disabled in Tauri 2 webviews and even
  // when supported it stole focus from the editor and dropped the
  // selection, leaving us with no range for `extendMarkRange` to
  // operate on.  Replaced with an inline popover anchored to the
  // Link toolbar button, matching the font / table / emoji picker
  // idiom already in this file.
  let showLinkPopover = $state(false)
  let linkPopoverPos = $state<{ x: number; y: number }>({ x: 0, y: 0 })
  let linkUrlInput = $state('')
  /** Whether the click that opened the popover landed inside an
   *  existing link.  Drives the visibility of the Remove button —
   *  there's nothing to remove when the user is creating a new
   *  link, so hiding the button keeps the popover compact. */
  let linkPopoverHasExisting = $state(false)
  /** Reference to the URL input so we can autofocus + select on
   *  open without round-tripping through a `bind:this` snapshot. */
  let linkInputEl: HTMLInputElement | null = $state(null)

  function openLinkPopover(e: MouseEvent) {
    const prev = ($editor?.getAttributes('link')?.href as string) ?? ''
    linkUrlInput = prev
    linkPopoverHasExisting = prev !== ''
    linkPopoverPos = anchorPopover(e)
    showLinkPopover = true
  }

  /** Apply the URL currently in the input.  Empty string removes
   *  the link (matches the previous `window.prompt` behaviour).
   *
   *  Three cases:
   *    1. URL empty + on existing link → strip the link mark.
   *    2. URL non-empty + range selected → apply the link mark to
   *       the selection (Tiptap's `extendMarkRange('link')` widens
   *       the range to cover the full anchor text when the cursor
   *       sits inside one).
   *    3. URL non-empty + cursor only (no selection) → insert the
   *       URL as visible text with the link mark.  Without this
   *       branch `setLink` silently no-ops because there's no
   *       range to apply the mark to — which is exactly the
   *       "Insert link does nothing" symptom from #317. */
  function commitLink() {
    const url = linkUrlInput.trim()
    if (url === '') {
      cmd().extendMarkRange('link').unsetLink().run()
    } else if ($editor && $editor.state.selection.empty) {
      cmd()
        .insertContent({
          type: 'text',
          text: url,
          marks: [{ type: 'link', attrs: { href: url } }],
        })
        .run()
    } else {
      cmd().extendMarkRange('link').setLink({ href: url }).run()
    }
    showLinkPopover = false
  }

  function removeLink() {
    cmd().extendMarkRange('link').unsetLink().run()
    showLinkPopover = false
  }

  /** Insert an image from a local file (embedded as data URL). */
  function addImageFromFile() {
    const input = document.createElement('input')
    input.type = 'file'
    input.accept = 'image/*'
    input.onchange = () => {
      const file = input.files?.[0]
      if (!file) return
      const reader = new FileReader()
      reader.onload = () => {
        const src = reader.result as string
        cmd().setImage({ src }).run()
      }
      reader.readAsDataURL(file)
    }
    input.click()
  }

  /** Request an image via the parent-supplied picker, or fall back
   *  to a raw URL prompt if the embedder didn't provide one. The
   *  picker path (Compose → NextcloudFilePicker) is what users
   *  actually want — the URL prompt just keeps the component
   *  self-contained for anywhere we reuse it without a Nextcloud
   *  backend. */
  function addImageFromNcOrUrl() {
    if (onrequestncimage) {
      onrequestncimage()
      return
    }
    const url = window.prompt('Image URL')
    if (url) {
      cmd().setImage({ src: url }).run()
    }
  }

  // ── Font family picker ─────────────────────────────────────
  //
  // Families we expose in the toolbar. Each entry is `{label, css}`
  // — label is what the user sees, `css` is the literal
  // `font-family` value Tiptap writes into `<span style="font-
  // family: …">`. Using system-font stacks (not single names)
  // means the recipient's client renders something reasonable
  // even when their OS doesn't have the exact face installed.
  const CURATED_FONTS: Array<{ label: string; css: string }> = [
    { label: 'Default', css: '' },
    {
      label: 'Sans-serif',
      css: '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
    },
    { label: 'Serif', css: 'Georgia, "Times New Roman", Times, serif' },
    {
      label: 'Monospace',
      css: '"SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
    },
    { label: 'Arial', css: 'Arial, Helvetica, sans-serif' },
    { label: 'Times', css: '"Times New Roman", Times, serif' },
  ]
  /** OS-installed font families discovered via the
   *  `list_system_fonts` Tauri command (#142).  Loaded once on
   *  mount; failures (sandboxed dev / missing perm) leave the
   *  list empty so the dropdown still shows the curated stacks.
   *  Module-scope cached so multiple compose windows in one
   *  session don't each round-trip to the backend. */
  let systemFonts = $state<string[]>(systemFontsCache)
  $effect(() => {
    if (systemFontsCache.length > 0) {
      systemFonts = systemFontsCache
      return
    }
    void api.system.listSystemFonts()
      .then((rows) => {
        const curatedLabels = new Set(
          CURATED_FONTS.map((f) => f.label.toLowerCase()),
        )
        systemFontsCache = rows.filter((f) => !curatedLabels.has(f.toLowerCase()))
        systemFonts = systemFontsCache
      })
      .catch((e) => {
        console.warn('list_system_fonts failed', e)
      })
  })
  /** Quote a single family name for safe insertion into a CSS
   *  `font-family` string.  Names that contain spaces or non-
   *  word characters need surrounding quotes per CSS3. */
  function familyToCss(name: string): string {
    if (/^[\w-]+$/.test(name)) return name
    return `"${name.replace(/"/g, '\\"')}"`
  }
  /** Combined picker rows — curated stacks at the top, then the
   *  user's OS fonts filtered by the in-picker search box. */
  const FONT_FAMILIES = $derived.by(() => {
    const q = fontPickerQuery.trim().toLowerCase()
    const curated = q
      ? CURATED_FONTS.filter((f) => f.label.toLowerCase().includes(q))
      : CURATED_FONTS
    const os = (q
      ? systemFonts.filter((f) => f.toLowerCase().includes(q))
      : systemFonts
    ).map((f) => ({ label: f, css: familyToCss(f) }))
    return [...curated, ...os]
  })
  let showFontPicker = $state(false)
  let fontPickerQuery = $state('')
  // ── Font picker windowing (#142 follow-up) ─────────────────
  // Even with content-visibility, Svelte mounts a DOM node per
  // {#each} entry on first show.  ~500 buttons × per-button
  // font-family = perceptible click-to-open lag on the first
  // open after launch.  Window the list manually: render only
  // the rows actually visible plus a small buffer, with a tall
  // spacer reserving the full scroll height.
  const FONT_ROW_H = 28
  const FONT_VIEWPORT_H = 288 // mirrors `max-h-72`
  // Buffer is the compromise between open-latency (more rows
  // = more DOM creation on first show) and scroll smoothness
  // (more rows = a fast flick stays inside the mounted window).
  // 20 rows ≈ 560px of overshoot on either side, ~50 rows
  // mounted total, which keeps both ends snappy.
  const FONT_BUFFER = 20
  let fontScrollY = $state(0)
  let fontScrollEl: HTMLDivElement | null = $state(null)
  const fontWindow = $derived.by(() => {
    const total = FONT_FAMILIES.length
    const visibleCount = Math.ceil(FONT_VIEWPORT_H / FONT_ROW_H) + FONT_BUFFER * 2
    const start = Math.max(0, Math.floor(fontScrollY / FONT_ROW_H) - FONT_BUFFER)
    const end = Math.min(total, start + visibleCount)
    return {
      start,
      end,
      total,
      slice: FONT_FAMILIES.slice(start, end),
    }
  })
  // Reset scroll back to 0 whenever the picker opens or the
  // search query changes — otherwise a previous scroll position
  // would carry over into a filtered list whose total height is
  // smaller, leaving the user staring at empty space.
  $effect(() => {
    void fontPickerQuery
    void showFontPicker
    if (fontScrollEl) fontScrollEl.scrollTop = 0
    fontScrollY = 0
  })

  /** Label for the toolbar button, reflecting the font at the current
      cursor position. `$editor` is a svelte-tiptap store that re-emits
      on every editor transaction, so this function re-runs after every
      selection change or edit — the label flips in step with the
      cursor. Falls back to the generic "Font" when the cursor sits in
      text carrying a family we don't have a pretty label for. */
  function currentFontLabel(): string {
    if (!$editor) return 'Font'
    const css = ($editor.getAttributes('textStyle')?.fontFamily as string | undefined) ?? ''
    if (!css) return 'Default'
    const curated = CURATED_FONTS.find((f) => f.css === css)
    if (curated) return curated.label
    const sys = systemFonts.find((f) => familyToCss(f) === css)
    if (sys) return sys
    return 'Font'
  }

  function setFont(css: string) {
    showFontPicker = false
    if (!$editor) return
    const ed = $editor

    // Non-empty selection: apply the mark to the selected range. This
    // has always worked via Tiptap's `setFontFamily` helper and we
    // keep using it so existing behavior (selected text rewrites to
    // the new face) is unchanged.
    if (!ed.state.selection.empty) {
      if (css === '') {
        ed.chain().focus().unsetFontFamily().run()
      } else {
        ed.chain().focus().setFontFamily(css).run()
      }
      return
    }

    // Empty selection: we want the *next* typed characters to use the
    // picked font. Tiptap's `setFontFamily` delegates to ProseMirror's
    // `setMark`, which on an empty selection adds to `storedMarks` —
    // in theory that's enough. In practice, because the toolbar
    // button steals focus on click and the click-to-editor focus
    // handoff produces an extra selection transaction, the stored
    // mark was getting cleared before the user's next keystroke.
    //
    // Dispatching `addStoredMark` as a standalone transaction —
    // *after* we explicitly return focus to the editor — sidesteps
    // the focus-handoff race: by the time this tr lands, the editor
    // owns focus and the selection is stable, so the mark stays
    // attached to the cursor position and rides along with the next
    // character the user types.
    ed.commands.focus()
    const { state, view } = ed
    const markType = state.schema.marks.textStyle
    if (!markType) return
    let tr = state.tr
    tr = css === ''
      ? tr.removeStoredMark(markType)
      : tr.addStoredMark(markType.create({ fontFamily: css }))
    view.dispatch(tr)
  }

  // ── Table grid picker state ────────────────────────────────
  let showTablePicker = $state(false)
  let tableHoverRows = $state(0)
  let tableHoverCols = $state(0)
  let tablePickerAnchor = $state<HTMLDivElement | null>(null)
  const TABLE_GRID = 8  // 8x8 picker, the common mail-toolbar size

  /** Outside-click dismissal for the table picker (#199).  Mirrors
   *  the project-wide idiom from CLAUDE.md: register the listener
   *  inside an `$effect` keyed on the open state, defer one tick
   *  so the click that opened the picker doesn't immediately
   *  close it, tear down on close. */
  $effect(() => {
    if (!showTablePicker) return
    const onMouseDown = (e: MouseEvent) => {
      if (tablePickerAnchor && !tablePickerAnchor.contains(e.target as Node)) {
        showTablePicker = false
        tableHoverRows = 0
        tableHoverCols = 0
      }
    }
    const id = setTimeout(
      () => document.addEventListener('mousedown', onMouseDown),
      0,
    )
    return () => {
      clearTimeout(id)
      document.removeEventListener('mousedown', onMouseDown)
    }
  })

  /** Selection (cursor position) snapshotted when the user opens
   *  the table picker.  We restore it before inserting so the
   *  table lands where the user's cursor was, not wherever Tiptap
   *  thinks the focus moved to during the dropdown interaction. */
  let savedTableSelection: { from: number; to: number } | null = null

  function openTablePicker() {
    if ($editor) {
      const sel = $editor.state.selection
      savedTableSelection = { from: sel.from, to: sel.to }
    }
    showTablePicker = !showTablePicker
  }

  function insertTable(rows: number, cols: number) {
    if (savedTableSelection) {
      cmd()
        .setTextSelection(savedTableSelection)
        .insertTable({ rows, cols, withHeaderRow: true })
        .run()
    } else {
      cmd().insertTable({ rows, cols, withHeaderRow: true }).run()
    }
    savedTableSelection = null
    showTablePicker = false
  }

  // ── Table editing actions (#103 follow-up) ─────────────────────
  // Thin wrappers around Tiptap's table commands.  The toolbar
  // disables them when the cursor isn't inside a table, so calling
  // them in that state would no-op anyway — but keeping the chain
  // explicit means future "is the cursor in a header row?" checks
  // can branch here without each call site reimplementing it.
  function tblAddRowAbove() { cmd().addRowBefore().run() }
  function tblAddRowBelow() { cmd().addRowAfter().run() }
  function tblAddColLeft()  { cmd().addColumnBefore().run() }
  function tblAddColRight() { cmd().addColumnAfter().run() }
  function tblDeleteRow()   { cmd().deleteRow().run() }
  function tblDeleteCol()   { cmd().deleteColumn().run() }
  function tblDelete()      { cmd().deleteTable().run() }
  function tblSetCellColor(e: Event) {
    const color = (e.target as HTMLInputElement).value
    cmd().setCellAttribute('backgroundColor', color).run()
  }
  function tblClearCellColor() {
    cmd().setCellAttribute('backgroundColor', null).run()
  }

  /** Reactive: is the cursor currently inside a table?  Drives the
   *  enabled/disabled state of the table-edit buttons in the
   *  Insert tab. */
  function tableActive(): boolean {
    return !!$editor?.isActive('table')
  }

  // ── Ribbon tab + emoji-picker state (#103 follow-up) ──────────
  // The toolbar is split into ribbon-style tabs.  `activeTab`
  // drives which panel is rendered below the tab strip; values
  // beyond the built-in three come from the embedder's
  // `extraTabs` prop (Compose contributes "attach").
  type BuiltinTab = 'format' | 'insert' | 'layout'
  // svelte-ignore state_referenced_locally
  let activeTab = $state<BuiltinTab | string>(defaultActiveTab ?? 'format')

  let showEmojiPicker = $state(false)

  /** Toolbar dropdown anchor coordinates (#267).  The toolbar panel
   *  uses `overflow-x: auto` for narrow-width horizontal scrolling,
   *  which per CSS spec also forces `overflow-y` to `auto` and clips
   *  vertically.  An `position: absolute` popover inside that
   *  container therefore disappears below the editor area + the
   *  horizontal scrollbar.  We dodge the clip by `position: fixed`-ing
   *  the popovers and stashing the trigger's `getBoundingClientRect()`
   *  on open so the menu still lands directly under its button.
   *  Same shape Notes uses for its folder action menu. */
  let fontPickerPos = $state<{ x: number; y: number }>({ x: 0, y: 0 })
  let tablePickerPos = $state<{ x: number; y: number }>({ x: 0, y: 0 })
  let emojiPickerPos = $state<{ x: number; y: number }>({ x: 0, y: 0 })

  /** Stash the trigger's screen-space rect so the matching popover
   *  can render at `position: fixed` above any clipping ancestor.
   *  Distinct from the `anchorBelow(rect)` helper above — that one
   *  takes a Tiptap-suggestion DOMRect; this one reads from a
   *  click event's currentTarget. */
  function anchorPopover(e: MouseEvent): { x: number; y: number } {
    const r = anchorRect(e.currentTarget as HTMLElement)
    return { x: r.left, y: r.bottom + 4 }
  }

  /** Document-level Escape dismissal for the three toolbar
   *  popovers.  The popovers' inline `onkeydown={...Escape...}`
   *  handlers only fire when focus is inside the popover; with
   *  `position: fixed` (and most pickers having no auto-focused
   *  input) focus tends to stay on the trigger button, so the
   *  inline handler never sees the keystroke.  Subscribing at
   *  the document level catches Escape from anywhere in the
   *  Compose window. */
  $effect(() => {
    if (
      !showFontPicker &&
      !showTablePicker &&
      !showEmojiPicker &&
      !showLinkPopover
    )
      return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      if (showFontPicker) {
        showFontPicker = false
        fontPickerQuery = ''
      }
      if (showTablePicker) showTablePicker = false
      if (showEmojiPicker) showEmojiPicker = false
      if (showLinkPopover) showLinkPopover = false
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  })

  // Click-outside dismissal for the link popover — same idiom as
  // the emoji popup below.  Crucial here because the popover takes
  // input focus on open, so the user expects clicking elsewhere
  // (back into the editor, the toolbar, …) to dismiss it.
  $effect(() => {
    if (!showLinkPopover) return
    const close = () => (showLinkPopover = false)
    const handle = setTimeout(() => {
      window.addEventListener('click', close)
    }, 0)
    return () => {
      clearTimeout(handle)
      window.removeEventListener('click', close)
    }
  })

  // Autofocus the URL input every time the popover opens, and pre-
  // select its content so the user can immediately overtype an
  // existing URL.  Keyed on `showLinkPopover` so re-opening the
  // popover (close → reopen on a different word) re-runs the focus.
  // setTimeout(0) rather than queueMicrotask so we land *after* the
  // DOM has actually committed — queueMicrotask can race the render
  // and find `linkInputEl` still null.
  $effect(() => {
    if (!showLinkPopover) return
    const t = setTimeout(() => {
      linkInputEl?.focus()
      linkInputEl?.select()
    }, 0)
    return () => clearTimeout(t)
  })

  function insertEmoji(e: string | null) {
    if (!e) return
    cmd().insertContent(e).run()
    showEmojiPicker = false
  }

  // Click-outside dismissal for the emoji popup.  The popup itself
  // stops propagation on its own click handler, so any click that
  // reaches `document` originated outside it.  We delay the install
  // by one tick (`setTimeout(0)`) so the click that *opened* the
  // popup doesn't immediately close it.  Same idiom would work for
  // the other popups (font / table) but the user only flagged the
  // emoji picker — keeping scope tight.
  $effect(() => {
    if (!showEmojiPicker) return
    const close = () => (showEmojiPicker = false)
    const handle = setTimeout(() => {
      window.addEventListener('click', close)
    }, 0)
    return () => {
      clearTimeout(handle)
      window.removeEventListener('click', close)
    }
  })

  /** Strip every mark + collapse the current block down to the
   *  default paragraph node.  The "Clear formatting" button. */
  function clearFormatting() {
    cmd().unsetAllMarks().clearNodes().run()
  }

  function setColor(e: Event) {
    const color = (e.target as HTMLInputElement).value
    cmd().setColor(color).run()
  }

  function setHighlight(e: Event) {
    const color = (e.target as HTMLInputElement).value
    cmd().toggleHighlight({ color }).run()
  }

  // Reactive "is active" helpers for styling toolbar buttons.
  // Returns the `is-active` class which `.rt-btn` (panel buttons)
  // and `.tb` (compact buttons) both pick up via `:global` rules.
  const ACTIVE_CLS = 'is-active'

  function active(name: string, attrs?: Record<string, unknown>): string {
    return $editor?.isActive(name, attrs) ? ACTIVE_CLS : ''
  }

  function activeAttrs(attrs: Record<string, unknown>): string {
    return $editor?.isActive(attrs) ? ACTIVE_CLS : ''
  }
</script>

<style>
  /* Compact toolbar buttons — used by the tab strip's undo/redo +
     embedder-supplied trailing actions where vertical space is
     scarce.  #152: bumped padding + font size in lockstep with
     the ribbon's `.rt-btn` resize so undo / redo / send /
     discard / save all read at the same visual weight; the
     previous 0.75rem font + 14px icons looked like an
     afterthought next to the wider tabs.  Same idiom as before
     otherwise; the ribbon's panel buttons get `.rt-btn` styling
     instead. */
  :global(.tb) {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0.375rem 0.75rem;
    border-radius: 0.25rem;
    font-size: 0.875rem;
    line-height: 1;
    cursor: pointer;
    transition: background 0.1s;
    position: relative;
    border: none;
    background: transparent;
    color: inherit;
  }
  :global(.tb:hover:not(:disabled)) {
    background: var(--color-surface-200);
  }
  :global(.dark .tb:hover:not(:disabled)) {
    background: var(--color-surface-700);
  }
  :global(.tb:disabled) {
    opacity: 0.4;
    cursor: not-allowed;
  }
  :global(.tb.is-active) {
    background: var(--color-surface-300);
  }
  :global(.dark .tb.is-active) {
    background: var(--color-surface-600);
  }

  /* ── Ribbon-style tab strip (#103 follow-up) ─────────────────── */

  /* Horizontally-scrolling tab list (#57 follow-up).  Shows a thin
     themed scrollbar only when the tab list actually overflows the
     available width — the user needs the visual cue that more tabs
     exist off to the side and the bar gives a draggable handle.
     Kept thin (6px) so it doesn't compete with the tabs'
     active-state underline; tinted to the surface palette so it
     reads as ribbon chrome rather than browser default. */
  :global(.rt-tab-scroller) {
    scrollbar-width: thin; /* Firefox */
    scrollbar-color: var(--color-surface-400) transparent;
  }
  :global(.dark .rt-tab-scroller) {
    scrollbar-color: var(--color-surface-600) transparent;
  }
  :global(.rt-tab-scroller::-webkit-scrollbar) {
    height: 6px;
  }
  :global(.rt-tab-scroller::-webkit-scrollbar-track) {
    background: transparent;
  }
  :global(.rt-tab-scroller::-webkit-scrollbar-thumb) {
    background-color: var(--color-surface-400);
    border-radius: 3px;
  }
  :global(.dark .rt-tab-scroller::-webkit-scrollbar-thumb) {
    background-color: var(--color-surface-600);
  }
  :global(.rt-tab-scroller::-webkit-scrollbar-thumb:hover) {
    background-color: var(--color-surface-500);
  }

  /* Tab buttons.  Rounded-top chip with a primary-colour underline
     when active, matching the ribbon-style tab look. */
  :global(.rt-tab) {
    padding: 0.45rem 1rem;
    font-size: 0.8125rem;
    font-weight: 500;
    background: transparent;
    border: none;
    /* Inactive tabs sit exactly on the muted tier — the readability
       floor for text on glass chrome (#453) — never below it. */
    color: var(--text-on-glass-muted);
    cursor: pointer;
    border-bottom: 2px solid transparent;
    border-top-left-radius: 0.25rem;
    border-top-right-radius: 0.25rem;
    line-height: 1;
    transition: background 150ms ease-out, border-color 150ms ease-out, color 150ms ease-out;
  }
  :global(.rt-tab:hover) {
    background: color-mix(in oklab, var(--color-primary-500) 10%, transparent);
    color: var(--text-on-glass);
  }
  :global(.rt-tab-active) {
    color: var(--color-primary-500);
    border-bottom-color: var(--color-primary-500);
  }

  /* Panel buttons — large stacked icon-above-label.  #152: bumped
     from the original ~3.25rem-wide / 18px-icon / 11px-label
     proportions to give the ribbon more visual weight; the old
     sizing read as cramped at 1.0 zoom. */
  :global(.rt-btn) {
    display: inline-flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.25rem;
    min-width: 3.75rem;
    padding: 0.5rem 0.625rem;
    border-radius: 0.5rem;
    background: transparent;
    border: none;
    color: var(--text-on-glass);
    cursor: pointer;
    transition: background 150ms ease-out, color 150ms ease-out;
    position: relative;
  }
  /* Theme-derived translucent hover (#451) — matches the
     hover:bg-primary-500/10 accent the Tailwind chrome uses. */
  :global(.rt-btn:hover:not(:disabled)) {
    background: color-mix(in oklab, var(--color-primary-500) 10%, transparent);
  }
  :global(.rt-btn:disabled) {
    opacity: 0.4;
    cursor: not-allowed;
  }
  :global(.rt-btn-icon) {
    font-size: 1.375rem;
    line-height: 1;
    height: 1.5rem;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  :global(.rt-btn-label) {
    font-size: 0.75rem;
    line-height: 1;
    white-space: nowrap;
  }
  :global(.rt-btn-wide) {
    min-width: 7rem;
  }
  :global(.rt-btn.is-active) {
    background: rgb(from var(--color-primary-500) r g b / 0.15);
    color: var(--color-primary-500);
  }

  /* #152 — keep the toolbar's horizontal scrollbar visually
     unobtrusive.  WebKit (Tauri's macOS / Linux engines) needs
     `::-webkit-scrollbar` rules; Firefox / Edge respect
     `scrollbar-width`. */
  :global(.rt-tab-panel) {
    scrollbar-width: thin;
    scrollbar-color: var(--color-surface-300) transparent;
  }
  :global([data-mode='dark'] .rt-tab-panel) {
    scrollbar-color: var(--color-surface-700) transparent;
  }
  :global(.rt-tab-panel::-webkit-scrollbar) {
    height: 6px;
  }
  :global(.rt-tab-panel::-webkit-scrollbar-thumb) {
    background: var(--color-surface-300);
    border-radius: 3px;
  }
  :global([data-mode='dark'] .rt-tab-panel::-webkit-scrollbar-thumb) {
    background: var(--color-surface-700);
  }

  /* Vertical rule between sub-groups inside a panel. */
  :global(.rt-divider) {
    display: inline-block;
    width: 1px;
    height: 2.25rem;
    background: var(--color-surface-300);
    margin: 0 0.375rem;
    align-self: center;
  }
  :global([data-mode='dark'] .rt-divider) {
    background: var(--color-surface-600);
  }
  /* Tiptap editor chrome — keep the editing area clean and consistent
     with the rest of the app. */
  :global(.tiptap) {
    outline: none;
    min-height: 200px;
    padding: 0.75rem;
    font-size: 0.875rem;
    line-height: 1.625;
  }
  :global(.tiptap p.is-editor-empty:first-child::before) {
    content: attr(data-placeholder);
    float: left;
    pointer-events: none;
    height: 0;
    color: var(--text-on-glass-muted);
  }
  /* Basic table styling so it's visible in the editor. */
  :global(.tiptap table) {
    border-collapse: collapse;
    width: 100%;
    margin: 0.5rem 0;
  }
  :global(.tiptap th),
  :global(.tiptap td) {
    border: 1px solid var(--color-surface-300);
    padding: 0.375rem 0.625rem;
    text-align: left;
    min-width: 80px;
  }
  :global([data-mode='dark'] .tiptap th),
  :global([data-mode='dark'] .tiptap td) {
    border-color: var(--color-surface-700);
  }
  :global(.tiptap th) {
    background: var(--color-surface-100);
    font-weight: 600;
  }
  :global([data-mode='dark'] .tiptap th) {
    background: var(--color-surface-800);
  }
  :global(.tiptap img) {
    max-width: 100%;
    height: auto;
  }
  :global(.tiptap blockquote) {
    border-left: 3px solid var(--color-surface-300);
    padding-left: 0.75rem;
    margin: 0.5rem 0;
    color: var(--color-surface-600);
  }
  :global([data-mode='dark'] .tiptap blockquote) {
    border-left-color: var(--color-surface-700);
    color: var(--color-surface-400);
  }
  :global(.tiptap hr) {
    border: none;
    border-top: 1px solid var(--color-surface-300);
    margin: 1rem 0;
  }
  :global([data-mode='dark'] .tiptap hr) {
    border-top-color: var(--color-surface-700);
  }
  :global(.tiptap ul),
  :global(.tiptap ol) {
    padding-left: 1.5rem;
    margin: 0.25rem 0;
  }
  :global(.tiptap ul) { list-style-type: disc; }
  :global(.tiptap ol) { list-style-type: decimal; }
  :global(.tiptap a) { color: var(--color-primary-500); text-decoration: underline; }

  /* Image resize handle styling. Positioned at the bottom-right
     corner of the wrapper span inserted by our NodeView (see
     `addNodeView` on the Image extension). The handle stays
     invisible until the image or wrapper is hovered so the editor
     doesn't show UI chrome on every image on first render. */
  :global(.tiptap .ev-resizable-img) {
    line-height: 0;
  }
  :global(.tiptap .ev-resize-handle) {
    position: absolute;
    right: -4px;
    bottom: -4px;
    width: 12px;
    height: 12px;
    border: 2px solid var(--color-primary-500);
    background: var(--color-surface-50);
    border-radius: 2px;
    cursor: nwse-resize;
    opacity: 0;
    transition: opacity 120ms ease;
    touch-action: none;
  }
  :global(.tiptap .ev-resizable-img:hover .ev-resize-handle) {
    opacity: 1;
  }
</style>

{#if $editor}
<!-- Wrapper: `h-full flex flex-col min-h-0` lets the editor fill whatever
     vertical space its parent gives it (e.g. when the Compose modal is
     resized taller). The toolbar stays at natural height; the content
     area below claims the remaining space via `flex-1`. Works in a
     block parent too — `h-full` is simply ignored and the editor falls
     back to its intrinsic 200 px minimum. -->
<div class="h-full flex flex-col min-h-0">
  <!-- ── Ribbon: tab strip + active panel (#103) ───────────────────
       Two-row ribbon toolbar.  Top row holds the tab labels
       on the left, undo/redo + the embedder's send actions on the
       right.  Bottom row renders the active tab's panel — bigger
       icon-above-label buttons for a less flat, more discoverable
       look than the previous single-row layout. -->
  <div class="border-b border-surface-200 dark:border-surface-700 bg-surface-100 dark:bg-surface-800">
    <!-- Tab strip.  Two-column flex layout:
         - LEFT column: the tab labels themselves.  Wrapped in a
           horizontally-scrolling container with `min-w-0` so a
           narrow Compose window can grow the embedder's tab list
           (e.g. Compose's Attach / Meetings / Encryption tabs)
           without ever pushing the trailing send island off-screen.
         - RIGHT column: undo / redo + `actionsTrailing` (Save +
           Send + the encrypted-status chip embedders contribute).
           Pinned with `shrink-0` so it stays anchored even
           when the tab strip overflows — the user's primary
           commit action must always be reachable. -->
    <div class="flex items-stretch pt-0.5">
      <!-- Scrolling tab list.  `role="tablist"` moves here so the
           buttons stay a direct child of the labelled list per
           ARIA's relationship rules. -->
      <div
        class="flex items-stretch gap-0 px-2 overflow-x-auto min-w-0 flex-1 rt-tab-scroller"
        role="tablist"
      >
        <button
          type="button"
          role="tab"
          aria-selected={activeTab === 'format'}
          class="rt-tab"
          class:rt-tab-active={activeTab === 'format'}
          onclick={() => (activeTab = 'format')}
        >Format</button>
        <button
          type="button"
          role="tab"
          aria-selected={activeTab === 'insert'}
          class="rt-tab"
          class:rt-tab-active={activeTab === 'insert'}
          onclick={() => (activeTab = 'insert')}
        >Insert</button>
        <button
          type="button"
          role="tab"
          aria-selected={activeTab === 'layout'}
          class="rt-tab"
          class:rt-tab-active={activeTab === 'layout'}
          onclick={() => (activeTab = 'layout')}
        >Layout</button>
        {#each extraTabs as t (t.id)}
          <button
            type="button"
            role="tab"
            aria-selected={activeTab === t.id}
            class="rt-tab"
            class:rt-tab-active={activeTab === t.id}
            onclick={() => (activeTab = t.id)}
          >
            {#if t.iconName}<span class="mr-1 inline-flex"><Icon name={t.iconName} size={14} /></span>{:else if t.icon}<span class="mr-1">{t.icon}</span>{/if}{t.label}
          </button>
        {/each}
      </div>

      <!-- Pinned send island: undo / redo + the embedder's
           trailing actions (Save / Send).  `shrink-0` so it
           survives a narrow window; `border-l` plus a tiny inset
           separates it from the scrolling tabs visually. -->
      <div class="shrink-0 flex items-center gap-1 px-1 border-l border-surface-200 dark:border-surface-700">
        <button class="tb" title="Undo (Ctrl+Z)" aria-label="Undo" onclick={() => doUndo()}><Icon name="undo" size={18} /></button>
        <button class="tb" title="Redo (Ctrl+Y)" aria-label="Redo" onclick={() => doRedo()}><Icon name="redo" size={18} /></button>
        {#if actionsTrailing}
          {#if !actionsTrailingCompact}
            <span class="w-px h-5 bg-surface-300 dark:bg-surface-600 mx-1"></span>
          {/if}
          {@render actionsTrailing()}
        {/if}
      </div>
    </div>

    <!-- Tab panel — flex row of large stacked-icon buttons.  Each
         tab's content lives behind its own `{#if}` so swapping tabs
         doesn't carry hidden DOM.  Dividers split logical sub-
         groups within a panel for scannability.
         #152: switched from flex-wrap to flex-nowrap +
         overflow-x-auto so a narrow window scrolls the ribbon
         horizontally instead of wrapping it onto a confusing
         second row. -->
    <div class="rt-tab-panel flex flex-nowrap items-center gap-0.5 px-2 py-1.5 min-h-12 overflow-x-auto">
      {#if activeTab === 'format'}
        <!-- Font family — wider trigger, dropdown menu. -->
        <div class="inline-block">
          <button
            type="button"
            class="rt-btn rt-btn-wide"
            title="Font family"
            onclick={(e) => {
              if (!showFontPicker) fontPickerPos = anchorPopover(e)
              showFontPicker = !showFontPicker
            }}
          >
            <span class="rt-btn-icon" aria-hidden="true">𝐀</span>
            <span class="rt-btn-label">{currentFontLabel()}</span>
            <Icon name="caret-down" size={12} />
          </button>
          {#if showFontPicker}
            <div
              class="fixed z-60 mt-0 w-64 popover-opaque rounded-xl py-1 flex flex-col"
              style="left: {Math.min(fontPickerPos.x, layoutViewport().width - 272)}px; top: {fontPickerPos.y}px;"
              role="menu"
              tabindex="-1"
              onclick={(e) => e.stopPropagation()}
              onkeydown={(e) => { if (e.key === 'Escape') { showFontPicker = false; fontPickerQuery = '' } }}
            >
              <div class="px-2 pt-1 pb-2 border-b border-surface-200 dark:border-surface-700">
                <input
                  type="search"
                  class="input w-full text-sm px-2 py-1 rounded-lg"
                  placeholder="Search fonts ({systemFonts.length} system)"
                  bind:value={fontPickerQuery}
                />
              </div>
              <!-- Windowed scroll container — absolute-positions
                   only the rows currently in (or close to) the
                   viewport.  Total scroll height is reserved by
                   a single spacer div sized to total*ROW_H so
                   the scrollbar geometry matches an unwindowed
                   list. -->
              <div
                bind:this={fontScrollEl}
                class="max-h-72 overflow-y-auto"
                onscroll={(e) => (fontScrollY = (e.currentTarget as HTMLDivElement).scrollTop)}
              >
                {#if fontWindow.total === 0}
                  <p class="px-3 py-2 text-xs text-surface-500 italic">
                    No fonts match "{fontPickerQuery}".
                  </p>
                {:else}
                  <div style="position: relative; height: {fontWindow.total * FONT_ROW_H}px;">
                    {#each fontWindow.slice as f, i (f.label)}
                      <!-- Append `, sans-serif` to the preview's
                           font-family stack so the row paints in
                           a generic face *immediately* on mount;
                           when the platform finishes resolving
                           the requested family the glyphs swap
                           in.  Beats the previous "blank row
                           briefly visible after scroll" flash. -->
                      <button
                        type="button"
                        class="absolute left-0 right-0 text-left px-3 text-sm leading-tight hover:bg-surface-200 dark:hover:bg-surface-800 truncate"
                        style="top: {(fontWindow.start + i) * FONT_ROW_H}px; height: {FONT_ROW_H}px;{f.css ? ` font-family: ${f.css}, sans-serif;` : ''}"
                        onclick={() => { setFont(f.css); fontPickerQuery = '' }}
                      >{f.label}</button>
                    {/each}
                  </div>
                {/if}
              </div>
            </div>
          {/if}
        </div>

        <span class="rt-divider"></span>

        <!-- Text style -->
        <button class="rt-btn {active('bold')}" title="Bold (Ctrl+B)" onclick={() => $editor?.chain().focus().toggleBold().run()}>
          <span class="rt-btn-icon"><strong>B</strong></span>
          <span class="rt-btn-label">Bold</span>
        </button>
        <button class="rt-btn {active('italic')}" title="Italic (Ctrl+I)" onclick={() => $editor?.chain().focus().toggleItalic().run()}>
          <span class="rt-btn-icon"><em>I</em></span>
          <span class="rt-btn-label">Italic</span>
        </button>
        <button class="rt-btn {active('underline')}" title="Underline (Ctrl+U)" onclick={() => $editor?.chain().focus().toggleUnderline().run()}>
          <span class="rt-btn-icon"><u>U</u></span>
          <span class="rt-btn-label">Underline</span>
        </button>
        <button class="rt-btn {active('strike')}" title="Strikethrough" onclick={() => $editor?.chain().focus().toggleStrike().run()}>
          <span class="rt-btn-icon"><s>S</s></span>
          <span class="rt-btn-label">Strike</span>
        </button>

        <span class="rt-divider"></span>

        <!-- Colors -->
        <label class="rt-btn cursor-pointer" title="Text color">
          <span class="rt-btn-icon"><Icon name="text-color" size={20} /></span>
          <span class="rt-btn-label">Color</span>
          <input type="color" class="w-0 h-0 opacity-0 absolute" onchange={setColor} />
        </label>
        <label class="rt-btn cursor-pointer" title="Highlight color">
          <span class="rt-btn-icon"><Icon name="highlight-text" size={20} /></span>
          <span class="rt-btn-label">Highlight</span>
          <input type="color" value="#fde68a" class="w-0 h-0 opacity-0 absolute" onchange={setHighlight} />
        </label>

        <span class="rt-divider"></span>

        <!-- Clear formatting — strips marks AND collapses to plain
             paragraph (the standard "Clear all formatting" action). -->
        <button class="rt-btn" title="Clear all formatting" onclick={clearFormatting}>
          <span class="rt-btn-icon"><Icon name="clear" size={20} /></span>
          <span class="rt-btn-label">Clear</span>
        </button>
      {:else if activeTab === 'insert'}
        <!-- #317 — the click opens an inline popover, not
             `window.prompt`, which is disabled in Tauri 2 webviews
             and stole the editor's selection anyway. -->
        <div class="inline-block">
          <button
            class="rt-btn {active('link')}"
            title="Insert link"
            onclick={openLinkPopover}
          >
            <span class="rt-btn-icon"><Icon name="open-link" size={20} /></span>
            <span class="rt-btn-label">Link</span>
          </button>
          {#if showLinkPopover}
            <!-- Pointer + key handlers exist only to shield the popover
                 contents from the document-level click-outside-to-dismiss
                 handler; they're not user-actionable. role="dialog" +
                 aria-label gives the wrapper accurate ARIA semantics. -->
            <div
              role="dialog"
              aria-label="Insert link"
              tabindex="-1"
              class="fixed z-60 popover-opaque rounded-xl p-2 flex items-center gap-2"
              style="left: {Math.min(linkPopoverPos.x, layoutViewport().width - 320)}px; top: {linkPopoverPos.y}px; width: 304px;"
              onclick={(e) => e.stopPropagation()}
              onmousedown={(e) => e.stopPropagation()}
              onkeydown={(e) => e.stopPropagation()}
            >
              <input
                bind:this={linkInputEl}
                bind:value={linkUrlInput}
                type="url"
                placeholder="https://example.com"
                class="flex-1 min-w-0 px-2 py-1 text-sm rounded-lg border border-surface-300 dark:border-surface-600 bg-surface-50 dark:bg-surface-900"
                onkeydown={(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault()
                    commitLink()
                  } else if (e.key === 'Escape') {
                    e.preventDefault()
                    showLinkPopover = false
                  }
                }}
              />
              <button
                type="button"
                class="text-xs px-2 py-1 rounded-lg bg-primary-500 text-white hover:bg-primary-600"
                onclick={commitLink}
              >Apply</button>
              {#if linkPopoverHasExisting}
                <button
                  type="button"
                  class="text-xs px-2 py-1 rounded-lg text-error-500 hover:bg-primary-500/10"
                  title="Remove link"
                  onclick={removeLink}
                >Remove</button>
              {/if}
            </div>
          {/if}
        </div>
        <button class="rt-btn" title="Insert image from local file" onclick={() => addImageFromFile()}>
          <span class="rt-btn-icon"><Icon name="insert-image" size={20} /></span>
          <span class="rt-btn-label">Image</span>
        </button>
        <button
          class="rt-btn"
          title={onrequestncimage ? 'Insert image from Nextcloud' : 'Insert image from URL'}
          onclick={() => addImageFromNcOrUrl()}
        >
          <span class="rt-btn-icon"><Icon name={onrequestncimage ? 'cloud' : 'open-link'} size={20} /></span>
          <span class="rt-btn-label">{onrequestncimage ? 'NC image' : 'From URL'}</span>
        </button>

        <span class="rt-divider"></span>

        <!-- Table picker -->
        <div class="inline-block" bind:this={tablePickerAnchor}>
          <button
            class="rt-btn"
            title="Insert table at cursor"
            onclick={(e) => {
              if (!showTablePicker) tablePickerPos = anchorPopover(e)
              openTablePicker()
            }}
          >
            <span class="rt-btn-icon"><Icon name="table" size={20} /></span>
            <span class="rt-btn-label">Table</span>
          </button>
          {#if showTablePicker}
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <div
              class="fixed z-60 p-2 popover-opaque rounded-xl"
              style="left: {Math.min(tablePickerPos.x, layoutViewport().width - 200)}px; top: {tablePickerPos.y}px;"
              onmouseleave={() => { tableHoverRows = 0; tableHoverCols = 0 }}
              onkeydown={(e) => e.key === 'Escape' && (showTablePicker = false)}
            >
              <div class="text-xs text-surface-500 mb-1 text-center">
                {tableHoverRows > 0 ? `${tableHoverRows} × ${tableHoverCols}` : 'Pick size'}
              </div>
              <div class="grid gap-0.5" style="grid-template-columns: repeat({TABLE_GRID}, 1fr)">
                {#each { length: TABLE_GRID } as _, r}
                  {#each { length: TABLE_GRID } as _, c}
                    <button
                      type="button"
                      aria-label="{r + 1} × {c + 1} table"
                      class="w-4 h-4 border rounded-sm cursor-pointer transition-colors
                        {r < tableHoverRows && c < tableHoverCols
                          ? 'bg-primary-500/40 border-primary-500'
                          : 'bg-surface-100 dark:bg-surface-700 border-surface-300 dark:border-surface-600'}"
                      onmouseenter={() => { tableHoverRows = r + 1; tableHoverCols = c + 1 }}
                      onclick={() => insertTable(r + 1, c + 1)}
                      tabindex="-1"
                    ></button>
                  {/each}
                {/each}
              </div>
            </div>
          {/if}
        </div>

        <button class="rt-btn" title="Horizontal rule" onclick={() => cmd().setHorizontalRule().run()}>
          <span class="rt-btn-icon">―</span>
          <span class="rt-btn-label">HR</span>
        </button>

        <span class="rt-divider"></span>

        <!-- Emoji picker — popup grid of curated emojis (#103
             follow-up).  Click outside or pick an emoji to dismiss. -->
        <div class="inline-block">
          <button
            class="rt-btn"
            title="Insert emoji"
            onclick={(e) => {
              if (!showEmojiPicker) emojiPickerPos = anchorPopover(e)
              showEmojiPicker = !showEmojiPicker
            }}
          >
            <span class="rt-btn-icon"><Icon name="emoji" size={20} /></span>
            <span class="rt-btn-label">Emoji</span>
          </button>
          {#if showEmojiPicker}
            <div
              class="fixed z-60"
              style="left: {Math.min(emojiPickerPos.x, layoutViewport().width - 360)}px; top: {emojiPickerPos.y}px;"
              role="menu"
              tabindex="-1"
              onclick={(e) => e.stopPropagation()}
              onkeydown={(e) => e.key === 'Escape' && (showEmojiPicker = false)}
            >
              <EmojiPicker allowClear={false} onpick={(e) => insertEmoji(e)} />
            </div>
          {/if}
        </div>

        <span class="rt-divider"></span>

        <!-- Table editing tools — visible always, but disabled when
             the cursor isn't inside a table.  Sits in the Insert
             tab next to the table-creation picker so the user
             reaches for one ribbon section regardless of whether
             they're creating or editing.  Background-colour input
             writes to a custom `backgroundColor` attribute on the
             cell (TableCell extension above) which renders as
             inline `style="background-color: …"` for cross-client
             email compatibility. -->
        {@const tblOn = tableActive()}
        <button class="rt-btn" title="Add row above" disabled={!tblOn} onclick={tblAddRowAbove}>
          <span class="rt-btn-icon"><Icon name="insert-row-above" size={20} /></span>
          <span class="rt-btn-label">Row above</span>
        </button>
        <button class="rt-btn" title="Add row below" disabled={!tblOn} onclick={tblAddRowBelow}>
          <span class="rt-btn-icon"><Icon name="insert-row-below" size={20} /></span>
          <span class="rt-btn-label">Row below</span>
        </button>
        <button class="rt-btn" title="Add column left" disabled={!tblOn} onclick={tblAddColLeft}>
          <span class="rt-btn-icon"><Icon name="insert-column-left" size={20} /></span>
          <span class="rt-btn-label">Col left</span>
        </button>
        <button class="rt-btn" title="Add column right" disabled={!tblOn} onclick={tblAddColRight}>
          <span class="rt-btn-icon"><Icon name="insert-column-right" size={20} /></span>
          <span class="rt-btn-label">Col right</span>
        </button>
        <button class="rt-btn" title="Delete current row" disabled={!tblOn} onclick={tblDeleteRow}>
          <span class="rt-btn-icon"><Icon name="delete-row" size={20} /></span>
          <span class="rt-btn-label">Del row</span>
        </button>
        <button class="rt-btn" title="Delete current column" disabled={!tblOn} onclick={tblDeleteCol}>
          <span class="rt-btn-icon"><Icon name="delete-column" size={20} /></span>
          <span class="rt-btn-label">Del col</span>
        </button>
        <label class="rt-btn cursor-pointer" title="Cell background colour" class:opacity-50={!tblOn}>
          <span class="rt-btn-icon"><Icon name="highlight-text" size={20} /></span>
          <span class="rt-btn-label">Cell colour</span>
          <input type="color" class="w-0 h-0 opacity-0 absolute" disabled={!tblOn} onchange={tblSetCellColor} />
        </label>
        <button class="rt-btn" title="Clear cell background colour" disabled={!tblOn} onclick={tblClearCellColor}>
          <span class="rt-btn-icon"><Icon name="clear" size={20} /></span>
          <span class="rt-btn-label">Clear fill</span>
        </button>
        <button class="rt-btn" title="Delete entire table" disabled={!tblOn} onclick={tblDelete}>
          <span class="rt-btn-icon"><Icon name="trash" size={20} /></span>
          <span class="rt-btn-label">Del table</span>
        </button>
      {:else if activeTab === 'layout'}
        <!-- Headings -->
        <button class="rt-btn {active('heading', { level: 1 })}" title="Heading 1" onclick={() => toggleHeading(1)}>
          <span class="rt-btn-icon">H₁</span>
          <span class="rt-btn-label">Heading 1</span>
        </button>
        <button class="rt-btn {active('heading', { level: 2 })}" title="Heading 2" onclick={() => toggleHeading(2)}>
          <span class="rt-btn-icon">H₂</span>
          <span class="rt-btn-label">Heading 2</span>
        </button>
        <button class="rt-btn {active('heading', { level: 3 })}" title="Heading 3" onclick={() => toggleHeading(3)}>
          <span class="rt-btn-icon">H₃</span>
          <span class="rt-btn-label">Heading 3</span>
        </button>

        <span class="rt-divider"></span>

        <!-- Lists -->
        <button class="rt-btn {active('bulletList')}" title="Bullet list" onclick={() => $editor?.chain().focus().toggleBulletList().run()}>
          <span class="rt-btn-icon"><Icon name="bullet-list" size={20} /></span>
          <span class="rt-btn-label">Bullets</span>
        </button>
        <button class="rt-btn {active('orderedList')}" title="Numbered list" onclick={() => $editor?.chain().focus().toggleOrderedList().run()}>
          <span class="rt-btn-icon"><Icon name="numbered-list" size={20} /></span>
          <span class="rt-btn-label">Numbered</span>
        </button>

        <span class="rt-divider"></span>

        <!-- Alignment -->
        <button class="rt-btn {activeAttrs({ textAlign: 'left' })}" title="Align left" onclick={() => $editor?.chain().focus().setTextAlign('left').run()}>
          <span class="rt-btn-icon"><Icon name="align-left" size={20} /></span>
          <span class="rt-btn-label">Left</span>
        </button>
        <button class="rt-btn {activeAttrs({ textAlign: 'center' })}" title="Align center" onclick={() => $editor?.chain().focus().setTextAlign('center').run()}>
          <span class="rt-btn-icon"><Icon name="align-center" size={20} /></span>
          <span class="rt-btn-label">Center</span>
        </button>
        <button class="rt-btn {activeAttrs({ textAlign: 'right' })}" title="Align right" onclick={() => $editor?.chain().focus().setTextAlign('right').run()}>
          <span class="rt-btn-icon"><Icon name="align-right" size={20} /></span>
          <span class="rt-btn-label">Right</span>
        </button>
        <button class="rt-btn {activeAttrs({ textAlign: 'justify' })}" title="Justify" onclick={() => $editor?.chain().focus().setTextAlign('justify').run()}>
          <span class="rt-btn-icon"><Icon name="justify" size={20} /></span>
          <span class="rt-btn-label">Justify</span>
        </button>

        <span class="rt-divider"></span>

        <button class="rt-btn {active('blockquote')}" title="Blockquote" onclick={() => cmd().toggleBlockquote().run()}>
          <span class="rt-btn-icon"><Icon name="quote" size={20} /></span>
          <span class="rt-btn-label">Quote</span>
        </button>
      {:else}
        {#each extraTabs as t (t.id)}
          {#if activeTab === t.id}
            {@render t.content()}
          {/if}
        {/each}
      {/if}
    </div>
  </div>

  <!-- Editor area. `flex-1 min-h-0` lets it shrink/grow with the
       wrapper's available height; `overflow-y-auto` scrolls internally
       once the content exceeds what fits. When the Compose modal is
       resized taller, this is what absorbs the new space. -->
  <div class="flex-1 min-h-0 border border-surface-200 dark:border-surface-700 rounded-b-lg glass-writing-surface overflow-y-auto">
    <EditorContent editor={$editor} />
  </div>
</div>

<!-- ── `@` contact picker popup ────────────────────────────
     `position: fixed` anchored to the trigger char's bounding rect
     (computed in `anchorBelow`). z-60 so we sit on top of the
     Compose modal's z-50 backdrop. The whole panel only renders
     while the suggestion plugin says it should be visible — the
     popup never lingers in the DOM tree when no `@` is active. -->
{#if mentionPicker.visible}
  <ul
    class="fixed z-60 max-h-72 min-w-72 overflow-y-auto popover-opaque rounded-xl py-1 text-sm"
    style="left: {mentionPicker.position.left}px; top: {mentionPicker.position.top}px;"
    role="listbox"
  >
    {#if mentionPicker.items.length === 0}
      <li class="px-3 py-2 text-xs text-surface-500">No matching contacts</li>
    {:else}
      {#each mentionPicker.items as c, i (c.id)}
        <li
          role="option"
          aria-selected={i === mentionPicker.selectedIndex}
          class="flex items-center gap-3 px-3 py-1.5 cursor-pointer
                 {i === mentionPicker.selectedIndex
                   ? 'bg-primary-500/15'
                   : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
          onmousedown={(e) => {
            // mousedown so we commit before the editor sees a
            // focus-loss and tears the popup down.
            e.preventDefault()
            mentionPicker.command?.(c)
          }}
        >
          {#if c.photoUrl}
            <img src={c.photoUrl} alt="" loading="lazy"
                 class="w-7 h-7 rounded-full object-cover shrink-0" />
          {:else}
            <div class="w-7 h-7 rounded-full bg-surface-300 dark:bg-surface-700
                        flex items-center justify-center text-[10px] font-semibold shrink-0">
              {c.label.trim().charAt(0).toUpperCase() || '?'}
            </div>
          {/if}
          <div class="flex-1 min-w-0">
            <p class="font-medium truncate">{c.label}</p>
            <p class="text-xs text-surface-500 truncate">
              {c.email}{#if c.hint} · {c.hint}{/if}
            </p>
          </div>
        </li>
        <!-- Divider after the last `participants` row, but only when
             there are also `others` below it. Pure visual separator
             — not selectable, not in the keyboard cycle. -->
        {#if i === mentionPicker.participantsCount - 1
              && i < mentionPicker.items.length - 1}
          <li
            aria-hidden="true"
            class="my-1 mx-2 border-t border-surface-200 dark:border-surface-700"
          ></li>
        {/if}
      {/each}
    {/if}
  </ul>
{/if}

<!-- ── `/` attachment picker popup ─────────────────────────
     Same shape as the contact picker, narrower because rows are
     just a filename + a paperclip glyph. Only attachments with a
     `content_id` show up — everything else has nothing to link to. -->
{#if attachmentPickerEverShown}
  <!-- Keep the DOM mounted across opens — `display: none` when
       hidden so the first open mints the rows once and every
       subsequent `/` is just a CSS flag flip plus a coordinate
       update.  Mounting only flips on first reach to avoid
       allocating the popup at all in the common case where the
       user never types `/`. -->
  <ul
    class="fixed z-60 max-h-72 min-w-64 overflow-y-auto popover-opaque rounded-xl py-1 text-sm"
    style="left: {attachmentPicker.position.left}px; top: {attachmentPicker.position.top}px; display: {attachmentPicker.visible ? 'block' : 'none'};"
    role="listbox"
  >
    {#if attachmentPicker.items.length === 0}
      <li class="px-3 py-2 text-xs text-surface-500">No attachments to reference</li>
    {:else}
      {#each attachmentPicker.items as a, i (a.content_id)}
        {@const thumb = thumbUrlSync({
          // Key by content_id (stable string) — not by the
          // bytes ref, which gets wrapped in a $state proxy
          // by the time it reaches us and would miss the
          // WeakMap-by-ref cache prewarm() populated.
          cacheKey: a.content_id,
          bytes: a.data ?? null,
          contentType: a.content_type ?? null,
          filename: a.filename,
        })}
        <li
          role="option"
          aria-selected={i === attachmentPicker.selectedIndex}
          class="flex items-center gap-2 px-3 py-1.5 cursor-pointer
                 {i === attachmentPicker.selectedIndex
                   ? 'bg-primary-500/15'
                   : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
          onmousedown={(e) => {
            e.preventDefault()
            attachmentPicker.command?.(a)
          }}
        >
          {#if thumb}
            <img
              src={thumb}
              alt=""
              class="w-7 h-7 object-cover rounded-sm shrink-0 bg-surface-200 dark:bg-surface-800"
            />
          {:else}
            <FileTypeIcon contentType={a.content_type ?? null} filename={a.filename} class="w-7 h-7" />
          {/if}
          <span class="truncate">{a.filename}</span>
        </li>
      {/each}
    {/if}
  </ul>
{/if}
{/if}
