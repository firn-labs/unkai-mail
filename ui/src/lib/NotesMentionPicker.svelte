<script lang="ts" module>
  /**
   * Row shapes the Notes mention picker (#260) accepts.  Discriminated
   * union by `kind` so the template can render contact and mail rows
   * with the same component while keeping each type's data narrow.
   *
   * The picker itself is dumb — it shows what it's given and emits a
   * pick event with the chosen item.  The parent
   * (`NotesMarkdownEditor`) owns the data sources and turns picks
   * into editor inserts.
   */
  export interface ContactItem {
    kind: 'contact'
    /** Stable key — typically the email address. */
    id: string
    label: string
    email: string
    photoUrl?: string | null
    hint?: string | null
  }
  export interface MailItem {
    kind: 'mail'
    /** Stable key — `${accountId}:${folder}:${uid}`. */
    id: string
    accountId: string
    folder: string
    uid: number
    subject: string
    from: string
    /** ISO date.  Formatted client-side for the row. */
    date: string
    snippet?: string | null
    isRead: boolean
  }
  export type PickerItem = ContactItem | MailItem
</script>

<script lang="ts">
  /**
   * NotesMentionPicker — popup for `@` (contact) and `/mail` (mail)
   * triggers in the Notes markdown editor (#260).
   *
   * Rendered into the body via `position: fixed` so the popup escapes
   * any scroll container the editor lives in.  The parent owns the
   * `(left, top)` anchor; we just paint there and keep ourselves
   * inside the viewport.
   *
   * The popup is keyboard-driven from the editor:
   *   - Up / Down cycle `selectedIndex`
   *   - Enter / Tab commit the highlighted row
   *   - Escape closes
   *
   * The editor wraps a `<div>` around CodeMirror's DOM, intercepts
   * those keys *before* CM6 sees them, and calls `selectPrev` /
   * `selectNext` / `pickSelected()` on this component (exposed via
   * bind:this).  Mousedown on a row commits immediately — we use
   * mousedown rather than click because the editor would otherwise
   * lose focus first and `pickSelected` would fire against stale
   * state.
   */
  import { onMount, tick } from 'svelte'

  interface Props {
    items: PickerItem[]
    /** Whether the popup should render at all.  Parent flips this
     *  based on the active mention context. */
    visible: boolean
    /** Whether the data source is still resolving — drives the
     *  empty-state copy so a slow `search_emails` round-trip doesn't
     *  flash "No matches" before results arrive. */
    loading?: boolean
    /** Viewport-relative anchor of the trigger character's *bottom*.
     *  The popup paints below this point, flipping above when there
     *  isn't room. */
    anchor: { left: number; top: number; bottom: number }
    onpick: (item: PickerItem) => void
    onclose: () => void
  }
  let {
    items,
    visible,
    loading = false,
    anchor,
    onpick,
    onclose,
  }: Props = $props()

  let selectedIndex = $state(0)
  let listEl: HTMLUListElement | undefined = $state()

  // Resolved screen position — `anchor.bottom` by default, flipped
  // above when the popup would clip the bottom of the viewport.
  // Recomputed reactively on every `anchor` / `items` change.
  const POPUP_MAX_H = 288 // 18rem == max-h-72; matches the class below.
  const POPUP_MIN_W = 320 // 20rem == min-w-80
  let resolved = $derived.by(() => {
    if (!visible) return { left: 0, top: 0 }
    // Default below the trigger char, nudged a hair so the row's
    // baseline aligns with the click target.
    let top = anchor.bottom + 4
    let left = anchor.left
    const vh = typeof window !== 'undefined' ? window.innerHeight : 0
    const vw = typeof window !== 'undefined' ? window.innerWidth : 0
    if (vh && top + POPUP_MAX_H > vh && anchor.top - POPUP_MAX_H - 4 >= 0) {
      // Not enough room below — flip above.
      top = anchor.top - POPUP_MAX_H - 4
    }
    if (vw && left + POPUP_MIN_W > vw) {
      left = Math.max(8, vw - POPUP_MIN_W - 8)
    }
    return { left, top }
  })

  // Reset the highlight whenever the item list changes shape — a
  // re-query landing with fewer rows than the previous selection
  // would otherwise leave us pointing past the end.
  $effect(() => {
    if (selectedIndex >= items.length) selectedIndex = 0
  })

  /** Keep the highlighted row in view as the user arrows through
   *  a long list.  Called from inside the keyboard handlers below. */
  async function ensureVisible(): Promise<void> {
    await tick()
    const ul = listEl
    if (!ul) return
    const row = ul.querySelector<HTMLElement>(
      `li[data-row-index="${selectedIndex}"]`,
    )
    row?.scrollIntoView({ block: 'nearest' })
  }

  /** Parent-callable: move highlight down.  No-op if list is empty. */
  export function selectNext(): void {
    if (items.length === 0) return
    selectedIndex = (selectedIndex + 1) % items.length
    void ensureVisible()
  }
  /** Parent-callable: move highlight up.  No-op if list is empty. */
  export function selectPrev(): void {
    if (items.length === 0) return
    selectedIndex = (selectedIndex - 1 + items.length) % items.length
    void ensureVisible()
  }
  /** Parent-callable: commit the highlighted row, if any. */
  export function pickSelected(): boolean {
    const item = items[selectedIndex]
    if (!item) return false
    onpick(item)
    return true
  }

  /** Cheap relative-time label for mail rows.  Same shape as the
   *  MailList row date — minutes / hours for today, weekday for
   *  this week, locale date string otherwise.  Kept inline rather
   *  than imported from MailList because that component carries
   *  multi-row state we don't need. */
  function formatMailDate(iso: string): string {
    const d = new Date(iso)
    if (Number.isNaN(d.getTime())) return ''
    const now = new Date()
    const sameDay =
      d.getFullYear() === now.getFullYear() &&
      d.getMonth() === now.getMonth() &&
      d.getDate() === now.getDate()
    if (sameDay) {
      return d.toLocaleTimeString(undefined, {
        hour: '2-digit',
        minute: '2-digit',
      })
    }
    const diffDays = (now.getTime() - d.getTime()) / 86_400_000
    if (diffDays < 7) {
      return d.toLocaleDateString(undefined, { weekday: 'short' })
    }
    return d.toLocaleDateString(undefined, {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
    })
  }

  // Close the popup when the user clicks outside of it.  Registered
  // only while visible so a closed popup costs nothing.  Uses
  // `mousedown` for the same reason the row commit handler does —
  // it lands before focus changes.
  onMount(() => {
    function onDocMousedown(e: MouseEvent): void {
      if (!visible) return
      const t = e.target as Node | null
      if (listEl && t && listEl.contains(t)) return
      onclose()
    }
    document.addEventListener('mousedown', onDocMousedown)
    return () => document.removeEventListener('mousedown', onDocMousedown)
  })
</script>

{#if visible}
  <!-- Fixed-positioned popup so it escapes the Notes editor's
       scroll container.  z-60 sits above the modal backdrop the
       Notes view would use for any future dialog. -->
  <ul
    bind:this={listEl}
    class="fixed z-60 max-h-72 min-w-80 overflow-y-auto rounded-xl glass-float py-1 text-sm"
    style="left: {resolved.left}px; top: {resolved.top}px;"
    role="listbox"
  >
    {#if loading && items.length === 0}
      <li class="px-3 py-2 text-xs text-surface-500 italic">Searching…</li>
    {:else if items.length === 0}
      <li class="px-3 py-2 text-xs text-surface-500">No matches</li>
    {:else}
      {#each items as item, i (item.id)}
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <li
          data-row-index={i}
          role="option"
          aria-selected={i === selectedIndex}
          class="flex items-center gap-3 px-3 py-1.5 cursor-pointer
                 {i === selectedIndex
                   ? 'bg-primary-500/15'
                   : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
          onmousedown={(e) => {
            // mousedown so we commit before the editor sees a
            // focus-loss and tears the popup down (#260, same
            // gotcha that bit the Compose mention picker).
            e.preventDefault()
            selectedIndex = i
            onpick(item)
          }}
        >
          {#if item.kind === 'contact'}
            {#if item.photoUrl}
              <img
                src={item.photoUrl}
                alt=""
                loading="lazy"
                class="w-7 h-7 rounded-full object-cover shrink-0"
              />
            {:else}
              <div
                class="w-7 h-7 rounded-full bg-surface-300 dark:bg-surface-700
                       flex items-center justify-center text-[10px] font-semibold shrink-0"
              >
                {item.label.trim().charAt(0).toUpperCase() || '?'}
              </div>
            {/if}
            <div class="flex-1 min-w-0">
              <p class="font-medium truncate">{item.label}</p>
              <p class="text-xs text-surface-500 truncate">
                {item.email}{#if item.hint} · {item.hint}{/if}
              </p>
            </div>
          {:else}
            <!-- Mail row: subject (top) + sender · date (bottom).  Bold
                 the subject only when the message is unread so the
                 picker echoes the MailList's read/unread emphasis. -->
            <div
              class="w-7 h-7 rounded-lg bg-primary-500/10 text-primary-600
                     dark:text-primary-400 flex items-center justify-center shrink-0"
              aria-hidden="true"
            >
              ✉
            </div>
            <div class="flex-1 min-w-0">
              <p class="truncate {item.isRead ? 'font-normal' : 'font-semibold'}">
                {item.subject || '(no subject)'}
              </p>
              <p class="text-xs text-surface-500 truncate">
                {item.from} · {formatMailDate(item.date)}
              </p>
            </div>
          {/if}
        </li>
      {/each}
    {/if}
  </ul>
{/if}
