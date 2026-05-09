<script lang="ts">
  /**
   * TimeField — slot-list time picker (#126).
   *
   * Replaces the native `<input type="time">` for consistency
   * with `DateField` and to give users a familiar
   * 15-minute slot dropdown.  The text input
   * itself stays editable so power users can type "08:45"
   * directly without scrolling the list — clicking or focusing
   * just additionally surfaces the slot picker.  Slots run
   * 00:00 → 23:45 in 15-minute increments and the dropdown
   * auto-scrolls to the currently-selected slot on open.
   *
   * Value is `HH:MM` so it round-trips through the existing
   * `fromLocalSplit` helper in EventEditor unchanged.
   */

  import { onMount, tick } from 'svelte'

  let {
    value = $bindable(''),
    id,
    ariaLabel,
  }: {
    value?: string
    id?: string
    ariaLabel?: string
  } = $props()

  let open = $state(false)
  let anchor: HTMLDivElement | undefined = $state()
  let listEl: HTMLUListElement | undefined = $state()
  // Stable id for the combobox / listbox `aria-controls` link.
  // A counter would also work; uniqueness is what matters.
  const listId = `timefield-list-${crypto.randomUUID()}`

  /** Pre-computed list of selectable times.  5-min increments
   *  (#236) so a user can pick odd off-the-hour starts like
   *  09:25 from the dropdown without dropping into the typed
   *  fallback.  The list is ~12× longer than the old 15-min
   *  set (288 entries) — still a single-frame render in the
   *  current listbox, no scroll-perf hit. */
  const slots = (() => {
    const out: string[] = []
    for (let h = 0; h < 24; h++) {
      for (let m = 0; m < 60; m += 5) {
        out.push(`${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}`)
      }
    }
    return out
  })()

  function pick(slot: string) {
    value = slot
    open = false
  }

  function onInputKey(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      open = false
    } else if (e.key === 'Enter') {
      open = false
    } else if (e.key === 'ArrowDown' && !open) {
      open = true
    }
  }

  // Auto-scroll the list to the selected slot when the
  // popover opens, so the user lands on something familiar
  // instead of always at 00:00.
  $effect(() => {
    if (!open) return
    void tick().then(() => {
      const li = listEl?.querySelector(`[data-slot="${value}"]`)
      if (li instanceof HTMLElement) {
        li.scrollIntoView({ block: 'center' })
      }
    })
  })

  // Outside-click closes.
  onMount(() => {
    function onClick(e: MouseEvent) {
      if (!open || !anchor) return
      if (!anchor.contains(e.target as Node)) open = false
    }
    document.addEventListener('mousedown', onClick)
    return () => document.removeEventListener('mousedown', onClick)
  })
</script>

<div class="relative" bind:this={anchor}>
  <input
    {id}
    type="text"
    inputmode="numeric"
    pattern="[0-9]{'{1,2}'}:[0-9]{'{2}'}"
    placeholder="HH:MM"
    aria-label={ariaLabel}
    aria-haspopup="listbox"
    role="combobox"
    aria-expanded={open}
    aria-controls={listId}
    autocomplete="off"
    class="input w-full px-3 py-2 text-sm rounded-md pr-9"
    bind:value
    onfocus={() => (open = true)}
    onclick={() => (open = true)}
    onkeydown={onInputKey}
  />
  <!-- Trailing clock icon — purely decorative, hints that the
       field is a time picker.  Sits inside the input's
       padding-right so the value text doesn't overlap. -->
  <svg
    xmlns="http://www.w3.org/2000/svg"
    class="w-4 h-4 text-surface-500 absolute right-3 top-1/2 -translate-y-1/2 pointer-events-none"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <circle cx="12" cy="12" r="9" />
    <path d="M12 7v5l3 2" />
  </svg>

  {#if open}
    <ul
      id={listId}
      bind:this={listEl}
      class="absolute z-50 mt-1 w-full max-h-60 overflow-y-auto rounded-md border border-surface-300 dark:border-surface-700 bg-surface-50 dark:bg-surface-900 shadow-lg"
      role="listbox"
    >
      {#each slots as slot (slot)}
        <li
          role="option"
          aria-selected={slot === value}
          data-slot={slot}
          class="px-3 py-1.5 text-sm cursor-pointer {slot === value
            ? 'bg-primary-500 text-white'
            : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
          onmousedown={(e) => {
            e.preventDefault()
            pick(slot)
          }}
        >
          {slot}
        </li>
      {/each}
    </ul>
  {/if}
</div>
