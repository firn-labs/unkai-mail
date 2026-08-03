<script lang="ts">
  /**
   * DateField — calendar-popover date picker (#126).
   *
   * A reusable date input that replaces the native
   * `<input type="date">`.  The native control varies wildly
   * across browsers and platforms (Chromium on Linux ships a
   * particularly minimal one), so we render a custom popover
   * matching the standard calendar-grid date-picker style.
   * Locale-formatted
   * display, prev/next month arrows, current-day highlight,
   * outside-click + Escape to close, arrow-key navigation
   * inside the grid.
   *
   * Value is `YYYY-MM-DD` so it round-trips through the
   * existing date helpers in EventEditor unchanged.
   */

  import { onMount } from 'svelte'

  let {
    value = $bindable(''),
    id,
    ariaLabel,
  }: {
    value?: string
    id?: string
    ariaLabel?: string
  } = $props()

  // Popover open / close.  Closing snaps the focused-month
  // view back to the selected date so reopening doesn't keep
  // the user on a month they were just browsing.
  let open = $state(false)
  let anchor: HTMLDivElement | undefined = $state()

  /** Parse `YYYY-MM-DD` to a `Date` (local-zone calendar
   *  date).  Returns today on bad / empty input. */
  function parseDate(s: string): Date {
    if (!s) return new Date()
    const [y, m, d] = s.split('-').map((p) => parseInt(p, 10))
    if (!y || !m || !d) return new Date()
    return new Date(y, m - 1, d)
  }
  /** Format `Date` → `YYYY-MM-DD` (local-zone). */
  function formatISO(d: Date): string {
    const pad = (n: number) => String(n).padStart(2, '0')
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
  }
  /** Localised display: "Apr 28, 2026". */
  function formatDisplay(s: string): string {
    if (!s) return ''
    const d = parseDate(s)
    return d.toLocaleDateString(undefined, {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
    })
  }
  function sameDay(a: Date, b: Date): boolean {
    return (
      a.getFullYear() === b.getFullYear() &&
      a.getMonth() === b.getMonth() &&
      a.getDate() === b.getDate()
    )
  }

  /** Currently-displayed month — driven by the prev/next
   *  arrows.  Initialised to the value's month so the popover
   *  opens centred on the current selection. */
  // svelte-ignore state_referenced_locally
  let view = $state(monthStart(parseDate(value)))
  function monthStart(d: Date): Date {
    return new Date(d.getFullYear(), d.getMonth(), 1)
  }
  // Keep the view in sync when the bound value changes
  // externally (e.g. reset by the parent on form open).
  $effect(() => {
    if (!open) view = monthStart(parseDate(value))
  })

  /** 6-week grid of `Date`s for the current view month.
   *  First row may include trailing days of the previous
   *  month; last row may spill into the next.  Always 42
   *  cells so the popover height never jumps as the user
   *  navigates between 28-, 30-, and 31-day months. */
  let grid = $derived.by(() => {
    const first = new Date(view.getFullYear(), view.getMonth(), 1)
    // RFC: week starts on Monday for our locale; getDay() 0=Sun → shift.
    const offset = (first.getDay() + 6) % 7
    const start = new Date(first)
    start.setDate(1 - offset)
    const cells: Date[] = []
    for (let i = 0; i < 42; i++) {
      const d = new Date(start)
      d.setDate(start.getDate() + i)
      cells.push(d)
    }
    return cells
  })

  let today = new Date()
  let selected = $derived(parseDate(value))

  function pick(d: Date) {
    value = formatISO(d)
    open = false
  }
  function goToday() {
    pick(new Date())
  }

  // Outside-click + Escape close.  Bound at mount so we don't
  // leak listeners on hot-reload.
  onMount(() => {
    function onKey(e: KeyboardEvent) {
      if (!open) return
      if (e.key === 'Escape') {
        open = false
        return
      }
      // Day-by-day arrow nav only makes sense in days mode — in
      // months / years grids the user picks with the mouse.
      if (mode !== 'days') return
      if (e.key === 'ArrowLeft') {
        e.preventDefault()
        const d = new Date(selected)
        d.setDate(d.getDate() - 1)
        value = formatISO(d)
      } else if (e.key === 'ArrowRight') {
        e.preventDefault()
        const d = new Date(selected)
        d.setDate(d.getDate() + 1)
        value = formatISO(d)
      } else if (e.key === 'ArrowUp') {
        e.preventDefault()
        const d = new Date(selected)
        d.setDate(d.getDate() - 7)
        value = formatISO(d)
      } else if (e.key === 'ArrowDown') {
        e.preventDefault()
        const d = new Date(selected)
        d.setDate(d.getDate() + 7)
        value = formatISO(d)
      } else if (e.key === 'Enter') {
        open = false
      }
    }
    function onClick(e: MouseEvent) {
      if (!open || !anchor) return
      if (!anchor.contains(e.target as Node)) open = false
    }
    document.addEventListener('keydown', onKey)
    document.addEventListener('mousedown', onClick)
    return () => {
      document.removeEventListener('keydown', onKey)
      document.removeEventListener('mousedown', onClick)
    }
  })

  // Picker mode — clicking the month or year label in the header
  // drills up into a coarser grid (12-month picker / 12-year
  // picker), matching the standard calendar-popover UX.  Resets
  // to 'days' whenever the popover closes so reopening always
  // shows the day grid.
  type Mode = 'days' | 'months' | 'years'
  let mode = $state<Mode>('days')
  $effect(() => {
    if (!open) mode = 'days'
  })

  // Locale-aware labels.  Split into month-only and year-only so
  // the header can render them as two independently-clickable
  // buttons (drill into months grid / years grid respectively).
  let monthLabelOnly = $derived(
    view.toLocaleDateString(undefined, { month: 'long' }),
  )
  let yearLabelOnly = $derived(view.getFullYear())

  // 12-cell month grid.  Labels respect the user's locale via
  // toLocaleDateString — same approach we use for weekdays.
  let monthCells = $derived.by(() => {
    const out: { idx: number; label: string }[] = []
    for (let m = 0; m < 12; m++) {
      const ref = new Date(view.getFullYear(), m, 1)
      out.push({
        idx: m,
        label: ref.toLocaleDateString(undefined, { month: 'short' }),
      })
    }
    return out
  })

  // 12-year block — anchored on a multiple of 12 so the same
  // year always lives in the same cell when navigating between
  // adjacent decades.
  let yearBlockStart = $derived(Math.floor(view.getFullYear() / 12) * 12)
  let yearCells = $derived.by(() => {
    const out: number[] = []
    for (let i = 0; i < 12; i++) out.push(yearBlockStart + i)
    return out
  })

  function pickMonth(monthIdx: number) {
    view = new Date(view.getFullYear(), monthIdx, 1)
    mode = 'days'
  }
  function pickYear(year: number) {
    view = new Date(year, view.getMonth(), 1)
    mode = 'months'
  }
  /** Step the header back / forward.  In days mode that's one
   *  month, in months mode one year, in years mode one 12-year
   *  block — matching what the user is currently looking at. */
  function stepHeader(direction: -1 | 1) {
    if (mode === 'days') {
      view = new Date(view.getFullYear(), view.getMonth() + direction, 1)
    } else if (mode === 'months') {
      view = new Date(view.getFullYear() + direction, view.getMonth(), 1)
    } else {
      view = new Date(view.getFullYear() + direction * 12, view.getMonth(), 1)
    }
  }

  // Mon … Sun — generated from a fixed reference week so the
  // labels match whatever the browser's locale renders.
  let weekdayLabels = $derived.by(() => {
    // 2024-01-01 was a Monday.
    const ref = new Date(2024, 0, 1)
    const out: string[] = []
    for (let i = 0; i < 7; i++) {
      const d = new Date(ref)
      d.setDate(ref.getDate() + i)
      out.push(d.toLocaleDateString(undefined, { weekday: 'short' }))
    }
    return out
  })
</script>

<div class="relative" bind:this={anchor}>
  <button
    type="button"
    {id}
    aria-label={ariaLabel}
    aria-haspopup="dialog"
    aria-expanded={open}
    class="input w-full px-3 py-2 text-sm rounded-lg text-left flex items-center justify-between gap-2"
    onclick={() => (open = !open)}
  >
    <span class={value ? '' : 'text-surface-400'}>
      {value ? formatDisplay(value) : 'Pick a date'}
    </span>
    <svg
      xmlns="http://www.w3.org/2000/svg"
      class="w-4 h-4 text-surface-500 shrink-0"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <rect x="3" y="4" width="18" height="18" rx="2" />
      <path d="M16 2v4M8 2v4M3 10h18" />
    </svg>
  </button>

  {#if open}
    <div
      class="absolute z-50 mt-1 w-[280px] rounded-lg border border-surface-300 dark:border-surface-700 bg-surface-50 dark:bg-surface-900 shadow-lg p-3"
      role="dialog"
      aria-label="Pick a date"
    >
      <!-- Header: prev | month / year label(s) | next.  The
           label area renders one or two clickable buttons that
           drill into the months / years grids — same UX as
           Material's date picker.  In days mode both labels are
           clickable; in months mode only the year is; in years
           mode the label collapses to a static decade range. -->
      <div class="flex items-center justify-between mb-2">
        <button
          type="button"
          class="btn btn-sm preset-tonal-surface w-8 h-8 p-0"
          aria-label="Previous"
          onclick={() => stepHeader(-1)}
        >‹</button>
        <div class="flex items-center gap-1">
          {#if mode === 'days'}
            <button
              type="button"
              class="px-2 py-1 rounded-lg hover:bg-surface-200 dark:hover:bg-surface-800 text-sm font-medium"
              onclick={() => (mode = 'months')}
              aria-label="Pick month"
            >{monthLabelOnly}</button>
            <button
              type="button"
              class="px-2 py-1 rounded-lg hover:bg-surface-200 dark:hover:bg-surface-800 text-sm font-medium"
              onclick={() => (mode = 'years')}
              aria-label="Pick year"
            >{yearLabelOnly}</button>
          {:else if mode === 'months'}
            <button
              type="button"
              class="px-2 py-1 rounded-lg hover:bg-surface-200 dark:hover:bg-surface-800 text-sm font-medium"
              onclick={() => (mode = 'years')}
              aria-label="Pick year"
            >{yearLabelOnly}</button>
          {:else}
            <span class="px-2 py-1 text-sm font-medium">
              {yearBlockStart} – {yearBlockStart + 11}
            </span>
          {/if}
        </div>
        <button
          type="button"
          class="btn btn-sm preset-tonal-surface w-8 h-8 p-0"
          aria-label="Next"
          onclick={() => stepHeader(1)}
        >›</button>
      </div>

      {#if mode === 'days'}
        <!-- Weekday header row (Mon-first). -->
        <div class="grid grid-cols-7 gap-0.5 mb-1">
          {#each weekdayLabels as wd (wd)}
            <div class="text-[10px] uppercase tracking-wide text-surface-500 text-center py-1">
              {wd}
            </div>
          {/each}
        </div>

        <!-- 6×7 grid.  Out-of-month days stay clickable but
             dimmed so the user can drag selection across month
             boundaries without breaking flow. -->
        <div class="grid grid-cols-7 gap-0.5">
          {#each grid as d (d.getTime())}
            {@const inMonth = d.getMonth() === view.getMonth()}
            {@const isToday = sameDay(d, today)}
            {@const isSelected = sameDay(d, selected)}
            <button
              type="button"
              class="text-sm h-8 rounded-lg flex items-center justify-center {isSelected
                ? 'bg-primary-500 text-white font-semibold'
                : isToday
                  ? 'border border-primary-500 text-primary-500'
                  : inMonth
                    ? 'hover:bg-surface-200 dark:hover:bg-surface-800'
                    : 'text-surface-400 hover:bg-surface-200 dark:hover:bg-surface-800'}"
              onclick={() => pick(d)}
            >
              {d.getDate()}
            </button>
          {/each}
        </div>
      {:else if mode === 'months'}
        <!-- 4×3 month grid.  Selected highlight only fires when
             the displayed year matches the selected date's year,
             so jumping into a different year shows a clean grid. -->
        <div class="grid grid-cols-3 gap-1">
          {#each monthCells as cell (cell.idx)}
            {@const isToday =
              cell.idx === today.getMonth() &&
              view.getFullYear() === today.getFullYear()}
            {@const isSelected =
              cell.idx === selected.getMonth() &&
              view.getFullYear() === selected.getFullYear()}
            <button
              type="button"
              class="text-sm h-10 rounded-lg flex items-center justify-center {isSelected
                ? 'bg-primary-500 text-white font-semibold'
                : isToday
                  ? 'border border-primary-500 text-primary-500'
                  : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
              onclick={() => pickMonth(cell.idx)}
            >
              {cell.label}
            </button>
          {/each}
        </div>
      {:else}
        <!-- 4×3 year grid spanning the current 12-year block. -->
        <div class="grid grid-cols-3 gap-1">
          {#each yearCells as y (y)}
            {@const isToday = y === today.getFullYear()}
            {@const isSelected = y === selected.getFullYear()}
            <button
              type="button"
              class="text-sm h-10 rounded-lg flex items-center justify-center {isSelected
                ? 'bg-primary-500 text-white font-semibold'
                : isToday
                  ? 'border border-primary-500 text-primary-500'
                  : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
              onclick={() => pickYear(y)}
            >
              {y}
            </button>
          {/each}
        </div>
      {/if}

      <!-- Footer: Today shortcut.  Clear button is
           deliberately omitted — the field is required for
           events, so giving the user a button to wipe it would
           just produce form-validation errors on save. -->
      <div class="flex justify-end mt-2">
        <button
          type="button"
          class="btn btn-sm preset-tonal-primary"
          onclick={goToday}
        >Today</button>
      </div>
    </div>
  {/if}
</div>
