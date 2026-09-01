<script lang="ts">
  /**
   * TimeField — split hour / minute dropdown time picker (#126).
   *
   * Replaces the native `<input type="time">` for consistency
   * with `DateField` (calendar-grid popover).  Click / focus
   * opens a popover with TWO scrollable columns side by side:
   * hours `00 … 23` on the left, minutes `00 … 59` on the
   * right.  Picking from one column updates the matching half
   * of the value and leaves the popover open so the user can
   * dial in the other half.  Clicking outside, hitting Escape,
   * or pressing Enter closes the popover.  The text input
   * itself stays editable for power users who'd rather type
   * "08:45" directly.
   *
   * Value is `HH:MM` so it round-trips through the existing
   * `fromLocalSplit` helper in EventEditor unchanged and slots
   * straight into TasksView's reminder serialiser.
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
  let hourListEl: HTMLUListElement | undefined = $state()
  let minuteListEl: HTMLUListElement | undefined = $state()
  // Stable id for the combobox / popover `aria-controls` link.
  const popoverId = `timefield-popover-${crypto.randomUUID()}`

  /** 24 entries — `'00'` … `'23'`.  Pre-computed once at module
   *  init so the popover render is just a string-list iteration. */
  const HOURS: string[] = (() => {
    const out: string[] = []
    for (let h = 0; h < 24; h++) out.push(String(h).padStart(2, '0'))
    return out
  })()

  /** 60 entries — `'00'` … `'59'`.  Same shape as `HOURS` so the
   *  two columns render with identical row geometry. */
  const MINUTES: string[] = (() => {
    const out: string[] = []
    for (let m = 0; m < 60; m++) out.push(String(m).padStart(2, '0'))
    return out
  })()

  /** Split the bound `value` into its two halves, defaulting
   *  unset / malformed input to `'00'` so the popover always has
   *  *something* highlighted as selected.  The empty-value case
   *  treats both halves as unset (we render no row as selected
   *  but still default-scroll to 00 on open). */
  function splitValue(v: string): { h: string; m: string } {
    if (!v) return { h: '00', m: '00' }
    const [rawH = '', rawM = ''] = v.split(':')
    const h = rawH.padStart(2, '0').slice(0, 2)
    const m = rawM.padStart(2, '0').slice(0, 2)
    return {
      h: /^\d{2}$/.test(h) ? h : '00',
      m: /^\d{2}$/.test(m) ? m : '00',
    }
  }

  /** `true` iff the input is truly empty — drives the "no row
   *  selected" highlight state so an opening popover doesn't
   *  visually pre-commit the user to 00:00. */
  const isUnset = $derived(value === '')
  const selectedH = $derived(splitValue(value).h)
  const selectedM = $derived(splitValue(value).m)

  function pickHour(h: string) {
    // Pulling from the *current* value (not the `selectedM`
    // derived) keeps the write atomic — `value = ...` triggers
    // exactly one bound-state assignment.  Default the minute
    // half to '00' the first time the user touches the picker
    // on a previously-empty field.
    const { m } = splitValue(value)
    value = `${h}:${m}`
  }
  function pickMinute(m: string) {
    const { h } = splitValue(value)
    value = `${h}:${m}`
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

  // Auto-scroll both columns to the currently-selected row when
  // the popover opens, so the user lands on something familiar
  // instead of always at 00.  Runs after `tick()` so the
  // `bind:this` refs are settled.
  $effect(() => {
    if (!open) return
    void tick().then(() => {
      const hEl = hourListEl?.querySelector(`[data-slot="${selectedH}"]`)
      if (hEl instanceof HTMLElement) hEl.scrollIntoView({ block: 'center' })
      const mEl = minuteListEl?.querySelector(`[data-slot="${selectedM}"]`)
      if (mEl instanceof HTMLElement) mEl.scrollIntoView({ block: 'center' })
    })
  })

  // Outside-click closes.  Same idiom as DateField / the broader
  // popover-dismissal convention from CLAUDE.md.
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
    role="combobox"
    aria-haspopup="dialog"
    aria-expanded={open}
    aria-controls={popoverId}
    autocomplete="off"
    class="input w-full px-3 py-2 text-sm rounded-lg pr-9"
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
    <div
      id={popoverId}
      class="absolute z-50 mt-1 popover-opaque rounded-xl p-2 flex gap-1"
      role="dialog"
      aria-label="Pick a time"
    >
      <!-- Hours column.  Fixed width so the two columns stay
           visually balanced; `min-w-0` on the lists keeps a long
           hover state from stretching the popover. -->
      <div class="flex flex-col items-stretch">
        <div class="text-[10px] uppercase tracking-wide text-surface-500 text-center pb-1">
          Hours
        </div>
        <ul
          bind:this={hourListEl}
          class="w-16 max-h-56 overflow-y-auto rounded-lg"
          role="listbox"
          aria-label="Hours"
        >
          {#each HOURS as h (h)}
            <li
              role="option"
              aria-selected={!isUnset && h === selectedH}
              data-slot={h}
              class="px-3 py-1 text-sm text-center cursor-pointer rounded-sm {!isUnset && h === selectedH
                ? 'bg-primary-500 text-white'
                : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
              onmousedown={(e) => {
                e.preventDefault()
                pickHour(h)
              }}
            >
              {h}
            </li>
          {/each}
        </ul>
      </div>
      <div class="flex flex-col items-stretch">
        <div class="text-[10px] uppercase tracking-wide text-surface-500 text-center pb-1">
          Min
        </div>
        <ul
          bind:this={minuteListEl}
          class="w-16 max-h-56 overflow-y-auto rounded-lg"
          role="listbox"
          aria-label="Minutes"
        >
          {#each MINUTES as m (m)}
            <li
              role="option"
              aria-selected={!isUnset && m === selectedM}
              data-slot={m}
              class="px-3 py-1 text-sm text-center cursor-pointer rounded-sm {!isUnset && m === selectedM
                ? 'bg-primary-500 text-white'
                : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
              onmousedown={(e) => {
                e.preventDefault()
                pickMinute(m)
              }}
            >
              {m}
            </li>
          {/each}
        </ul>
      </div>
    </div>
  {/if}
</div>
