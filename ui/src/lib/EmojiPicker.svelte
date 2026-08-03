<script lang="ts">
  /**
   * Thin wrapper around the `emoji-picker-element` web component
   * (#176).  We import its definition once for side-effects to
   * register the `<emoji-picker>` custom element, then mount it
   * inside our own card surface so the popover keeps the same
   * shape, theming, and prop contract every existing call site
   * (`AccountSettings`, `ContactsView`, `RichTextEditor`) was
   * built against.  All the search / categories / skin-tone /
   * recents UX comes from the underlying element — none of it
   * is our code to maintain anymore.
   *
   * Apache-2.0, zero runtime deps, framework-agnostic — see
   * https://github.com/nolanlawson/emoji-picker-element.
   */

  import 'emoji-picker-element'
  import { onDestroy } from 'svelte'

  interface Props {
    /** Currently chosen emoji.  Not visually surfaced inside the
     *  underlying picker (it has its own recent / search UI), but
     *  callers still pass it for symmetry; we use it only to seed
     *  the "Clear" button's enabled state. */
    value?: string | null
    /** Width of the popover in Tailwind units (defaults to w-80 = 20rem).
     *  Forwarded to a wrapping div; the underlying element auto-fills it. */
    widthClass?: string
    /** Whether to show a "no emoji" / clear option above the picker.
     *  The web component itself has no clear button — we render one
     *  next to its search bar when the host opts in. */
    allowClear?: boolean
    onpick: (emoji: string | null) => void
  }
  let {
    value = null,
    widthClass = 'w-80',
    allowClear = true,
    onpick,
  }: Props = $props()

  /** The emoji-click event the web component emits.  Unicode is
   *  the rendered string with the chosen skin tone applied — we
   *  pass it straight to onpick. */
  type EmojiClickEvent = CustomEvent<{ unicode?: string; emoji?: { unicode: string } }>

  let pickerEl = $state<HTMLElement | null>(null)

  $effect(() => {
    const el = pickerEl
    if (!el) return
    const handler = (e: Event) => {
      const detail = (e as EmojiClickEvent).detail
      const unicode = detail?.unicode ?? detail?.emoji?.unicode ?? null
      if (unicode) onpick(unicode)
    }
    el.addEventListener('emoji-click', handler)
    return () => el.removeEventListener('emoji-click', handler)
  })

  // Theme follow-through: emoji-picker-element honours its
  // `class="dark"` / `class="light"` attribute.  Most of the app
  // toggles `dark` on `<html>`, so we mirror that into the
  // picker's class on mount + when the system theme flips.
  let isDark = $state(false)
  $effect(() => {
    const sync = () => {
      isDark = document.documentElement.classList.contains('dark')
    }
    sync()
    const obs = new MutationObserver(sync)
    obs.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['class'],
    })
    return () => obs.disconnect()
  })

  onDestroy(() => {
    pickerEl = null
  })
</script>

<!-- Deliberately opaque, not .glass-float: the picker renders inside
     glass modals (Sidebar's icon picker, Compose via RichTextEditor),
     and stacking a second backdrop-filter layer there is off-limits. -->
<div
  class="{widthClass} bg-surface-50 dark:bg-surface-900 border border-surface-300 dark:border-surface-600 rounded-xl shadow-lg flex flex-col"
>
  {#if allowClear}
    <div class="px-2 pt-2 pb-1 border-b border-surface-200 dark:border-surface-700">
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 w-full text-xs"
        onclick={() => onpick(null)}
        disabled={value === null}
      >
        ∅ No emoji
      </button>
    </div>
  {/if}
  <emoji-picker
    bind:this={pickerEl}
    class={isDark ? 'dark' : 'light'}
  ></emoji-picker>
</div>

<style>
  /* Strip the underlying element's own card chrome so it slots
     cleanly inside ours (we provide the rounded border + shadow
     above).  The web component exposes its theming via CSS
     custom properties — keep this list in sync with Skeleton's
     surface tokens so the picker tracks the active theme. */
  /* Skeleton v3 exposes theme tokens as full color values (oklch /
     hex), not rgb-triplets, so we forward them straight to the
     web component's CSS-variable surface — no wrapping needed. */
  emoji-picker {
    width: 100%;
    --background: transparent;
    --border-size: 0;
    --button-active-background: color-mix(in oklab, var(--color-primary-500) 15%, transparent);
    --button-hover-background: var(--color-surface-200);
    --emoji-padding: 0.25rem;
    --indicator-color: var(--color-primary-500);
    --input-border-color: var(--color-surface-300);
    --input-border-radius: 0.5rem;
    --input-padding: 0.375rem 0.5rem;
    --num-columns: 8;
    --outline-color: var(--color-primary-500);
  }
  :global(.dark) emoji-picker {
    --button-hover-background: var(--color-surface-800);
    --input-border-color: var(--color-surface-600);
  }
</style>
