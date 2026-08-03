<script lang="ts">
  /**
   * SearchInput — the canonical search-field shape used across the
   * app.  Encapsulates the magnifier-icon left adornment, the
   * Skeleton `.input` field, and the clear-X right adornment so
   * every "Search …" surface (mail, contacts, notes, shares, …)
   * speaks the same visual language without each call site
   * re-inlining the same `<div class="relative">…<input…>…<button>X</button></div>`
   * three times.
   *
   * # Why a component rather than a class string
   *
   * The pattern is structural, not purely cosmetic — the magnifier
   * lives in an absolutely-positioned span, the clear-X conditionally
   * mounts when the field has a value, and the input itself reserves
   * left + right padding to clear both adornments.  A class string
   * would only solve the *input's* styling, not the structural
   * sibling elements, and every caller would still have to re-paste
   * the absolute positioning and the conditional clear-button.  The
   * component pattern packages the three parts as one unit.
   *
   * # Adornment slot
   *
   * Callers that need a popover anchored to the input's bounding box
   * can pass children — they render *inside* the wrapper's `relative`
   * container so `absolute top-full left-0 right-0` resolves against
   * the input, not the page.
   */

  import type { Snippet } from 'svelte'
  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'

  interface Props {
    /** Bindable query string. */
    value: string
    placeholder: string
    /** Accessible label — falls back to `placeholder` when omitted. */
    ariaLabel?: string
    /** Optional Tailwind class on the outer wrapper — defaults to
     *  `w-full` (fill parent).  Override to e.g. `max-w-md mx-auto`
     *  for the centered header pattern in SharesView. */
    class?: string
    /** Optional bindable handle on the underlying `<input>` so
     *  callers can `focus()` / `select()` from outside. */
    inputEl?: HTMLInputElement | null
    /** Pass-through event hooks for callers that need to react to
     *  keystrokes immediately (debounce, Esc-clear, Enter-commit).
     *  Keeping them optional means simple "Search …" surfaces can
     *  rely on `bind:value` alone. */
    oninput?: (e: Event) => void
    onkeydown?: (e: KeyboardEvent) => void
    onfocus?: () => void
    onblur?: () => void
    /** Adornment slot rendered inside the relative wrapper. */
    children?: Snippet
  }

  let {
    value = $bindable(''),
    placeholder,
    ariaLabel,
    class: cls = 'w-full',
    inputEl = $bindable(null),
    oninput,
    onkeydown,
    onfocus,
    onblur,
    children,
  }: Props = $props()
</script>

<div class="relative {cls}">
  <span
    class="absolute left-2 top-1/2 -translate-y-1/2 text-surface-400 pointer-events-none flex items-center"
    aria-hidden="true"
  >
    <Icon name="search" size={14} />
  </span>
  <input
    bind:this={inputEl}
    bind:value
    type="text"
    class="input w-full pl-7 pr-8 py-1.5 text-sm rounded-lg"
    {placeholder}
    aria-label={ariaLabel ?? placeholder}
    {oninput}
    {onkeydown}
    {onfocus}
    {onblur}
  />
  {#if value}
    <button
      type="button"
      class="absolute right-2 top-1/2 -translate-y-1/2 text-surface-500 hover:text-surface-700 dark:hover:text-surface-200 text-xs"
      onclick={() => {
        value = ''
        inputEl?.focus()
      }}
      title={m.search_input_clear()}
      aria-label={m.search_input_clear()}
    >
      &#x2715;
    </button>
  {/if}
  {@render children?.()}
</div>
